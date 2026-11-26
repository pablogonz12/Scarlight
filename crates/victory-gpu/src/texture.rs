//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); uploads cover each
//! compatible plane, mip and array slice (a depth-stencil texture's stencil
//! plane too, where the host's layout is the console's).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

pub(crate) mod ahead;

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Texture,
    /// The bytes of the texture each subresource (mip `+` slice times
    /// `mips`) covers, for detiling only the 64 KiB pages that changed
    /// ([`Staging::refresh_pages`]). `None` for a 3D texture, whose depth
    /// slices interleave, and when the counts disagree; then a change
    /// detiles the whole texture, as before.
    ranges: Option<Vec<(u64, u64)>>,
    /// Mips per array slice.
    mips: u32,
    /// The title's top mip levels the host copy lacks (memory pressure,
    /// [`crate::resource::dropped_mips`]): subresource `i` holds the title's
    /// mip `i + skipped` of its slice.
    skipped: u32,
    /// Mips and array slices in one native plane.
    subresources_per_plane: u32,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    memory: *const u8,
    /// The resource the staging buffer is of, for the resident mip window
    /// its views are narrowed to ([`note_resident`]).
    resource: This,
    /// The title released the resource: a list may still hold this staging
    /// and refresh it, and that must not note a window under an address
    /// another texture may have been given ([`forget`]).
    released: std::sync::atomic::AtomicBool,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then. (Every list that sampled a title texture
    /// uploaded all of it again, changed or not: 5,411 subresource copies
    /// of 1,120 textures in one frame of run 18.)
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// What the texture's first upload is doing when it was started ahead on
    /// the copy queue (`ahead`, `VICTORY_UPLOAD_AHEAD`): 0 nothing, 1 on its
    /// way, 2 it has run.
    ahead: std::sync::atomic::AtomicU8,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// The frame of its last refresh, `NEVER` if it has had none: what
    /// `VICTORY_TEXTURE_REFRESH_EVERY` bounds a texture's staleness by
    /// ([`Staging::due`]).
    looked: std::sync::atomic::AtomicU64,
    /// Which of the title's contents each copy and each present carries,
    /// and in what order ([`present_order`]).
    present: Present,
}

/// What a title texture's copies and presents have carried, so that a frame
/// shown out of order is read off the log rather than argued about: every
/// detiling of the staging buffer is a content generation (the movie's
/// frame, when the texture is a movie), the content a list's copy was
/// queued with is noted when that list runs, and each present says which
/// content its copy carries beside the newest one the staging buffer holds.
#[derive(Default)]
#[allow(dead_code)] // only the diagnostics-gated present line reads these
struct Present {
    /// Contents found (one per detiling), and detilings.
    contents: std::sync::atomic::AtomicU64,
    writes: std::sync::atomic::AtomicU64,
    /// The generation of the copy a list last queued, the detilings there
    /// had been, and the frame it was queued in.
    queued: std::sync::atomic::AtomicU64,
    queued_writes: std::sync::atomic::AtomicU64,
    queued_in: std::sync::atomic::AtomicU64,
    /// The generations presented, in order (the newest `SHOWN` of them).
    #[cfg(feature = "diagnostics")]
    shown: Mutex<Vec<u64>>,
    /// Each generation's bytes: (generation, the frame it was found in, a
    /// label of the bytes), the newest `HISTORY` of them.
    #[cfg(feature = "diagnostics")]
    history: Mutex<Vec<(u64, u64, u64)>>,
}

/// Which content a present of a title texture showed.
#[cfg(feature = "diagnostics")]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Order {
    /// The content the copy queued for this present carries.
    pub(crate) content: u64,
    /// The frame that copy was queued in.
    pub(crate) queued_in: u64,
    /// A label of that content's bytes.
    pub(crate) label: u64,
    /// The newest content the staging buffer holds, and the frame it was
    /// found in.
    pub(crate) staged: u64,
    pub(crate) staged_in: u64,
    /// Detilings of the staging buffer since the copy was queued: its
    /// bytes may be newer than the list that queued it saw.
    pub(crate) overwritten: u64,
    /// Presents of this texture, this one included.
    pub(crate) presents: u64,
    /// A content no older than this one was shown already: the frame is
    /// out of the order the title wrote (or the same frame again).
    pub(crate) back: bool,
}

impl Present {
    #[cfg(feature = "diagnostics")]
    const HISTORY: usize = 16;
    #[cfg(feature = "diagnostics")]
    const SHOWN: usize = 32;

    /// A new content was detiled into the staging buffer.
    #[allow(dead_code)] // called from the diagnostics-gated path
    fn detiled(&self, tiled: &[u8]) {
        use std::sync::atomic::Ordering::Relaxed;
        self.contents.fetch_add(1, Relaxed);
        self.writes.fetch_add(1, Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            let bytes = blake3::hash(tiled);
            let mut label = [0u8; 8];
            label.copy_from_slice(&bytes.as_bytes()[..8]);
            let frame = crate::frame::current();
            let written = self.contents.load(Relaxed);
            let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            history.push((written, frame, u64::from_le_bytes(label)));
            if history.len() > Self::HISTORY {
                history.remove(0);
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = tiled;
    }

    /// A list queued a copy of the content now in the staging buffer.
    #[allow(dead_code)] // called from the diagnostics-gated path
    fn queued(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.queued_writes.store(self.writes.load(Relaxed), Relaxed);
        self.queued_in.store(crate::frame::current(), Relaxed);
        self.queued.store(self.contents.load(Relaxed), Relaxed);
    }

    /// Whether the title's newest content is one no submitted list uploaded:
    /// what a present must put up itself, or show a copy behind.
    fn behind(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.queued.load(Relaxed) != self.contents.load(Relaxed)
    }

    /// What the present now shows (`filled`: the present put up the newest
    /// content itself), and how it compares with what was shown before.
    #[cfg(feature = "diagnostics")]
    fn order(&self, filled: bool) -> Order {
        use std::sync::atomic::Ordering::Relaxed;
        let staged = self.contents.load(Relaxed);
        let content = if filled {
            staged
        } else {
            self.queued.load(Relaxed)
        };
        let (staged_in, label) = {
            let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            history
                .iter()
                .find(|&&(g, _, _)| g == content)
                .map_or((0, 0), |&(_, frame, label)| (frame, label))
        };
        let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        let back = shown.iter().any(|&g| g >= content);
        let presents = shown.len() as u64 + 1;
        shown.push(content);
        if shown.len() > Self::SHOWN {
            shown.remove(0);
        }
        Order {
            content,
            queued_in: self.queued_in.load(Relaxed),
            label,
            staged,
            staged_in,
            overwritten: self
                .writes
                .load(Relaxed)
                .saturating_sub(self.queued_writes.load(Relaxed)),
            presents,
            back,
        }
    }
}

/// Whether a present's content is logged frame by frame
/// (`VICTORY_TEXTURE_ORDER=1`): the order of the movie's frames, from the
/// run itself.
#[cfg(feature = "diagnostics")]
pub(crate) fn order_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_ORDER").is_some_and(|v| v == "1"))
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
    /// The guard said "not written" and this check hashed anyway (the
    /// texture's turn); a changed hash then is a write the guard missed.
    checked: bool,
    /// A write the guard missed was found: the texture is hashed at every
    /// check from then on. A movie's frames, written by the title into view
    /// memory every frame, were refreshed one frame in eight (older frames
    /// shown again and again).
    missed: bool,
}

impl Last {
    /// `hash` is not the hash of the last read: what the guard let pass
    /// as unwritten was written.
    fn changed(&mut self, hash: blake3::Hash) {
        if self.checked && self.hash.is_some() && !self.missed {
            self.missed = true;
            // Told once per texture, so a run says whether the guard misses
            // writes at all (the intro: the movie's frames) and what it
            // costs to stop trusting it there.
            static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = MISSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                tracing::warn!(
                    n,
                    "a texture was written while the view guard said it was not: hashed at every check from now on (VICTORY_TEXTURE_GUARD_TRUST=1 does not learn this)"
                );
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexGuardMissed, 1);
        }
        self.hash = Some(hash);
    }
}

