//! Upload buffers a list copied through, kept by size class for the next
//! copy ([`crate::host::HostList::upload`]).
//!
//! Every copy between title memory and a host copy, and every scratch
//! buffer too big for a list's chunk, made its own `UPLOAD` buffer and let
//! it go when its list was reset: a `CreateCommittedResource`, a map and a
//! free of the driver's allocation each time (the comment at
//! [`crate::host::HostList::upload`]: "one buffer per copy; pool them once
//! uploads are frequent"). The copies a frame makes repeat a handful of
//! sizes, so a buffer of the size a copy asks for is almost always kept
//! already.
//!
//! Sizes are rounded up to half-step classes (1x / 1.5x of powers of two
//! from 64 KiB to 32 MiB): a 300 KiB copy takes a 384 KiB buffer, and the
//! worst waste is under 1.5x. Buffers over 32 MiB are one-offs and are not
//! pooled.
//!
//! The pool never sees a buffer the card may still read: a list hands its
//! uploads to `crate::host::retire`, which keeps them until every
//! submission in the air has finished, and only their `Drop` puts them
//! back here (the candidate's `upload_pool.rs` has the same rule).
//! `VICTORY_UPLOAD_POOL=0` makes every copy make its own buffer, as before;
//! `VICTORY_UPLOAD_POOL_MB=<n>` sets the bytes kept (192 by default).

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// The most bytes kept at once when `VICTORY_UPLOAD_POOL_MB` is not set.
const DEFAULT_POOL_MB: u64 = 192;

/// Buffers larger than this are one-offs and are not pooled.
const MAX_POOLED_BYTES: u64 = 32 * MIB;

/// The smallest class: the smallest `UPLOAD` heap D3D12 makes.
const MIN_CLASS: u64 = 64 * KIB;

/// The next class above `class`: the half-step above a power of two, else
/// the next power of two (64K, 96K, 128K, 192K, 256K, ...).
fn next_class(class: u64) -> u64 {
    if class.is_power_of_two() {
        class + class / 2
    } else {
        class.next_power_of_two()
    }
}

/// The size class `bytes` belongs to, or `None` for a buffer that is not
/// pooled (more than [`MAX_POOLED_BYTES`]). Anything smaller than the
/// smallest class takes the smallest class.
pub(crate) fn class_for(bytes: u64) -> Option<u64> {
    if bytes > MAX_POOLED_BYTES {
        return None;
    }
    let mut class = MIN_CLASS;
    while class < bytes {
        class = next_class(class);
    }
    Some(class.min(MAX_POOLED_BYTES))
}

/// A buffer nothing holds now: its resource, the CPU pointer and the GPU
/// address it was mapped at, and its class size.
struct Kept {
    resource: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    bytes: u64,
}

// SAFETY: a D3D12 upload buffer (free-threaded) and the persistent mapping
// and address it was made with, moved between threads and dropped once.
unsafe impl Send for Kept {}

/// An upload buffer a list holds until it is reset: its resource (nothing
/// else keeps it alive), the CPU pointer and GPU address of its mapping,
/// and its class size. Dropped, it goes back to the pool.
pub(crate) struct Entry {
    kept: Option<Kept>,
}

// SAFETY: see `Kept`.
unsafe impl Send for Entry {}

impl Entry {
    /// The buffer itself, for the commands that read it and the record the
    /// list keeps of what it holds.
    pub(crate) fn resource(&self) -> ID3D12Resource {
        // SAFETY: `kept` is `Some` until this entry is dropped.
        self.kept.as_ref().unwrap().resource.clone()
    }

    /// The persistent CPU pointer of the buffer's mapping.
    pub(crate) fn data(&self) -> *mut u8 {
        self.kept.as_ref().unwrap().data
    }

    /// The GPU address the buffer is mapped at.
    pub(crate) fn gpu(&self) -> u64 {
        self.kept.as_ref().unwrap().gpu
    }

    /// The bytes of the buffer (its class size).
    pub(crate) fn bytes(&self) -> u64 {
        self.kept.as_ref().unwrap().bytes
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(kept) = self.kept.take() {
            keep(kept);
        }
    }
}

/// Upload buffers nothing holds now, one list per class.
struct Pool {
    /// Kept buffers, by class index (`free[i]` holds `class_of(i)` bytes).
    free: Vec<Vec<Kept>>,
    bytes: u64,
    takes: u64,
    hits: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    free: Vec::new(),
    bytes: 0,
    takes: 0,
    hits: 0,
});

/// The class index of `class` (dense, ascending from the smallest).
fn index_of(class: u64) -> usize {
    let mut at = 0;
    let mut size = MIN_CLASS;
    while size < class {
        size = next_class(size);
        at += 1;
    }
    at
}

