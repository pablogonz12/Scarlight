//! Residents: title memory the card only reads, handed to it from an arena.
//!
//! A resident is a title range the submission's shaders see at its own
//! address, with no copy between them and no upload before it: the buffer is
//! the arena's placement over the title's pages (`crate::arena`), so what the
//! card reads *is* the title's memory. Three things make that admissible:
//!
//! - the range lies in an open arena part, which was proven **both ways** by
//!   the arena's canary before the part was handed out (`arena::prove`), and
//!   is proven once more here for the range itself ([`admit`]: a sentinel
//!   written at the title's address must come back through the part's
//!   buffer, and the title's bytes are put back);
//! - the card cannot write it: only a buffer created without
//!   `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` is admitted, so D3D12 can
//!   give the shaders no UAV over it (`resource.rs` tells
//!   [`crate::shadow::register`] which it is);
//! - no submission is in the air ([`crate::host::late`]): a placement is
//!   only made while every submission has been waited for, which is also
//!   what makes [`admit`]'s sentinel a safe thing to write.
//!
//! So for exactly those shadows the upload ([`crate::shadow::Shadow::update`]),
//! the copy-back ([`crate::shadow::Shadow::copy_back`]) and the write-watch
//! asking are all gone, and the copy itself is never made. A region is
//! dropped when the runtime says the pages show something else (a remap, a
//! free, a protection change: [`crate::shadow::in_place_let_go`]), and the
//! next ask over it resolves and proves again, or is a copy.
//!
//! What it is *not*: the fork's `VICTORY_INPLACE=4` (`notes/resident-lane.md`),
//! which keeps a resident in the *card's* memory and merges it back into the
//! arena every submission. That needs `R`/`B`, an upload, the merge, declared
//! writes, cross-marks and slot retargeting; none of them is here, and none
//! of them is needed to remove the host copy of a range the card only reads.
//! `notes/resident-on-arena.md` has the item-by-item comparison.
//!
//! `VICTORY_RESIDENT=read` turns it on; only that exact value, and off by
//! default: a switch that puts the title's memory behind the card must not be
//! reached by a typo.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource};

use crate::inplace::{Placed, Rig};

/// The bytes [`admit`] moves: the title's first cache line and a half, far
/// past the first 16 bytes a copy would have a chance to fake.
const SAMPLE: usize = 64;

/// Whether the resident class is on: `VICTORY_RESIDENT=read`. Read once;
/// off, every entry point is a `None` and every counter stays zero.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var("VICTORY_RESIDENT").ok().as_deref()))
}

/// [`on`]'s answer for a value of `VICTORY_RESIDENT`. Only `read` is on: the
/// class places ranges the card cannot write, and a value that named
/// something else (`1`, `write`, a stray space) must leave every range a
/// copy rather than place one on a hope.
fn switch(value: Option<&str>) -> bool {
    value == Some("read")
}

/// Why a range was not made resident, in the order they are asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The buffer can be written by shaders (it has a UAV): not this class.
    Writable = 0,
    /// A submission had not been waited for: nothing may be placed, and no
    /// sentinel may be written.
    Late,
    /// No part of the arena holds the range (not reported committed, over
    /// the budget, or the host refused, counted by the arena).
    NoPart,
    /// The part ends before the range does: the range would run past the
    /// buffer it was placed in.
    Short,
    /// No device to prove it with, or the range is shorter than [`SAMPLE`].
    Unprovable,
    /// The sentinel written at the title's address did not come back
    /// through the part's buffer: the import was refused and the host used
    /// memory of its own (vkd3d-proton#3522).
    Canary,
    /// The part's bytes cannot be mapped for the proof.
    Rig,
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
const WHY_NAMES: [&str; 7] = [
    "shaders may write it",
    "a submission had not been waited for",
    "no arena part holds it",
    "the part ends before the range does",
    "nothing could prove it",
    "the card does not show the title's bytes",
    "no rig for the proof",
];

/// Ranges turned down since the start, by [`Why`].
static TURNED: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// Regions resident now, and their bytes.
static HELD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Refresh steps not copied (their count and bytes), and copy-backs skipped
/// (their count and bytes), since the last frame line.
static SAVED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Regions dropped because their pages show something else, and writes the
/// card declared into a resident range (which the class says cannot happen:
/// counted so that it would be seen).
static DROPPED: AtomicU64 = AtomicU64::new(0);
static DECLARED_WRITES: AtomicU64 = AtomicU64::new(0);