/// Textures found written while the guard said they were not.
pub(crate) static MISSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return false;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    let first = list.first_use(placed as usize);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list
        .transition(source, COPY_SOURCE)
        .map(|(texture, before)| {
            let state = crate::host::legal_state(texture, COPY_SOURCE);
            let barrier = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(state as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
            before
        });
    if !first && before.is_none() {
        return true;
    }
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. It never shows partly read.
///
/// The refresh has waited for this lock only for what is left of its
/// frame's [`waiting_room`] since; this switch is the same as a budget of
/// 0 (it is not the whole of it any more).
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there.
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// How many microseconds a thread's texture refreshes may wait for title
/// memory to hold still in one frame before they read through the kernel
/// instead (`VICTORY_TEXTURE_WAIT_US`, default 8000).
///
/// Waiting for the lock the runtime takes alone while it remaps title
/// memory was the whole of the streaming spikes: 1,033 ms of one frame's
/// refreshes in run fx-d3dc-1, 882, 746, 507 in others, against 6-10 ms of
/// asking the write watch. The wait cannot be shorter than the remapping,
/// so the budget caps it: past it, a texture is read through the kernel
/// (`refresh_unlocked`, about 0.6 ms a megabyte against 0.3 in place) and
/// so shows what it held before at worst a frame late. 0 waits always, as
/// before; any large value is `VICTORY_TEXTURE_NO_WAIT`'s behaviour.
fn wait_us() -> u64 {
    static US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    crate::switches::u64v(
        "VICTORY_TEXTURE_WAIT_US",
        *US.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_WAIT_US")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8_000)
        }),
    )
}

thread_local! {
    /// The frame this thread's [`wait_us`] budget was last reset in, and
    /// the microseconds it has spent waiting since.
    static WAITED: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((u64::MAX, 0)) };
}

/// Waits for title memory to hold still on this refresh's behalf: whatever
/// is left of this frame's budget (0: no wait at all).
fn waiting_room() -> u64 {
    let budget = wait_us();
    if budget == 0 {
        return 0;
    }
    let frame = crate::frame::current();
    WAITED.with(|w| {
        let (at, spent) = w.get();
        if at != frame {
            w.set((frame, 0));
            return budget;
        }
        budget.saturating_sub(spent)
    })
}

/// Adds `us` to this frame's waiting, for [`waiting_room`].
fn waited_out(us: u64) {
    if wait_us() == 0 {
        return;
    }
    let frame = crate::frame::current();
    WAITED.with(|w| {
        let (at, spent) = w.get();
        w.set((
            frame,
            if at == frame {
                spent.saturating_add(us)
            } else {
                us
            },
        ));
    });
}

/// Frames between two reads of a texture only part of which is mapped
/// (`VICTORY_TEXTURE_PARTIAL_FRAMES`, default 30): reading around unmapped
/// pages is slow and nothing tells us when pages arrive, so a mip that
/// streams in shows up to this many frames late.
pub(crate) fn partial_frames() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_PARTIAL_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
    })
}

/// Frames between two reads of a texture none of whose mips is mapped
/// (`VICTORY_TEXTURE_UNMAPPED_FRAMES`, default 2): it is black until one
/// arrives — a character's clothes and skin, the report's flat surfaces —
/// so it is looked at again straight away rather than up to 30 frames
/// later. Only for textures with nothing there: an unmapped read is the
/// whole texture's bytes of failing kernel reads.
fn unmapped_frames() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_UNMAPPED_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2)
    })
}

/// The runs of whole 64 KiB pages of title memory `[start, end)` written
/// after asking `previous`, as `(offset from `start`, bytes)`: pages outside
/// the texture are cut away and pages that meet are one run, read in one
/// call. Offsets are texture-relative because that is the space the
/// subresource ranges and [`victory_formats::texture::Layout::detile_window`]
/// use; the address itself is only for the read.
fn dirty_runs(
    written: &[(usize, u64)],
    previous: u64,
    start: usize,
    end: usize,
) -> Vec<(u64, usize)> {
    let mut pages: Vec<usize> = written
        .iter()
        .filter(|&&(_, w)| w > previous)
        .map(|&(p, _)| p)
        .collect();
    if pages.is_empty() {
        return Vec::new();
    }
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(u64, usize)> = Vec::new();
    for page in pages {
        let lo = (page * crate::watch::PAGE).max(start);
        let hi = ((page + 1) * crate::watch::PAGE).min(end);
        if lo >= hi {
            continue;
        }
        let at = (lo - start) as u64;
        match runs.last_mut() {
            Some(last) if last.0 + last.1 as u64 == at => last.1 += hi - lo,
            _ => runs.push((at, hi - lo)),
        }
    }
    runs
}

/// Whether a changed texture is detiled only where it changed, the 64 KiB
/// pages the title wrote since the last asking (`VICTORY_DETILE_DIRTY=0`
/// detiles the whole texture at every change, as before).
fn detile_dirty() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DETILE_DIRTY").is_none_or(|v| v != "0"))
}

/// Frames between two looks at a texture at most (`VICTORY_TEXTURE_REFRESH_EVERY`,
/// default 1: every frame, as before). A world that renders at a frame a second
/// spends most of that frame reading title memory for textures: measured on one
/// title and machine, one frame had 24,821 texture refreshes pulling 24.8 GB of
/// title memory with none of them skipped, 6,814 ms of the frame's 7,000 ms of
/// texture work spent waiting for it (Marvisak/VIctory#2478). At 8 the same
/// frame was 145-522 ms instead of 2,333-6,906 ms. The cost is staleness: a
/// texture's contents lag by up to this many frames. Asked at each refresh,
/// so it may be turned live (`crate::switches`).
fn refresh_every() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let base = *N.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_REFRESH_EVERY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1)
    });
    crate::switches::u64v("VICTORY_TEXTURE_REFRESH_EVERY", base)
}

/// Whether a texture at `address` is looked at in `frame`, `every` frames
/// apart at most: round-robin by its own address, so the frames share the
/// work instead of one frame taking all of it. `every` 0 or 1, and every
/// frame when the switch is unset: each frame, as before. (`address` alone
/// would put every texture of a heap on the same frame; the address's
/// 64 KiB are the unit, as their layout is.)
fn due_at(every: u64, frame: u64, address: usize) -> bool {
    every <= 1
        || frame
            .wrapping_add((address >> 16) as u64)
            .is_multiple_of(every)
}

/// No refresh yet ([`Staging::looked`]): older than any frame can be.
const NEVER: u64 = u64::MAX;

/// Whether a texture at `address` whose last refresh was in `looked` is read
/// in `frame`, `every` frames apart at most: on its own turn, so the frames
/// share the reads, and on any frame once it has gone `every` frames without
/// one, so a texture a title uses only on frames that are not its turn cannot
/// stay stale for longer than the interval. Never twice in one frame.
fn due(every: u64, frame: u64, address: usize, looked: u64) -> bool {
    if every <= 1 || looked == NEVER {
        return true;
    }
    let age = frame.wrapping_sub(looked);
    (age >= 1 && due_at(every, frame, address)) || age >= every
}

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). Round 3 of the memory work read every such texture that way
/// to be safe from pages taken away under the read; it cost 0.6 ms per MB
/// against 0.3 in place (90-105 ms a frame while the world streams in, runs
/// 72 and 73, against 20-40), and both runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Whether a refresh hashes and detiles a texture's bytes where they are when
/// the whole texture is mapped, instead of copying them out first
/// (`VICTORY_TEXTURE_IN_PLACE`; on unless 0). idea from xcl-dev/xcl#602
/// (dns54).
///
/// The copy is what the refresh of an unchanged texture paid for nothing: its
/// hash, and the detile only when it changed, are read from title memory
/// under the remap lock the refresh already holds (`title_memory_reading`),
/// which is what keeps the pages there while we read them.
fn in_place() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Live (crate::switches): asked per refresh.
    crate::switches::b(
        "VICTORY_TEXTURE_IN_PLACE",
        *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_IN_PLACE").is_none_or(|v| v != "0")),
    )
}

/// The largest texture read in place, MiB (`VICTORY_TEXTURE_INPLACE_MAX_MB`,
/// default 1). Past it the copy is made under the lock and the detile
/// outside it, as before: the runtime waits for every reader while it
/// remaps, so a long detile under the lock lengthens the next refreshes'
/// wait (the streaming spikes of `xcl-bin/notes/tex-fx.md` are that wait).
fn in_place_max() -> usize {
    static MB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // Live (crate::switches): asked per refresh.
    let mb = crate::switches::u64v(
        "VICTORY_TEXTURE_INPLACE_MAX_MB",
        *MB.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_INPLACE_MAX_MB")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(1)
        }) as u64,
    );
    (mb as usize) << 20
}

/// The hash of `[at, at + len)` read in place, twice: `None` when the title
/// moved the bytes between the two reads (it writes where the write watch
/// cannot see it, through another view), in which case this refresh decides
/// nothing and the next one looks again. `Some` is a hash of one stable
/// reading, equal to the hash of a copy taken at that moment.
///
/// SAFETY: `at` must be readable for `len` bytes for the whole call (the
/// caller holds the mapping, [`crate::watch::title_memory_reading`]).
unsafe fn stable_hash(at: *const u8, len: usize) -> Option<blake3::Hash> {
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(at, len) };
    settled(blake3::hash(bytes), blake3::hash(bytes))
}