/// Whether kept upload buffers are reused (`VICTORY_UPLOAD_POOL=0` off).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPLOAD_POOL").is_none_or(|v| v != "0"))
}

/// The bytes kept at once (`VICTORY_UPLOAD_POOL_MB`).
pub(crate) fn budget() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_UPLOAD_POOL_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_POOL_MB)
            .saturating_mul(MIB)
    })
}

/// An upload buffer of `bytes` (rounded up to its class): one kept, or one
/// `make` makes with its class size (the resource, its CPU pointer and its
/// GPU address).
pub(crate) fn take<F>(bytes: u64, make: F) -> Option<Entry>
where
    F: FnOnce(u64) -> Option<(ID3D12Resource, *mut u8, u64)>,
{
    let Some(class) = class_for(bytes) else {
        let (resource, data, gpu) = make(bytes)?;
        return Some(Entry {
            kept: Some(Kept {
                resource,
                data,
                gpu,
                bytes,
            }),
        });
    };
    if enabled() {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        pool.takes += 1;
        let at = index_of(class);
        if pool.free.len() > at
            && let Some(kept) = pool.free[at].pop()
        {
            pool.bytes = pool.bytes.saturating_sub(class);
            pool.hits += 1;
            return Some(Entry { kept: Some(kept) });
        }
    }
    let (resource, data, gpu) = make(class)?;
    Some(Entry {
        kept: Some(Kept {
            resource,
            data,
            gpu,
            bytes: class,
        }),
    })
}