/// The resident ranges by their first page: bytes. A range is here while a
/// shadow over it is placed; the shadow's drop lets it go.
#[derive(Default)]
struct Registry {
    regions: BTreeMap<u64, u64>,
}

impl Registry {
    /// `[at, at + len)` is resident.
    fn hold(&mut self, at: u64, len: u64) {
        self.regions.insert(at, len);
    }

    /// `[at, at + len)` shows something else now: every region over it goes
    /// (part of a region is not enough: the pages it was proven over are not
    /// the pages it was proven over any more). Returns how many went.
    fn changed(&mut self, at: u64, len: u64) -> u64 {
        let end = at.saturating_add(len.max(1));
        let gone: Vec<u64> = self.regions.range(..end).filter(|&(&b, &n)| b + n > at).map(|(&b, _)| b).collect();
        for base in &gone {
            self.regions.remove(base);
        }
        gone.len() as u64
    }

    /// Regions in it and their bytes. Only the tests ask (CodeRabbit, PR
    /// #273): compiled for them alone.
    #[cfg(test)]
    fn census(&self) -> (u64, u64) {
        (self.regions.len() as u64, self.regions.values().copied().sum())
    }
}

static REGIONS: Mutex<Registry> = Mutex::new(Registry { regions: BTreeMap::new() });

fn with<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// A buffer of title memory the card only reads, placed in an arena part:
/// `None` leaves the caller to copy it, as without the class.
///
/// `read_only` is the buffer's own fact (`D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS`
/// was not set), not an assumption about how the title uses it.
pub(crate) fn place(at: u64, len: u64, read_only: bool) -> Option<Placed> {
    if !on() {
        return None;
    }
    let why = match try_place(at, len, read_only) {
        Ok(placed) => return Some(placed),
        Err(why) => why,
    };
    TURNED[why as usize].fetch_add(1, Ordering::Relaxed);
    None
}

/// [`place`], with the reason it did not happen.
fn try_place(at: u64, len: u64, read_only: bool) -> Result<Placed, Why> {
    if !read_only {
        return Err(Why::Writable);
    }
    // Nothing may be placed, and no sentinel written into the title's bytes,
    // while a submission that reads them is in the air.
    if crate::host::late() {
        return Err(Why::Late);
    }
    let host = crate::host::host().ok_or(Why::Unprovable)?;
    let resolved = crate::arena::resolve(at, len).ok_or(Why::NoPart)?;
    // SAFETY: a live buffer; the description is its own.
    if unsafe { resolved.resource.GetDesc() }.Width < resolved.offset + len {
        return Err(Why::Short);
    }
    if len < SAMPLE as u64 {
        return Err(Why::Unprovable);
    }
    if !admit(&host.device, &resolved.resource, resolved.offset, at)? {
        return Err(Why::Canary);
    }
    with(|regions| regions.hold(at, len));
    HELD[0].fetch_add(1, Ordering::Relaxed);
    HELD[1].fetch_add(len, Ordering::Relaxed);
    Ok(Placed {
        buffer: resolved.resource,
        heap: resolved.heap,
        gpu: resolved.gpu,
        bias: resolved.offset,
    })
}

/// Proves that the title's bytes at `at` are what the card reads through
/// `resource` at `offset`: a sentinel written at the title's address after
/// the import, read back through the part's buffer. The title's bytes go
/// back either way (the copy only reads, so putting them back cannot race
/// with it).
///
/// `Err(Why::Canary)` is the hole of vkd3d-proton#3522 made visible: the
/// heap and the buffer exist, every draw runs, and the buffer is memory of
/// the host's own. Nothing may be placed over it.
fn admit(device: &ID3D12Device, resource: &ID3D12Resource, offset: u64, at: u64) -> Result<bool, Why> {
    if SAMPLE as u64 > unsafe { resource.GetDesc() }.Width.saturating_sub(offset) {
        return Err(Why::Unprovable);
    }
    let mut their = [0u8; SAMPLE];
    let mut sent = [0u8; SAMPLE];
    sentinel(&mut sent, (at & 0xff) as u8);
    // SAFETY: `at` is the caller's committed, read-write title memory, and
    // the sample lies inside it (the caller asks for whole ranges of it).
    unsafe {
        std::ptr::copy_nonoverlapping(at as *const u8, their.as_mut_ptr(), SAMPLE);
        std::ptr::copy_nonoverlapping(sent.as_ptr(), at as *mut u8, SAMPLE);
    }
    // A fetch that fails after its copy was submitted leaves the copy the
    // card's: the rig holds the allocator the list was recorded in, which must
    // live until the card finishes with it (`arena.rs` keeps its own the same
    // way, CodeRabbit PR #273), and `fetch` keeps the readback buffer.
    let read = match Rig::new(device) {
        Ok(mut rig) => match rig.fetch(resource, offset, SAMPLE) {
            Ok(read) => Ok(read),
            Err(_) => {
                std::mem::forget(rig);
                Err(Why::Canary)
            }
        },
        Err(_) => Err(Why::Rig),
    };
    // SAFETY: as above: the title's bytes go back the way they came.
    unsafe { std::ptr::copy_nonoverlapping(their.as_ptr(), at as *mut u8, SAMPLE) };
    Ok(read? == sent)
}