/// [`stable_hash`]'s decision: two hashings of the same bytes that agree are
/// one reading of them; two that do not are a title writing while we read,
/// and nothing is decided on a mix.
fn settled(first: blake3::Hash, second: blake3::Hash) -> Option<blake3::Hash> {
    (first == second).then_some(first)
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped_at(read.as_mut_ptr(), memory, size, false, crate::host::SITE_TEXTURE);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
    }
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} staging buffers of textures let go ({} KiB), held by no list",
                bytes >> 10
            )
        });
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default)
            .insert(resource as usize)
    };
    // The host chain is complete; the window title memory was last found
    // with is stale and must not clamp the views of it any more (#616).
    note_resident(resource, (0, 0), true);
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe {
            std::ptr::from_ref(&desc)
                .cast::<[u32; 14]>()
                .read_unaligned()
        };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    at as u64,
                    (
                        bytes,
                        words[4],
                        words[6],
                        words[7],
                        words[8],
                        resource as usize,
                    ),
                );
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut()
        && let Some(staging) = map.remove(&resource)
    {
        // A list not yet reset may still hold it and refresh it: that must
        // not note a window for the released address.
        staging
            .released
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its resident window goes too: the address a handle names is given to
    // other textures, which must not inherit a clamp meant for this one.
    let mut resident = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = resident.as_mut() {
        map.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, d| d.5 != resource);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state. Whether a copy was recorded.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) -> bool {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return false;
    }
    let Some(staging) = staging(resource, texture) else {
        return false;
    };
    use std::sync::atomic::Ordering::Relaxed;
    staging.used.store(crate::host::moment(), Relaxed);
    // `VICTORY_UPLOAD_AHEAD`: a texture's first upload is read and copied on
    // the copy queue, ahead of the lists (`ahead`), so a frame's preparation
    // does not stop for the read of a whole texture; the submission waits for
    // it before its lists run (`VICTORY_SHOW_AFTER`).
    if ahead::on()
        && !staging.uploaded.load(Relaxed)
        && !list.writes_title(staging.memory as usize, staging.layout.size as usize)
    {
        let key = ahead::key(&staging);
        let upload = ahead::Upload {
            buffer: staging.buffer.clone(),
            footprints: staging.footprints.clone(),
            texture: texture.clone(),
            bytes: staging.bytes,
        };
        let detiling = staging.clone();
        // The claim is taken with the job (and the registry's lock), so
        // exactly one list starts a texture's first upload: two batches
        // copying one texture is not something D3D12 allows, and the second
        // would be waited on by nothing.
        if let Some(job) = ahead::take(
            key,
            &staging.ahead,
            upload,
            Box::new(move || {
                detiling.refresh_now();
            }),
        ) {
            // The host texture holds what the staging buffer was detiled to
            // once the copies have run, as a list's own uploads leave it; a
            // batch that did not run gives the claim back, and the next list
            // uploads it itself.
            let ran = staging.clone();
            job.then(move |copied: bool| {
                use std::sync::atomic::Ordering::Relaxed;
                ran.ahead
                    .store(if copied { ahead::DONE } else { ahead::NONE }, Relaxed);
                if copied {
                    ran.uploaded.store(true, Relaxed);
                    ran.dirty.store(false, Relaxed);
                    #[cfg(feature = "diagnostics")]
                    ran.present.queued();
                }
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexUploads, 1);
            return false;
        }
        // On its way with another list: this list records no copy, and the
        // submission waits for it before the lists run
        // (`ahead::synchronise`).
        if staging.ahead.load(Relaxed) == ahead::PENDING {
            return false;
        }
    }
    // The host texture has the staging buffer's contents: only the check
    // at submission is left, and what it finds changed the next list that
    // samples the texture uploads (a frame late where every list of the
    // frame was recorded before that check).
    if staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return false;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    if ahead::on() {
        // This list records the upload: no upload ahead may claim it until
        // this one has run (`ahead::LIST`), or the copy queue would write the
        // texture beside a drawing list's copy, with no fence between them.
        staging.ahead.store(ahead::LIST, Relaxed);
    }
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // When the list is submitted, its copy takes what the staging buffer
    // holds by then: noted before the check below, which sets `dirty`
    // again if it detiles (the next list then uploads once more).
    {
        let staging = staging.clone();
        list.defer_host(move || {
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);
            if ahead::on() {
                staging.ahead.store(ahead::DONE, Relaxed);
            }
        });
    }
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging.clone());
    // Diagnostics: when this list runs, its copy carries whatever content
    // the refresh above just found -- noted after that refresh, so a
    // present can say which content it shows and whether a detiling has
    // replaced those bytes since.
    #[cfg(feature = "diagnostics")]
    list.defer_host(move || staging.present.queued());
    true
}