/// Keeps `kept` for the next copy of its class, or lets it go past
/// [`budget`] (or with the pool off, or for a size that is not pooled).
fn keep(kept: Kept) {
    let bytes = kept.bytes;
    if !enabled() || class_for(bytes) != Some(bytes) {
        return;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.bytes + bytes > budget() {
        return;
    }
    let at = index_of(bytes);
    if pool.free.len() <= at {
        pool.free.resize_with(at + 1, Vec::new);
    }
    pool.bytes += bytes;
    pool.free[at].push(kept);
}

/// `(takes, hits, bytes kept)`: how often a copy found a buffer kept, and
/// what is kept now.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stats() -> (u64, u64, u64) {
    let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    (pool.takes, pool.hits, pool.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classes go up by half steps and never fall below the smallest.
    #[test]
    fn sizes_round_up_to_half_step_classes() {
        assert_eq!(class_for(1), Some(64 * KIB));
        assert_eq!(class_for(64 * KIB), Some(64 * KIB));
        assert_eq!(class_for(64 * KIB + 1), Some(96 * KIB));
        assert_eq!(class_for(96 * KIB), Some(96 * KIB));
        assert_eq!(class_for(300 * KIB), Some(384 * KIB));
        assert_eq!(class_for(384 * KIB), Some(384 * KIB));
        assert_eq!(class_for(MIB), Some(MIB));
        assert_eq!(class_for(1536 * KIB), Some(1536 * KIB));
        assert_eq!(class_for(MAX_POOLED_BYTES), Some(MAX_POOLED_BYTES));
        // One-offs are not pooled.
        assert_eq!(class_for(MAX_POOLED_BYTES + 1), None);
    }

    /// The waste of a class is under 1.5x of what was asked for (below the
    /// smallest class, every copy takes the smallest class).
    #[test]
    fn no_class_wastes_more_than_half_again() {
        let mut asked = 1;
        while asked <= MAX_POOLED_BYTES {
            let class = class_for(asked).unwrap();
            assert!(class >= asked, "{asked} took {class}");
            assert!(class >= MIN_CLASS, "{asked} took {class}");
            assert!(class * 2 <= asked.max(MIN_CLASS) * 3, "{asked} took {class}");
            assert_eq!(class % (16 * KIB), 0, "{asked} took {class}");
            asked = asked * 3 / 2 + 1;
        }
    }

    /// Each class has its own list, in ascending order, and asking for the
    /// class itself gives its own list (a buffer kept for a 300 KiB copy is
    /// found again by the next 300 KiB copy).
    #[test]
    fn class_indexes_are_dense_and_ascending() {
        let mut last = None;
        let mut asked = 1;
        let mut expected = 0;
        while asked <= MAX_POOLED_BYTES {
            let class = class_for(asked).unwrap();
            if Some(class) != last {
                assert_eq!(index_of(class), expected, "{asked} took {class}");
                assert_eq!(class_for(class), Some(class));
                expected += 1;
                last = Some(class);
            }
            asked += 1;
        }
        assert!(expected >= 19);
    }
}

#[cfg(test)]
mod device_tests {
    //! Reuse and what it costs, on whatever adapter the run asks for
    //! (`crate::test_device`): the bytes are never read by the card.

    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12Device,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
    use windows::core::Interface;

    /// An upload buffer as `Host::upload_buffer` makes one: `UPLOAD` heap,
    /// row major, mapped for its whole life.
    fn make(device: &ID3D12Device, size: u64) -> Option<(ID3D12Resource, *mut u8, u64)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
            Alignment: 0,
            Format: DXGI_FORMAT_UNKNOWN,
        };
        let mut resource: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut resource,
                )
                .ok()?;
            let resource = resource?;
            resource.Map(0, None, Some(&mut data)).ok()?;
            let gpu = resource.GetGPUVirtualAddress();
            Some((resource, data.cast(), gpu))
        }
    }

    /// A pool of one process, emptied of what an earlier test kept, so the
    /// counts it makes are its own.
    fn clear() {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        pool.free.clear();
        pool.bytes = 0;
        pool.takes = 0;
        pool.hits = 0;
    }

    /// A buffer a list let go of is the one the next copy of its class
    /// gets: the same resource, not a new allocation. (Without the pool,
    /// `take` makes a `CreateCommittedResource` per call and the second
    /// buffer is a different one.)
    #[test]
    fn a_buffer_a_list_let_go_is_the_one_the_next_copy_gets() {
        let _warp = crate::warptest::lock();
        let Some(device) = crate::test_device::device(D3D_FEATURE_LEVEL_12_0) else {
            eprintln!("skipped: no device: a_buffer_a_list_let_go_is_the_one_the_next_copy_gets");
            return;
        };
        if !enabled() {
            eprintln!("skipped: VICTORY_UPLOAD_POOL=0");
            return;
        }
        clear();
        let (before, _, _) = stats();
        let first = take(300 * KIB, |class| make(&device, class)).expect("an upload buffer");
        let raw = first.resource().clone();
        let bytes = first.bytes();
        assert_eq!(bytes, 384 * KIB, "a 300 KiB copy takes its class");
        drop(first);
        let second = take(300 * KIB, |class| make(&device, class)).expect("an upload buffer");
        let again = second.resource();
        assert_eq!(
            again.as_raw(),
            raw.as_raw(),
            "the buffer the first copy let go of is not the one the second got"
        );
        let (takes, hits, _) = stats();
        assert_eq!(takes - before, 2, "two copies asked for a buffer");
        assert!(hits >= 1, "the second copy found no buffer kept");
        drop(second);
        // The one buffer is kept again: the second copy reused the first's,
        // so nothing new was made and nothing is wasted.
        let (_, _, kept) = stats();
        assert_eq!(kept, 384 * KIB, "the buffer a list let go was not kept");
        clear();
    }

    /// Microbenchmark (`#[ignore]`d): what a frame's copies cost with the
    /// pool and without, over the sizes copies repeat. Run it with
    /// `--ignored --nocapture upload_pool_hit_rate`.
    #[test]
    #[ignore]
    fn upload_pool_hit_rate() {
        let _warp = crate::warptest::lock();
        let Some(device) = crate::test_device::device(D3D_FEATURE_LEVEL_12_0) else {
            eprintln!("skipped: no device: upload_pool_hit_rate");
            return;
        };
        // The sizes a frame's copies take: constants, a draw's corners, and
        // the read-backs of textures, 64 KiB to 4 MiB, repeated as they are
        // in the world.
        let sizes: [u64; 7] = [256, 4 * KIB, 64 * KIB, 96 * KIB, 300 * KIB, MIB, 3 * MIB];
        for &rounds in &[64u64, 1024] {
            clear();
            let mut created = 0u64;
            let started = std::time::Instant::now();
            for i in 0..rounds {
                let size = sizes[(i as usize) % sizes.len()];
                let entry = take(size, |class| {
                    created += 1;
                    make(&device, class)
                })
                .expect("an upload buffer");
                // A list's hold: the buffer comes back when it is dropped.
                drop(entry);
            }
            let elapsed = started.elapsed();
            let (takes, hits, kept) = stats();
            eprintln!(
                "copies {rounds}: {:.1} us each, {hits} of {takes} from the pool ({}%), \
                 {created} created, {} KiB kept",
                elapsed.as_secs_f64() * 1e6 / rounds as f64,
                hits.checked_mul(100).and_then(|h| h.checked_div(takes)).unwrap_or(0),
                kept / KIB,
            );
        }
        // Before the port: a buffer made (and freed) for every copy.
        clear();
        let started = std::time::Instant::now();
        for i in 0..1024u64 {
            let size = sizes[(i as usize) % sizes.len()];
            let class = class_for(size).expect("a class");
            let _ = make(&device, class).expect("an upload buffer");
        }
        eprintln!(
            "copies 1024 with no pool: {:.1} us each (a buffer made per copy)",
            started.elapsed().as_secs_f64() * 1e6 / 1024.0,
        );
        clear();
    }
}