/// [`SAMPLE`] bytes that differ from seed to seed.
fn sentinel(bytes: &mut [u8; SAMPLE], seed: u8) {
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(seed) | 1;
    }
}

/// A refresh of `len` bytes of a resident shadow was not copied: the upload
/// a copy would have done. Called once per refresh step, so the bytes are
/// what the step covered, not what a copy would have found changed (it
/// copies only the pages the title wrote: this is an upper bound).
pub(crate) fn refreshed(len: u64) {
    SAVED[0].fetch_add(1, Ordering::Relaxed);
    SAVED[1].fetch_add(len, Ordering::Relaxed);
}

/// A copy-back reached a resident shadow and did nothing: the card writes
/// the title's memory itself. Should be zero: a resident buffer has no UAV,
/// and level 2's `Shadow::gpu_writes` returns before a step is deferred.
pub(crate) fn copy_back_skipped(len: u64) {
    SAVED[2].fetch_add(1, Ordering::Relaxed);
    SAVED[3].fetch_add(len, Ordering::Relaxed);
}

/// The resident shadow for `[at, at + len)` let go: its region is no longer
/// held (the pages stay locked while a list or a slot still holds the
/// buffer, which is what the shadow's own references count).
pub(crate) fn gone(at: u64, len: u64) {
    HELD[0].fetch_sub(1, Ordering::Relaxed);
    HELD[1].fetch_sub(len, Ordering::Relaxed);
    with(|regions| {
        regions.regions.remove(&at);
    });
}

/// `[at, at + len)` shows something else now (a remap, a free, a protection
/// change): every resident region over it is dropped, and the next ask over
/// it proves its part again or is a copy.
pub(crate) fn changed(at: u64, len: u64) -> u64 {
    let gone = with(|regions| regions.changed(at, len));
    DROPPED.fetch_add(gone, Ordering::Relaxed);
    gone
}

/// A resident shadow was told its shaders write `[at, at + len)`. The class
/// only admits buffers without a UAV, so this is a finding: the range is
/// dropped (no ask after this one places it again without a fresh proof)
/// and counted, and the write goes to the title's memory as level 2 does.
pub(crate) fn wrote(at: u64, len: u64) {
    DECLARED_WRITES.fetch_add(1, Ordering::Relaxed);
    changed(at, len);
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
fn count(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
fn per_frame(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed)
}

/// Resident regions and their bytes now, and the copies saved since the last
/// frame line (refresh steps and bytes, copy-backs and bytes).
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn frame() -> [u64; 4] {
    [
        per_frame(&SAVED[0]),
        per_frame(&SAVED[1]),
        per_frame(&SAVED[2]),
        per_frame(&SAVED[3]),
    ]
}

/// [`HELD`] now: regions and bytes.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn held() -> (u64, u64) {
    (count(&HELD[0]), count(&HELD[1]))
}