impl Staging {
    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        // Its first upload is on the copy queue, reading these bytes: it must
        // have run before they are detiled over (`ahead`).
        if self.ahead.load(Relaxed) == ahead::PENDING {
            ahead::settle(ahead::key_of(self));
        }
        // `VICTORY_TEXTURE_REFRESH_EVERY`: read at most once every n frames
        // (see [`due`]). Its contents then lag by up to that many frames, and
        // what was detiled before stays what is uploaded. Only once it has
        // been uploaded: a texture's first contents always arrive, or it
        // would show what the staging buffer happened to hold.
        let frame = crate::frame::current();
        let every = refresh_every();
        let looked = self.looked.load(Relaxed);
        if every > 1
            && self.uploaded.load(Relaxed)
            && !due(every, frame, self.memory as usize, looked)
        {
            return;
        }
        self.looked.store(frame, Relaxed);
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(
            whole.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        if let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false))
        {
            let previous = last.asked;
            if previous != 0 && written.iter().all(|&(_, w)| w <= previous) {
                return 0;
            }
            last.asked = now;
            // Only the 64 KiB pages the title wrote since the last asking
            // are read and detiled, where the texture is big enough for that
            // to pay (`VICTORY_DETILE_DIRTY=0`: the whole texture, as
            // before).
            if previous != 0
                && let Some(read) = self.refresh_pages(&written, previous)
            {
                return read;
            }
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped_at(read.as_mut_ptr(), self.memory, size, false, crate::host::SITE_TEXTURE);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: 229 MB and 51 ms a frame in a gameplay view
        // where two textures change (run 58). It is guarded now, as buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns.
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) =
                waited(1, || crate::watch::writes(start, start + size, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            // Every scheduled refresh hashes, or with `VICTORY_TEXTURE_REFRESH_EVERY`
            // the two turns would multiply (7 and 8: a change found 56 frames
            // late, not 48).
            let turn = refresh_every() > 1
                || crate::frame::current()
                    .wrapping_add((start >> 16) as u64)
                    .is_multiple_of(HASH_EVERY);
            last.checked = unwritten && turn;
            // `VICTORY_TEXTURE_GUARD_TRUST=1`: a missed write is not learnt from.
            static TRUST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let trust = *TRUST.get_or_init(|| {
                std::env::var_os("VICTORY_TEXTURE_GUARD_TRUST").is_some_and(|v| v == "1")
            });
            if unwritten && !turn && (trust || !last.missed) {
                return 0;
            }
        }
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read in runs 71 and
        // 74).
        // Waited for, but only for what is left of this frame's budget
        // ([`waiting_room`]): past it the texture is read through the
        // kernel instead, whole or not at all (`refresh_unlocked`), which
        // is what `VICTORY_TEXTURE_NO_WAIT=1` does always.
        let Some(_mapped) = waited(2, || {
            let room = if no_wait() { 0 } else { waiting_room() };
            if room == 0 {
                return crate::watch::title_memory_reading_now();
            }
            let started = std::time::Instant::now();
            let got =
                crate::watch::title_memory_reading_within(std::time::Duration::from_micros(room));
            waited_out(started.elapsed().as_micros() as u64);
            got
        }) else {
            return self.refresh_unlocked(last, size);
        };
        if !committed(self.memory, size) {
            // Partly mapped: a streamed texture with only some mips' pages
            // there, the usual state while it streams in. Read through the
            // copy, which stands unmapped pages (zeros), and upload what
            // is there; nothing at all mapped changes nothing.
            // shortcut: read again at most every 30 frames (reading around
            // unmapped pages is slow, and nothing tells us when pages
            // arrive): a mip that streams in shows up to 30 frames late.
            // What of it is there, as a mip window: the views of it are
            // narrowed to it while it lasts (a streamed texture's mips
            // arrive from the smallest, at the start of each slice).
            let window = resident_mips(self.memory, &self.layout.planes[0].layout);
            if !self.released.load(std::sync::atomic::Ordering::Relaxed) {
                note_resident(self.resource, window, false);
            }
            #[cfg(feature = "diagnostics")]
            if window.1 > 0 && window.0 > 0 {
                crate::copystat::add(crate::copystat::C::ResidentNarrowed, 1);
            } else if window.1 == 0 {
                crate::copystat::add(crate::copystat::C::ResidentNone, 1);
            }
            // Read through the kernel, which needs no lock over the mapping.
            drop(_mapped);
            // Read again in `partial_frames`, or in `unmapped_frames` when
            // no mip at all is there (the texture is black until one
            // arrives, where a partly mapped one shows its small mips).
            let wait = if window.1 == 0 {
                unmapped_frames()
            } else {
                partial_frames()
            };
            let frame = crate::frame::current();
            if last.partial.is_some_and(|at| frame < at + wait) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = with_read(size, |read| {
                crate::host::copy_mapped_at(read.as_mut_ptr(), self.memory, size, false, crate::host::SITE_TEXTURE);
                let hash = blake3::hash(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.changed(hash);
                if read.iter().all(|&b| b == 0) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            let _ = changed;
            return size;
        }
        // All of it is there: no view of it is narrowed any more.
        if !self.released.load(std::sync::atomic::Ordering::Relaxed) {
            note_resident(self.resource, (0, 0), true);
        }
        if through_copy() {
            // A read through the kernel, which needs no lock.
            drop(_mapped);
            return with_title(self.memory, size, |tiled| {
                let hash = blake3::hash(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.changed(hash);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // The whole texture is mapped: hash it where it is, and detile it
        // where it is unless it is big (`in_place`). An unchanged texture then
        // costs one reading of it, not a copy and a reading.
        if in_place() && size <= in_place_max() {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            // SAFETY: `committed` said every page is there and `_mapped` keeps
            // it there.
            let hash = unsafe { stable_hash(self.memory, size) };
            match hash {
                Some(hash) if last.hash == Some(hash) => {
                    // Nothing changed: no copy, no detile, nothing at all.
                    drop(_mapped);
                    return size;
                }
                Some(hash) => {
                    last.hash = Some(hash);
                    // SAFETY: as above.
                    let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
                    self.detile(tiled);
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                    drop(_mapped);
                    return size;
                }
                // The title wrote it while we read it (the write watch cannot
                // see every write): this refresh decides nothing, the next one
                // looks again, and the texture shows what it showed.
                None => {
                    drop(_mapped);
                    return size;
                }
            }
        }
        // Only the copy is done under the lock (`_mapped` says the pages are
        // there): the hash and the detile are the biggest part of a refresh,
        // and the runtime waits for every reader while it remaps, so neither
        // is held under it. The copy stays in this thread's read buffer,
        // which nothing else touches between the two calls.
        // SAFETY: `committed` said every page is there and `_mapped` keeps
        // it there.
        with_read(size, |read| unsafe {
            std::ptr::copy_nonoverlapping(self.memory, read.as_mut_ptr(), size);
        });
        drop(_mapped);
        let fresh = with_read(size, |tiled| {
            let hash = blake3::hash(tiled);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            if last.hash == Some(hash) {
                return false;
            }
            // Learnt from a write the guard missed (`intro-fix`), and counted.
            last.changed(hash);
            self.detile(tiled);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            true
        });
        let _ = fresh;
        size
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. Then nothing is taken and the texture
    /// is looked at again at its next refresh (what was learnt about writes
    /// to it is forgotten, so that it is not passed over as unwritten).
    /// Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// Detiles only the pages of title memory written since `previous` (the
    /// asking the staging buffer was last brought up to date after), instead
    /// of the whole texture. Returns the bytes read, or `None` when the
    /// caller must take the whole texture (a first refresh, a 3D texture or
    /// one whose subresource count disagrees with the layout, or
    /// `VICTORY_DETILE_DIRTY=0`).
    ///
    /// A world atlas written in a few places a frame is read and detiled
    /// whole at each of its refreshes; the write watch already knows which
    /// 64 KiB pages changed, so only those runs are read, and only the
    /// elements of them each subresource covers are detiled.
    fn refresh_pages(&self, written: &[(usize, u64)], previous: u64) -> Option<usize> {
        use std::sync::atomic::Ordering::Relaxed;
        if !detile_dirty() || previous == 0 {
            return None;
        }
        let ranges = self.ranges.as_ref()?;
        let start = self.memory as usize;
        let end = start.saturating_add(self.layout.size as usize);
        // The runs are offsets from the texture's first byte, as `ranges`
        // are: the title's memory lies at whatever address the view got.
        let runs = dirty_runs(written, previous, start, end);
        if runs.is_empty() {
            return Some(0);
        }
        let mut read = 0usize;
        for &(run_at, run_len) in &runs {
            let run_start = start + run_at as usize;
            // SAFETY: a buffer of our own, filled by the copy.
            let mut buf = vec![0u8; run_len];
            // Read through the copy, which stands unmapped pages.
            crate::host::copy_mapped_at(buf.as_mut_ptr(), run_start as *const u8, run_len, false, crate::host::SITE_TEXTURE);
            for (sub, &(rs, re)) in ranges.iter().enumerate() {
                let a = rs.max(run_at);
                let b = re.min(run_at + run_len as u64);
                if a >= b {
                    continue;
                }
                let footprint = &self.footprints[sub];
                let f = footprint.Footprint;
                let pitch = f.RowPitch as usize;
                let rows = self.rows[sub] as usize;
                let len = pitch * rows * f.Depth as usize;
                // SAFETY: the subresource's part of the mapped staging buffer.
                let linear = unsafe {
                    std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
                };
                let (plane_index, mip, slice) =
                    plane_of(sub as u32, self.mips, self.subresources_per_plane);
                let Some(plane) = self.layout.planes.get(plane_index) else {
                    continue;
                };
                let window = &buf[(a - run_at) as usize..(b - run_at) as usize];
                // `ranges` and the runs are texture-relative; a plane's
                // layout numbers its own bytes from its offset.
                if let Err(e) = plane.layout.detile_window(
                    window,
                    a - plane.offset,
                    mip,
                    slice,
                    linear,
                    pitch,
                    pitch * rows,
                ) {
                    tracing::warn!(?e, mip, slice, "texture not detiled");
                }
            }
            read += run_len;
        }
        // The diagnostics' content generation (`Present`) is noted for whole
        // detiles only: the picture and the upload's `dirty` do not need it.
        self.dirty.store(true, Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexWatched, read as u64);
        Some(read)
    }
    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        self.present.detiled(tiled);
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.planes[0].layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            // D3D12 orders mips first, then array slices, then planes.
            let (plane_index, mip, slice) =
                plane_of(i as u32, self.mips, self.subresources_per_plane);
            let Some(plane) = self.layout.planes.get(plane_index) else {
                continue;
            };
            let start = plane.offset as usize;
            let Some(tiled) = tiled.get(start..start + plane.layout.size as usize) else {
                tracing::warn!(
                    i,
                    start,
                    size = plane.layout.size,
                    "a plane lies past the texture's memory"
                );
                return;
            };
            // The title's top mips the host copy lacks (dropped_mips) are not
            // subresources of a plane: they shift every mip it names.
            let mip = mip + self.skipped;
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) =
                plane
                    .layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start, its middle and its end
/// (a texture streams in whole mips; the small ones lie first). Read
/// through the kernel, so pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const WINDOW: usize = 16 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut hasher = blake3::Hasher::new();
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(3 * WINDOW, 0);
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped_at(
                read.as_mut_ptr(),
                (at as usize + from) as *const u8,
                bytes,
                false,
                crate::host::SITE_TEXTURE,
            );
            hasher.update(&read[..bytes]);
        };
        if len <= 3 * WINDOW {
            take(0, len);
        } else {
            take(0, WINDOW);
            take((len / 2) & !0xfff, WINDOW);
            take(len - WINDOW, WINDOW);
        }
    });
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first) | 1
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    let size = s.layout.size as usize;
    Some((size as u64, committed(s.memory, size)))
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true, 1);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: 8.8 ms for one question in the title's
/// 1.1 GB block (round 6). The pass over the heap's views asks three or
/// more of them for every texture slot that is due, 3,000 slots a frame
/// when the card has no room (run 75): that pass then ran 208-264 ms, and
/// while it ran the submitting thread's write-watch askings waited behind
/// the walks (run 76: 8-22 askings a frame of 6-21 ms each, 46-172 ms).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
///
/// Only one page in `stride` of those the query finds outside the working
/// set costs a read: an answer that needs no count of pages (is anything
/// there) asks about the first page of every 64 KiB block, the size the
/// title's memory is mapped in, and a texture that is not there at all then
/// costs 1,024 reads for its 64 MiB instead of 16,384, on every look.
fn readable(at: usize, len: usize, all: bool, stride: usize) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let stride = stride.max(1);
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            let read = (valid || (page + i) % stride == 0)
                && (valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte));
            if read {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// Whether any byte of `[at, at + len)` can be read: a texture streaming
/// in can have only its interior mips mapped, and it then has bytes to
/// stage though neither its first nor its last page is there.
fn any_mapped(at: *const u8, len: usize) -> bool {
    any_mapped_by(at as usize, len, |p, n| readable(p, n, false, 16).0)
}

/// [`any_mapped`] with the question about a range of pages passed in (how
/// many of them can be read), asked of 256-page batches as [`readable`] does
/// and stopping at the first that holds one: the batching is what a test
/// checks, without the kernel.
fn any_mapped_by(start: usize, len: usize, readable_pages: impl Fn(usize, usize) -> usize) -> bool {
    const BATCH: usize = 256 * 4096;
    let end = start.saturating_add(len);
    let mut p = start;
    while p < end {
        let n = (end - p).min(BATCH);
        if readable_pages(p, n) > 0 {
            return true;
        }
        p += n;
    }
    false
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. Neither end page committed is usually none (two queries
/// for the slots whose texture is away, which are asked again and again),
/// but a texture whose interior mips alone have arrived is not away: it is
/// looked for a readable page only then ([`any_mapped`]).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    if !committed(at, 1) && !committed((end - 1) as *const u8, 1) && !any_mapped(at, len) {
        return 0;
    }
    if !by_query() {
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE
            .get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1"))
        {
            // By page, without the walk: whole pages, cut to the range.
            let (there, _) = readable(start, len, false, 1);
            return (there as u64 * 4096).min(len as u64);
        }
        // By 64 KiB block, one page asked of each: the title's memory is
        // mapped in blocks of that size (the console's page), and the
        // answer is a measure of how much of a texture has arrived, not a
        // promise that every byte can be read (the upload reads through the
        // kernel). By page, a texture half there cost a failed kernel read
        // for each of its missing pages, every frame it stayed due: 1,663
        // textures "uploaded in part" kept the pass over the heap's views at
        // 90-160 ms a frame (run 81). `VICTORY_MAPPED_BY_PAGE=1`: by page.
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if readable(lo, 1, true, 1).0 == 1 {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        return there;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let next = (info.BaseAddress as usize)
            .saturating_add(info.RegionSize)
            .min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

/// Whether the texture the title is about to show has the title's newer
/// bytes still to be put up: `Some(true)` if a staging for it exists and
/// holds bytes no submitted list uploaded yet (`dirty` since the last
/// upload), `Some(false)` if it exists and is up to date, `None` if its
/// contents never come from title memory (the host renders to it). For the
/// present, which shows our copy of it either way: the run's
/// "written behind the guard" and the present line say whether a movie's
/// frames are ever a submission late.
#[allow(dead_code)] // the diagnostics-gated present line
pub(crate) fn present_pending(resource: This) -> Option<bool> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    use std::sync::atomic::Ordering::Relaxed;
    Some(staging.dirty.load(Relaxed) && !staging.uploaded.load(Relaxed))
}

/// Which content a present of `resource` shows beside the newest the
/// title's memory holds (`None` if its contents never come from title
/// memory): logged frame by frame with `VICTORY_TEXTURE_ORDER=1`, and
/// counted in the frame line whether it was shown out of order or with the
/// staging rewritten since its copy was queued.
#[cfg(feature = "diagnostics")]
pub(crate) fn present_order(resource: This, filled: bool) -> Option<Order> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?.clone();
    drop(map);
    Some(staging.present.order(filled))
}

/// `VICTORY_PRESENT_FILL=0` leaves the present showing what the title's
/// lists uploaded (the run then measures the old path).
fn fill_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_FILL").is_none_or(|v| v != "0"))
}

/// Whether the present of `resource` must put the title's newest content up
/// itself: its staging holds a content no submitted list uploaded (the
/// list that sampled it was recorded before the refresh found it, so what
/// the title's lists uploaded is a frame behind -- with two textures taking
/// turns, frames out of order).
pub(crate) fn present_fill_pending(resource: This) -> bool {
    if !fill_on() {
        return false;
    }
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|map| map.get(&(resource as usize)))
        .is_some_and(|staging| staging.present.behind())
}

/// Records in the present's own list (in order with the copy to the back
/// buffer, and before it) the refresh of `resource`'s staging buffer and
/// the copy of it into the host copy: what [`present_fill_pending`] asked
/// for. `texture` must be in the copy destination state; the caller moves
/// it on to the copy source state, as [`upload`] leaves it.
pub(crate) fn present_fill(list: &HostList, resource: This, texture: &ID3D12Resource) -> bool {
    let filled = upload(list, resource, texture);
    #[cfg(feature = "diagnostics")]
    if filled {
        crate::copystat::add(crate::copystat::C::PresentRefilled, 1);
    }
    filled
}

/// The mips of a title texture whose memory is all there: `(first, count)`,
/// `(0, 0)` if none is.
///
/// A streamed texture's pages arrive a few blocks at a time, and its
/// smallest mips lie at the start of each slice (`Layout::mip_extents`), so
/// what is there is a suffix of the mip chain: the memory below `mapped`
/// holds every mip from `first` on. A view over that range samples what the
/// console's residency clamp would have it sample, instead of reading zeros
/// for the mips that have not arrived.
pub(crate) fn resident_suffix(extents: &[(u64, u64)], mapped: u64) -> (u32, u32) {
    let n = extents.len();
    let mut first = n;
    while first > 0 {
        if extents[first - 1].1 > mapped {
            break;
        }
        first -= 1;
    }
    (first as u32, (n - first) as u32)
}

/// [`resident_suffix`] of the texture at `memory`: the mips all of whose
/// bytes are mapped now.
pub(crate) fn resident_mips(
    memory: *const u8,
    layout: &victory_formats::texture::Layout,
) -> (u32, u32) {
    if layout.size == 0 || layout.mips.is_empty() {
        return (0, 0);
    }
    let mapped = mapped_prefix(memory, layout.size as usize);
    if mapped >= layout.size {
        return (0, layout.mips.len() as u32);
    }
    if mapped == 0 {
        // Interior mips alone are there: no prefix to narrow the chain to,
        // so the view names the mips asked for and the ones that have not
        // arrived read zeros (the slot still shows the texture that was).
        return (0, layout.mips.len() as u32);
    }
    resident_suffix(&layout.mip_extents(), mapped)
}

/// Bytes of `[at, at + len)` mapped from its start without a gap, by 64 KiB
/// blocks (the console's page, how the title maps a texture: from the
/// start, smallest mips first). [`mapped_bytes`] counts every mapped block,
/// which is not the same thing: a texture whose first blocks are missing
/// (1,642 uploads in run fx-d3dc-1) has none of its smaller mips.
fn mapped_prefix(at: *const u8, len: usize) -> u64 {
    const BLOCK: usize = 64 << 10;
    let mut there = 0u64;
    let mut block = at as usize;
    let end = block.saturating_add(len);
    while block < end {
        let n = (end - block).min(BLOCK);
        if readable(block, 1, true, 1).0 != 1 {
            break;
        }
        there += n as u64;
        block += n;
    }
    there
}

/// The resident mip window `(first, count)` of each texture, noted at its
/// last staging refresh ([`note_resident`]) and read when a view of it is
/// made ([`resident`]): one store, shared by both.
static RESIDENT: Mutex<Option<HashMap<usize, (u32, u32)>>> = Mutex::new(None);

/// The resident mip window `(first, count)` of a texture a view is made on:
/// what [`resident_mips`] found at its last upload. `None` while it is all
/// there (or nothing is known: no staging buffer, no view to narrow).
pub(crate) fn resident(resource: This) -> Option<(u32, u32)> {
    let map = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()?.get(&(resource as usize)).copied()
}

/// Whether views of a part-resident texture are narrowed to the mips whose
/// memory is there (`VICTORY_RESIDENT_MIPS`; on unless 0). A streamed
/// texture's mips that have not arrived read as zeros, and the console
/// clamps the level of detail to what it holds; the host has no clamp but
/// the view's own window ([`narrowed`]). idea from JMAN730/victory#672
/// (jman/cursor/gpu-streamed-mip-view-616-111a).
pub(crate) fn resident_mips_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Live (crate::switches): asked per view.
    crate::switches::b(
        "VICTORY_RESIDENT_MIPS",
        *ON.get_or_init(|| std::env::var_os("VICTORY_RESIDENT_MIPS").is_none_or(|v| v != "0")),
    )
}

/// Whether the host GPU wrote `resource` (a copy or resolve), so its
/// contents are on the host and title-memory residency says nothing about
/// what its views may name.
pub(crate) fn is_host_written(resource: This) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)))
}

/// The window a host view of `resource` asking for `asked` should name now:
/// the title's own range once a host copy filled the chain
/// ([`is_host_written`]), else the mips that are there ([`narrowed`]).
pub(crate) fn wanted(
    asked: (u32, u32),
    resident: Option<(u32, u32)>,
    mips: u32,
    host_written: bool,
) -> (u32, u32) {
    let all = (
        asked.0,
        if asked.1 == 0 {
            mips.saturating_sub(asked.0)
        } else {
            asked.1
        },
    );
    if host_written {
        return all;
    }
    match resident {
        Some(window) if window.1 > 0 => narrowed(asked, window, mips),
        _ => all,
    }
}

/// Notes the resident mip window of `resource` (`(0, 0)` when none of its
/// mips is there): a view of it is narrowed to it while it stands, and
/// widened when more has arrived.
pub(crate) fn note_resident(resource: This, window: (u32, u32), whole: bool) {
    let mut map = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if whole {
        map.remove(&(resource as usize));
    } else {
        map.insert(resource as usize, window);
    }
}

/// The mips a view asks for, `(MostDetailedMip, MipLevels or 0 for the
/// rest)`, narrowed to the resident ones: what the view should name so that
/// every mip it can sample is there.
pub(crate) fn narrowed(asked: (u32, u32), resident: (u32, u32), mips: u32) -> (u32, u32) {
    let (first, levels) = asked;
    let end = if levels == 0 {
        mips
    } else {
        first.saturating_add(levels).min(mips)
    };
    let (rfirst, rcount) = resident;
    let rend = rfirst.saturating_add(rcount).min(mips);
    let start = first.max(rfirst).min(rend.saturating_sub(1));
    let stop = end.min(rend);
    // Nothing the view asked for is there: its most detailed resident mip
    // alone (the picture is the smallest mip, not black).
    let count = stop.saturating_sub(start).max(1);
    (start, count)
}