/// The line the frame log prints: the class, what it holds, and the copies
/// a frame did not make. Empty while the class is off.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn line() -> Option<String> {
    if !on() {
        return None;
    }
    let saved = frame();
    let (regions, bytes) = held();
    let turned: Vec<String> = WHY_NAMES
        .iter()
        .zip(&TURNED)
        .filter_map(|(name, n)| match count(n) {
            0 => None,
            n => Some(format!("{n} {name}")),
        })
        .collect();
    let turned = if turned.is_empty() { "none".to_owned() } else { turned.join(", ") };
    Some(format!(
        "resident read: {regions} regions {:.1} MB held; per frame {} refreshes {:.1} MB not copied, \
         {} copy-backs {:.1} MB skipped; {} dropped, {} writes declared into a resident, turned down: {turned}",
        bytes as f64 / 1e6,
        saved[0],
        saved[1] as f64 / 1e6,
        saved[2],
        saved[3] as f64 / 1e6,
        count(&DROPPED),
        count(&DECLARED_WRITES),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON, ID3D12Device13,
    };
    use windows::core::Interface;
    use windows_sys::Win32::System::Memory as mem;

    fn device() -> Option<ID3D12Device> {
        crate::test_device::device(D3D_FEATURE_LEVEL_12_0)
    }

    /// Only a plain `read` turns the class on: a typo must leave every range
    /// a copy.
    #[test]
    fn only_read_turns_the_class_on() {
        assert!(switch(Some("read")));
        for value in [None, Some(""), Some("0"), Some("1"), Some(" read "), Some("read "), Some("Read"), Some("write"), Some("on")] {
            assert!(!switch(value), "{value:?} must not place a resident");
        }
        // The frame line's own line is only printed while the class is on
        // (the switch is read once, and the tests do not set it).
        if !on() {
            assert!(line().is_none());
        }
    }

    /// Synthetic regions, no device: a region is held whole, a range that
    /// overlaps it drops it whole, and a range that misses it leaves it.
    #[test]
    fn a_change_drops_exactly_the_regions_it_touches() {
        let mut regions = Registry::default();
        regions.hold(0x1000_0000, 1 << 20);
        regions.hold(0x1010_0000, 2 << 20);
        regions.hold(0x1030_0000, 4 << 20);
        assert_eq!(regions.census(), (3, 7 << 20));
        // A range at one region's first byte takes that one and nothing else.
        assert_eq!(regions.changed(0x1010_0000, 0x1000), 1);
        // The 2 MiB one went; the 1 MiB and the 4 MiB ones stay.
        assert_eq!(regions.census(), (2, 5 << 20));
        // A range that just misses every region takes none (the region that
        // ends where the range begins does not hold a byte of it).
        assert_eq!(regions.changed(0x1020_0000, 0x1000), 0);
        assert_eq!(regions.changed(0x10FF_0000, 0x1000), 0);
        // A range that touches one region at its last byte takes it whole.
        assert_eq!(regions.changed(0x103F_FFFF, 1), 1);
        assert_eq!(regions.census(), (1, 1 << 20));
        // And the last one goes the same way.
        assert_eq!(regions.changed(0x1000_0000, 1), 1);
        assert_eq!(regions.census(), (0, 0));
        // The same start holds its own length; a second hold replaces it.
        let mut one = Registry::default();
        one.hold(0x4000_0000, 1 << 20);
        one.hold(0x4000_0000, 2 << 20);
        assert_eq!(one.census(), (1, 2 << 20));
        assert_eq!(one.changed(0x4000_0000, 1 << 20), 1);
    }

    /// The title writes its memory after the import and the card must read
    /// those bytes: it is what admits a placement, and it is what says a
    /// plain rewrite is no reason to drop the region. Then the runtime says
    /// the pages show something else, and the region goes (the next ask
    /// proves the part again or is a copy).
    #[test]
    fn warp_a_title_write_after_import_reaches_the_card() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let bytes = 4 * 64 * 1024u64;
            let base = mem::VirtualAlloc(std::ptr::null(), bytes as usize, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            let at = base as u64;
            std::ptr::write_bytes(base, 0x11, bytes as usize);
            // The part is opened and proven both ways by the arena's canary.
            let mut state = crate::arena::State::default();
            state.committed(at, bytes);
            let resolved = state
                .resolve(&device, at + 0x200, 0x1000, u64::MAX)
                .unwrap_or_else(|why| panic!("not in place: {why:?}"));
            // The title's CPU thread writes after the import: the card must see it.
            let fresh = [0x5eu8; SAMPLE];
            std::ptr::copy_nonoverlapping(fresh.as_ptr(), base.add(0x100), SAMPLE);
            let mut rig = Rig::new(&device).unwrap();
            let got = rig.fetch(&resolved.resource, resolved.offset - 0x100, SAMPLE).unwrap();
            assert_eq!(got, fresh, "a title write after the import is what the card reads");
            // And that is exactly the proof a placement runs for its own range.
            assert!(admit(&device, &resolved.resource, resolved.offset, at + 0x200).unwrap());
            // A rewrite is not a reason to drop: the mapping is the same and
            // the proof above holds. A remap/free is.
            let mut regions = Registry::default();
            regions.hold(at, bytes);
            assert_eq!(regions.census(), (1, bytes));
            assert_eq!(regions.changed(at + 2 * 64 * 1024, 64 * 1024), 1);
            assert_eq!(regions.census(), (0, 0));
            drop(resolved);
            state.released(at, bytes);
            assert_ne!(mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE), 0);
        }
    }

    /// A buffer the driver did not really import (memory of the card's own
    /// at the same offset) cannot pass for the title's: the sentinel written
    /// at the title's address never comes back through it, so no region is
    /// placed over it. This is vkd3d-proton#3522, refused.
    #[test]
    fn warp_the_proof_refuses_a_range_the_card_does_not_show() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        // SAFETY: allocations of the test's own.
        unsafe {
            let bytes = 64 * 1024usize;
            let base = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            std::ptr::write_bytes(base, 0x22, bytes);
            let mut rig = Rig::new(&device).unwrap();
            let card = rig.committed(D3D12_HEAP_TYPE_DEFAULT, bytes as u64, D3D12_RESOURCE_STATE_COMMON).unwrap();
            rig.store(&card, 0, &[0u8; SAMPLE]).unwrap();
            assert_eq!(
                admit(&device, &card, 0, base as u64),
                Ok(false),
                "memory of the card's own cannot prove for the title's"
            );
            // The title's bytes are what they were: the sentinel went back.
            assert_eq!(std::slice::from_raw_parts(base, SAMPLE), &[0x22u8; SAMPLE]);
            assert_ne!(mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE), 0);
        }
    }

    /// The copies a resident saves, and their milliseconds, on this card and
    /// not assumed: N read-only regions of 1 MiB, opened and proven as the
    /// shadow path opens them, then the upload a copy would pay per
    /// submission (title memory into a host copy) measured beside it.
    #[test]
    fn warp_resident_bench_copies() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        const N: usize = 8;
        let bytes = 1 << 20;
        let mut title = Vec::new();
        // SAFETY: allocations of the bench's own.
        unsafe {
            for _ in 0..N {
                let at = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
                assert!(!at.is_null());
                std::ptr::write_bytes(at, 0x33, bytes);
                title.push(at as u64);
            }
        }
        let mut state = crate::arena::State::default();
        let mut open_ms = 0.0f64;
        let mut resolved = Vec::new();
        for (i, &at) in title.iter().enumerate() {
            state.committed(at, bytes as u64);
            let asked = std::time::Instant::now();
            let part = state
                .resolve(&device, at, bytes as u64, u64::MAX)
                .unwrap_or_else(|why| panic!("region {i} not in place: {why:?}"));
            // As `place` does for each shadow over it: one read-direction proof.
            assert!(admit(&device, &part.resource, part.offset, at).unwrap());
            open_ms += asked.elapsed().as_secs_f64() * 1000.0;
            resolved.push(part);
        }
        // The upload a copy would pay per submission: title memory read into
        // a host copy. A resident pays none, and no copy exists to be made.
        let mut scratch = vec![0u8; bytes];
        let asked = std::time::Instant::now();
        for &at in &title {
            // SAFETY: the bench's own allocation, `bytes` long.
            unsafe { std::ptr::copy_nonoverlapping(at as *const u8, scratch.as_mut_ptr(), bytes) };
        }
        let copy_ms = asked.elapsed().as_secs_f64() * 1000.0 / N as f64;
        std::hint::black_box(&scratch);
        for &at in &title {
            refreshed(bytes as u64);
            let _ = at;
        }
        let saved = frame();
        assert_eq!(saved[0], N as u64, "one refresh not copied per region");
        assert_eq!(saved[1], N as u64 * bytes as u64);
        eprintln!(
            "resident bench: {N} read-only regions of {} KiB: {:.3} ms each to open a part and prove the range ({open_ms:.3} ms total); \
             the upload a copy would pay per submission (title memory into a host copy) is {copy_ms:.3} ms per {} KiB on this card, \
             so {N} regions = {} refreshes, {} KiB and about {:.3} ms a submission not copied, and no copy of them is made at all",
            bytes >> 10,
            open_ms / N as f64,
            bytes >> 10,
            saved[0],
            saved[1] >> 10,
            N as f64 * copy_ms
        );
        assert!(open_ms < 10_000.0, "opening {N} regions took {open_ms} ms");
        drop(resolved);
        for &at in &title {
            state.released(at, bytes as u64);
        }
        // SAFETY: the bench's own allocations, no part open over them.
        unsafe {
            for &at in &title {
                assert_ne!(mem::VirtualFree(at as *mut _, 0, mem::MEM_RELEASE), 0);
            }
        }
    }
}