/// The plane, mip and array slice of native subresource `i`: D3D12 orders
/// mips first, then array slices, then planes (`subresources_per_plane` =
/// mips × slices).
fn plane_of(i: u32, mips: u32, subresources_per_plane: u32) -> (usize, u32, u32) {
    let subresource = i % subresources_per_plane;
    (
        (i / subresources_per_plane) as usize,
        subresource % mips,
        subresource / mips,
    )
}

/// Whether a depth-stencil texture's second plane is uploaded
/// (`VICTORY_TEXTURE_PLANES=0`: the first plane alone, as before).
fn planes_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_PLANES").is_none_or(|v| v != "0"))
}

/// The planes of `layout` the host takes, where the host takes more than
/// one: the host format's plane count (`host_planes`, `None` if the host
/// cannot say), for a format whose console planes the host lays out the
/// same way (the stencil plane its second, one byte a texel).
///
/// One plane otherwise: the console's 16-bit depth with 8-bit stencil is
/// three bytes a texel with the stencil alongside, where the host's depth
/// plane (R32 float, four bytes) is not the console's, and converting it
/// is not done here (the depth plane's twin carries its contents instead).
fn planes_uploaded(
    layout: &victory_formats::texture::Texture,
    host_planes: Option<u32>,
    format: u32,
) -> usize {
    if layout.planes.len() < 2 || !planes_on() {
        return 1;
    }
    // The host format must be a typed depth-stencil one: a copy writes one
    // plane of a texture at a time, and D3D12 refuses that for a TYPELESS
    // format (the runtime faults where it would size the copy).
    const TYPED: [u32; 2] = [20, 45]; // D32_FLOAT_S8X24_UINT, D24_UNORM_S8_UINT
    let Some(host_planes) = host_planes.filter(|_| TYPED.contains(&format)) else {
        return 1;
    };
    let matching = victory_formats::texture::format(format)
        .is_some_and(|f| f.stencil && f.bytes == layout.planes[0].layout.desc.element_bytes);
    if matching {
        (host_planes.max(1) as usize).min(layout.planes.len())
    } else {
        1
    }
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    // Read (and make) the host copy first: `hosted` takes the texture's own
    // lock, which `let_go_idle` holds while it calls `unstage` and so takes
    // this one.
    let skipped = crate::resource::dropped_mips(resource);
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(mut layout) = crate::resource::texture_layout(&desc) else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout
        .planes
        .iter()
        .any(|plane| plane.layout.desc.swizzle.xor())
    {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let subresources_per_plane = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    // The host's planes of a depth-stencil format, if it has any: asked
    // only where the console has a second plane to upload.
    let host_planes = if layout.planes.len() > 1 {
        let mut info = D3D12_FEATURE_DATA_FORMAT_INFO {
            Format: d.Format,
            ..Default::default()
        };
        // SAFETY: a live device and a correctly sized format query.
        unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_INFO,
                (&raw mut info).cast(),
                size_of_val(&info) as u32,
            )
        }
        .ok()
        .map(|()| u32::from(info.PlaneCount))
    } else {
        None
    };
    let planes = planes_uploaded(&layout, host_planes, d.Format.0 as u32);
    if planes < layout.planes.len() {
        // shortcut: the planes above the uploaded ones are not read: a
        // console texture whose depth plane the host holds in another form
        // (the console's 16-bit depth with 8-bit stencil, 3 bytes a texel)
        // keeps its first plane, as it was.
        layout.planes.truncate(planes);
        let last = layout.planes.last()?;
        layout.size = last.offset + last.layout.size;
    }
    let count = subresources_per_plane * layout.planes.len() as u32;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // A plane at a time: D3D12 gives a depth-stencil plane's own copy format
    // only when the subresources asked for are one plane's.
    for (plane, _) in layout.planes.iter().enumerate() {
        let first = (plane as u32 * subresources_per_plane) as usize;
        let mut plane_total = 0;
        // SAFETY: outputs for this plane's subresources.
        unsafe {
            host.device.GetCopyableFootprints(
                &d,
                first as u32,
                subresources_per_plane,
                total,
                Some(footprints[first..].as_mut_ptr()),
                Some(rows[first..].as_mut_ptr()),
                None,
                Some(&mut plane_total),
            );
        }
        total = total.max(total + plane_total);
    }
    // The buffer covers every footprint's whole rows: `GetCopyableFootprints`
    // leaves the last row of the last footprint unpadded, and a placed copy
    // names RowPitch × Height bytes.
    let total = footprints
        .iter()
        .zip(&rows)
        .map(|(f, &r)| {
            f.Offset + u64::from(f.Footprint.RowPitch) * u64::from(r) * u64::from(f.Footprint.Depth)
        })
        .fold(total, u64::max);
    let (buffer, data) = host.upload_buffer(total)?;
    // Memory each subresource (plane, then mip plus slice times mips)
    // occupies, for detiling only the pages that changed. 3D textures
    // interleave their depth slices, so they are detiled whole.
    let ranges = {
        let (mips, slices) = (u32::from(d.MipLevels), u32::from(d.DepthOrArraySize));
        let per_plane = mips.saturating_mul(slices);
        (d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE3D
            && per_plane != 0
            && per_plane == subresources_per_plane
            && layout.planes.len() as u32 * per_plane == count)
            .then(|| {
                let mut r = Vec::with_capacity(count as usize);
                for plane in &layout.planes {
                    let extents = plane.layout.mip_extents();
                    for slice in 0..slices {
                        let base = plane.offset + u64::from(slice) * plane.layout.slice_size;
                        for &(start, end) in &extents {
                            r.push((base + start, base + end));
                        }
                    }
                }
                r
            })
    };
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        buffer,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        ranges,
        mips: u32::from(d.MipLevels),
        skipped,
        subresources_per_plane,
        bytes: total,
        memory,
        resource,
        released: std::sync::atomic::AtomicBool::new(false),
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        ahead: std::sync::atomic::AtomicU8::new(0),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        looked: std::sync::atomic::AtomicU64::new(NEVER),
        present: Present::default(),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod in_place_tests {
    use super::{in_place, in_place_max, stable_hash};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// The hash of a stable range read in place is the hash of a copy of it,
    /// and a range the title moves under the reader is not decided on.
    #[test]
    fn the_live_hash_is_the_copy_of_it_and_a_moving_one_is_none() {
        // SAFETY: two pages of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * 4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        // SAFETY: our memory.
        let bytes = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, 2 * 4096) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 31 + 7) as u8;
        }
        // SAFETY: the range is ours and stays mapped for the call.
        let live =
            unsafe { stable_hash(at as *const u8, 2 * 4096) }.expect("a range nobody writes");
        assert_eq!(
            live,
            blake3::hash(bytes),
            "in place and through a copy are the same bytes"
        );
        // A title writing the bytes while they are read: the two hashings
        // disagree, and no decision is made on the mix (the next refresh
        // looks again).
        assert_eq!(super::settled(live, live), Some(live));
        let moved = blake3::hash(b"the title wrote this in between");
        assert_eq!(super::settled(live, moved), None);
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// The switch is on unless asked off, and the size limit has a default.
    #[test]
    fn the_switch_and_the_limit_have_defaults() {
        if std::env::var_os("VICTORY_TEXTURE_IN_PLACE").is_none() {
            assert!(in_place());
        }
        if std::env::var_os("VICTORY_TEXTURE_INPLACE_MAX_MB").is_none() {
            assert_eq!(in_place_max(), 1 << 20);
        }
    }
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    /// The written pages become texture-relative runs: the address the view
    /// got plays no part in the intersection with the subresource ranges,
    /// pages outside the texture are cut away, and pages that meet are one
    /// read. (An absolute address here skipped every subresource: the
    /// refresh then advanced its asking and kept the old pixels.)
    #[test]
    fn dirty_pages_become_texture_relative_runs() {
        const PAGE: usize = crate::watch::PAGE;
        let start = 0x7f00_0000_0000usize;
        let end = start + 4 * PAGE;
        let written = [
            (start / PAGE, 1),
            (start / PAGE, 2),
            (start / PAGE + 1, 2),
            (start / PAGE + 3, 3),
            (start / PAGE + 4, 3),
        ];
        assert_eq!(
            super::dirty_runs(&written, 1, start, end),
            vec![(0, 2 * PAGE), (3 * PAGE as u64, PAGE)]
        );
        assert_eq!(
            super::dirty_runs(&written, 2, start, end),
            vec![(3 * PAGE as u64, PAGE)]
        );
        // A page past the texture's end is not part of this one's runs.
        assert!(
            super::dirty_runs(&written, 2, start, end)
                .iter()
                .all(|&(at, len)| { at + len as u64 <= 4 * PAGE as u64 })
        );
        assert!(super::dirty_runs(&written, 3, start, end).is_empty());
    }

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| {
                assert!(bytes.iter().all(|&b| b == 0x77))
            });
            let base =
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(
                VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT),
                0
            );
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [
                3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0,
            ];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(
                size >= row * height as usize,
                "the layout holds every texel"
            );
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(
                    std::ptr::null(),
                    (pages + 1) * 4096,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                )
                .cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(
                    VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE),
                    base.cast()
                );
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(
                        direct[y * pitch..][..row].iter().any(|&b| b != 0xee),
                        "row {y} written"
                    );
                    assert!(
                        direct[y * pitch + row..(y + 1) * pitch]
                            .iter()
                            .all(|&b| b == 0xee),
                        "nothing past row {y}"
                    );
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::Last;

    #[test]
    fn a_change_the_guard_let_pass_makes_the_texture_hashed_every_check() {
        let (a, b) = (blake3::hash(b"a"), blake3::hash(b"b"));
        let mut last = Last::default();
        last.changed(a);
        assert!(!last.missed);
        // A turn's hash that finds a change after the guard said unwritten.
        last.checked = true;
        last.changed(b);
        assert!(last.missed);
        assert_eq!(last.hash, Some(b));
    }

    #[test]
    fn a_first_read_or_a_checked_turn_without_a_change_is_not_a_miss() {
        let mut last = Last {
            checked: true,
            ..Default::default()
        };
        last.changed(blake3::hash(b"a"));
        assert!(!last.missed);
    }
}

#[cfg(test)]
mod refresh_every_tests {
    use super::{NEVER, due, due_at, refresh_every};

    /// Unset, and `0` or `1`, is every frame: nothing changes for anyone who
    /// does not ask for it, and `0` is the way to ask for it off again.
    #[test]
    fn unset_zero_and_one_are_every_frame() {
        for every in [0, 1] {
            for frame in 0..8 {
                for address in [0x10_0000, 0x10_0040, 0x7f_0000_0000] {
                    assert!(
                        due_at(every, frame, address),
                        "every {every}, frame {frame}, at {address:#x}"
                    );
                }
            }
        }
        if std::env::var_os("VICTORY_TEXTURE_REFRESH_EVERY").is_none() {
            assert_eq!(refresh_every(), 1, "the switch's default is every frame");
        }
    }

    /// A texture a title samples only on frames that are not its turn is
    /// still read once the interval has passed: staleness is bounded by it,
    /// and no texture is read twice in one frame.
    #[test]
    fn an_off_turn_texture_is_read_once_the_interval_has_passed() {
        const EVERY: u64 = 8;
        // Its turn is every eighth frame; a title that samples it on the
        // frames in between (even frames here) still has it read.
        let address = 0x11_0000usize;
        let mut looked = NEVER;
        let mut reads = Vec::new();
        for frame in (0..64).step_by(2) {
            if due(EVERY, frame, address, looked) {
                reads.push(frame);
                looked = frame;
            }
        }
        assert_eq!(
            reads.len(),
            64 / EVERY as usize,
            "one read every {EVERY} frames: {reads:?}"
        );
        for pair in reads.windows(2) {
            assert!(
                pair[1] - pair[0] <= EVERY,
                "never stale for longer than {EVERY}: {reads:?}"
            );
        }
        // Twice in one frame is not a second read, and the frame after it is
        // off its turn: it waits for the interval.
        assert!(!due(EVERY, 0, address, 0), "read already in this frame");
        assert!(
            !due(EVERY, 1, address, 0),
            "one frame later, off its turn: no read"
        );
        // A texture used every frame is read once every n, on its own turn.
        let mut looked = NEVER;
        let mut reads = Vec::new();
        for frame in 0..64 {
            if due(EVERY, frame, 0x10_0000, looked) {
                reads.push(frame);
                looked = frame;
            }
        }
        assert_eq!(
            reads.len(),
            64 / EVERY as usize,
            "eight reads in 64 frames: {reads:?}"
        );
        assert!(reads.windows(2).all(|p| p[1] - p[0] == EVERY), "{reads:?}");
    }

    /// Every n frames at most, and a texture's turns are spread: over any n
    /// frames each texture is looked at exactly once, and two textures far
    /// apart in a heap are not looked at in the same frame.
    #[test]
    fn textures_take_turns_over_the_frames() {
        const EVERY: u64 = 8;
        // 64 KiB apart: one address unit each, so each has its own turn.
        let addresses = [0x10_0000usize, 0x11_0000, 0x12_0000, 0x13_0000];
        for &address in &addresses {
            let turns: Vec<u64> = (0..64).filter(|&f| due_at(EVERY, f, address)).collect();
            assert_eq!(
                turns.len(),
                64 / EVERY as usize,
                "{address:#x} is read once every {EVERY} frames"
            );
            // Not two turns closer than n frames, nor further apart either.
            for pair in turns.windows(2) {
                assert_eq!(
                    pair[1] - pair[0],
                    EVERY,
                    "{address:#x}: every {EVERY} frames"
                );
            }
            // And the frames differ between textures, 64 KiB apart.
            let in_window: Vec<u64> = (0..EVERY).filter(|&f| due_at(EVERY, f, address)).collect();
            assert_eq!(in_window.len(), 1);
            let others: Vec<u64> = addresses
                .iter()
                .filter(|&&a| a != address)
                .map(|&a| (0..EVERY).find(|&f| due_at(EVERY, f, a)).unwrap())
                .collect();
            assert!(
                others.iter().all(|&f| f != in_window[0]),
                "{address:#x} shares its frame with another"
            );
        }
    }
}

#[cfg(test)]
mod present_tests {
    use super::Present;

    /// The content a copy queued is behind the staging buffer's once the
    /// title writes again and it is detiled: what the present must put up
    /// itself.
    #[test]
    fn a_detiling_after_a_copy_leaves_it_behind() {
        let p = Present::default();
        assert!(
            !p.behind(),
            "no copy queued and no content: nothing to put up"
        );
        p.detiled(b"frame 1");
        assert!(p.behind(), "a content no copy carries yet");
        p.queued();
        assert!(!p.behind());
        p.detiled(b"frame 2");
        assert!(p.behind(), "the title wrote the next frame");
        p.queued();
        assert!(!p.behind());
    }

    /// A content shown when a newer one was shown already is out of order;
    /// a film shown in order never goes back.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn shown_content_that_goes_back_is_counted() {
        let p = Present::default();
        let mut order = Vec::new();
        for (i, bytes) in [b"1", b"2", b"3"].iter().enumerate() {
            p.detiled(*bytes);
            p.queued();
            let o = p.order(false);
            assert_eq!(o.content, i as u64 + 1);
            assert_eq!(o.staged, o.content);
            assert!(!o.back, "in order");
            order.push(o.presents);
        }
        assert_eq!(order, [1, 2, 3]);
        // The second frame shown again, as a copy a submission behind
        // would: the content goes back.
        p.queued();
        let o = p.order(false);
        assert!(o.back);
        // And one the present put up itself is the newest, never back.
        p.detiled(b"4");
        let o = p.order(true);
        assert_eq!(o.content, 4);
        assert_eq!(o.staged, 4);
        assert!(!o.back);
    }

    /// A depth-stencil texture's stencil plane is uploaded where the host
    /// lays it out as the console does, and the subresources of a plane are
    /// its mips and array slices, plane after plane: a stencil-only write of
    /// the title's then reaches the host's stencil plane.
    #[test]
    fn a_stencil_plane_is_uploaded_where_the_host_lays_it_out_the_same() {
        use super::{plane_of, planes_uploaded};
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        let texture = |format| {
            Texture::new(&TextureDesc {
                dimension: Dimension::Tex2d,
                width: 2,
                height: 2,
                depth_or_array: 2,
                mips: 2,
                format,
                samples: 1,
                swizzle: Swizzle::LINEAR,
            })
            .expect("a console layout")
        };
        // R32G8X24_TYPELESS: 32-bit depth, 8-bit stencil, one byte a texel,
        // the host's own two planes.
        let depth_stencil = texture(19);
        let typed_20 = texture(20);
        assert_eq!(depth_stencil.planes.len(), 2);
        // Both planes only with the switch on and a typed host format the
        // runtime can copy one plane of; one plane otherwise.
        let both = if super::planes_on() { 2 } else { 1 };
        assert_eq!(
            planes_uploaded(&typed_20, Some(2), 20),
            both,
            "depth and stencil"
        );
        assert_eq!(
            planes_uploaded(&typed_20, Some(0), 20),
            1,
            "a host that names no plane"
        );
        assert_eq!(
            planes_uploaded(&typed_20, Some(1), 20),
            1,
            "a host with one plane"
        );
        assert_eq!(
            planes_uploaded(&typed_20, None, 20),
            1,
            "a host that cannot say"
        );
        assert_eq!(
            planes_uploaded(&depth_stencil, Some(2), 19),
            1,
            "a TYPELESS host format"
        );
        let (depth, stencil) = (&depth_stencil.planes[0], &depth_stencil.planes[1]);
        assert_eq!(
            (
                depth.layout.desc.element_bytes,
                stencil.layout.desc.element_bytes
            ),
            (4, 1)
        );
        assert_eq!(
            stencil.offset,
            depth.layout.size.next_multiple_of(stencil.layout.alignment)
        );
        // The console's 16-bit depth with stencil stands on a 32-bit depth
        // plane on the host, not the same bytes: the depth plane alone, as
        // before (its twin carries the contents).
        let console_d16s8 = texture(118);
        assert_eq!(console_d16s8.planes.len(), 2);
        assert_eq!(console_d16s8.planes[0].layout.desc.element_bytes, 2);
        assert_eq!(planes_uploaded(&console_d16s8, Some(2), 45), 1);
        // Two mips and two slices a plane, plane after plane.
        let names = (0..8u32).map(|i| plane_of(i, 2, 4)).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                (0, 0, 0),
                (0, 1, 0),
                (0, 0, 1),
                (0, 1, 1),
                (1, 0, 0),
                (1, 1, 0),
                (1, 0, 1),
                (1, 1, 1)
            ]
        );
    }
}

#[cfg(test)]
mod resident_tests {
    use super::{narrowed, note_resident, resident, wanted};

    /// A view of a part-resident texture names the resident suffix of what
    /// it asked for, and never a mip that is not there. Hand-built windows
    /// only: the mip counts are the ones `Layout::mip_extents` yields.
    #[test]
    fn the_window_is_clamped_to_the_resident_mips() {
        // Eight mips asked for, the five smallest (3..8) arrived: those.
        assert_eq!(narrowed((0, 0), (3, 5), 8), (3, 5));
        // The whole chain arrived: as the title asked.
        assert_eq!(narrowed((0, 0), (0, 8), 8), (0, 8));
        // 0..3 asked for, only 2..6 there: the one mip of both, 2.
        assert_eq!(narrowed((0, 3), (2, 4), 8), (2, 1));
        // Nothing it asked for is there: the most detailed resident mip
        // alone (the smallest mip is the picture, not a window of none).
        assert_eq!(narrowed((0, 2), (5, 3), 8), (5, 1));
        // A window reaching past the chain is cut at its end.
        assert_eq!(narrowed((2, 0), (1, 2), 8), (2, 1));
        // One level, and that level arrived: unchanged.
        assert_eq!(narrowed((6, 1), (6, 2), 8), (6, 1));
    }

    /// One store behind the window: what a refresh notes is what a view of
    /// it reads back, and clearing it (what a host copy filling the chain
    /// does) is seen too. Each function used to have its own map, so
    /// nothing ever read back what was noted.
    #[test]
    fn the_noted_window_is_the_one_read_back() {
        // A handle no other test uses (the store is process-wide).
        let resource = 0x6720_0011u64 as _;
        assert_eq!(resident(resource), None, "nothing noted yet");
        note_resident(resource, (3, 5), false);
        assert_eq!(resident(resource), Some((3, 5)));
        // More levels arrived: the window moves.
        note_resident(resource, (2, 6), false);
        assert_eq!(resident(resource), Some((2, 6)));
        // Cleared (what a host copy filling the chain does, #616).
        note_resident(resource, (0, 0), true);
        assert_eq!(resident(resource), None);
        // The title releasing the resource drops its window: the next one at
        // that address starts unclamped.
        note_resident(resource, (4, 2), false);
        super::forget(resource as usize);
        assert_eq!(resident(resource), None);
        // And noted again after it: one store, both ways.
        note_resident(resource, (1, 2), false);
        assert_eq!(resident(resource), Some((1, 2)));
        note_resident(resource, (0, 0), true);
        assert_eq!(resident(resource), None);
    }

    /// A slot follows the resident suffix while it streams, and the title's
    /// own range again once a host copy filled its chain (#616).
    #[test]
    fn a_host_copy_widens_it_back_to_the_titles_range() {
        assert_eq!(wanted((0, 0), Some((4, 4)), 8, false), (4, 4));
        assert_eq!(wanted((0, 0), Some((4, 4)), 8, true), (0, 8));
        // Nothing known yet: what the title asked for, `0` levels being the
        // rest of the chain.
        assert_eq!(wanted((1, 0), None, 8, false), (1, 7));
        assert_eq!(wanted((0, 3), Some((0, 8)), 8, false), (0, 3));
    }
}

#[cfg(test)]
mod sparse_tests {
    use super::any_mapped_by;

    const PAGE: usize = 4096;

    /// How many pages of a batch at `first` can be read, with page `wanted`
    /// (the only one) the answer.
    fn only(wanted: usize) -> impl Fn(usize, usize) -> usize {
        move |first, n| usize::from((first..first + n).contains(&wanted))
    }

    /// A texture whose interior mips alone have arrived is not away: a page
    /// past an unreadable run answers for it, and none answers for one that
    /// is not there at all.
    #[test]
    fn the_sweep_finds_an_interior_readable_page() {
        assert!(
            any_mapped_by(0, 8 * PAGE, only(3 * PAGE)),
            "page 3 can be read"
        );
        assert!(!any_mapped_by(0, 8 * PAGE, |_, _| 0), "nothing there");
    }

    /// The sweep crosses its 256-page batches.
    #[test]
    fn the_sweep_crosses_a_batch() {
        assert!(
            any_mapped_by(0, 600 * PAGE, only(512 * PAGE)),
            "page 512 is in the second batch"
        );
    }
}
