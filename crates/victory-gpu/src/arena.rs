//! The arena: the title's own memory, handed to the GPU as its own.
//!
//! A part of the title's private memory is opened as a D3D12 heap
//! (`OpenExistingHeapFromAddress1`, which takes a part of an allocation, not
//! only its base) and one buffer is placed over all of it
//! ([`crate::inplace::place_part`]). What the submission's shaders read and
//! write through that buffer *is* the title's bytes: no upload before it, no
//! copy back after it. It is the layer a resident (`notes/resident-lane.md`,
//! #980 Stage B) stands on; nothing in this tree places one yet.
//!
//! The hole this module is built around: when vkd3d-proton refuses the import
//! it allocates memory of its own and tells nobody
//! (`HansKristian-Work/vkd3d-proton#3522`). The heap and the buffer are made,
//! every draw runs, and the title's memory never receives the shaders' writes.
//! Nothing in the API says so, so every part is asked **both ways** before it
//! is handed out ([`prove`]): the CPU writes at the title's address and the
//! GPU's buffer must read it back, and the GPU writes through the buffer and
//! the title's address must show it. A part that fails either way is dropped,
//! counted, and every ask after it gets a copy, as before.
//!
//! What is here is the subset of the fork's `arena.rs` (fxceless, PR #980,
//! `VICTORY_INPLACE=3`) that needs nothing of Stage B: the registry of the
//! title's committed regions as the runtime reports them
//! ([`title_memory_committed`], [`title_memory_released`]), the part grid
//! ([`grid_part`], [`PART`], [`STRIDE`]), a budget, and [`resolve`]. Not here:
//! sections (view memory) and the opening thread, both of which need the rest
//! of the fork's line (`notes/arena-lane.md` §4).
//!
//! `VICTORY_ARENA=1` turns it on; off (the default) every entry point is a
//! `None` and the runtime's reports are dropped.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Heap, ID3D12Resource};

use crate::inplace::{Cover, Placed, Rig, cover, place_part, private_whole};

/// The most a part holds: a D3D12 heap is "4 GiB or less", and the merge a
/// resident does counts 32-bit offsets from the buffer's first byte.
pub(crate) const PART: u64 = (4 << 30) - K64 as u64;
/// Parts of a longer run begin this far apart, so that a run committed in
/// steps can be open in more than one part without the parts overlapping.
pub(crate) const STRIDE: u64 = 2 << 30;
const K64: usize = 64 << 10;
/// The bytes one canary moves: the region's first cache line and a half, far
/// past the first 16 bytes any copy would have a chance to fake.
const SAMPLE: usize = 64;

/// Whether the arena may hand out title memory: `VICTORY_ARENA=1`. Off by
/// default; off, nothing is opened and the runtime's reports are dropped.
fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var("VICTORY_ARENA").ok().as_deref()))
}

/// [`on`]'s answer for a value of `VICTORY_ARENA`. Only `1` is on: a switch
/// that corrupts the picture when the import is refused must not be reached
/// by a typo.
fn switch(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Why a range is not in place (counted in [`TURNED`], by this order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// View memory: a section's pages, which this lane does not open.
    View = 0,
    /// Not memory the runtime reported committed.
    NotTitle,
    /// It spans pages not committed (a hole between the title's commits).
    NotCommitted,
    /// It spans pages of another protection than read-write.
    Protection,
    /// Its run of committed pages cannot hold it in one part.
    Size,
    /// The arena's budget is used up.
    Budget,
    /// The host refused to open the part.
    Host,
    /// The canary did not find the region both ways: the import was refused
    /// and the host used memory of its own.
    Canary,
}

const WHY_NAMES: [&str; 8] = [
    "view memory (not this lane)",
    "not memory the runtime reported committed",
    "spans pages not committed",
    "spans pages of another protection",
    "longer than a part of its run",
    "over the budget",
    "refused by the host",
    "the canary did not find the region both ways",
];

/// Ranges turned down since the start, by [`Why`].
pub(crate) static TURNED: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// Parts opened, retired, and their bytes, since the start.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Pages the canary proved, since the start.
static PROVEN: AtomicU64 = AtomicU64::new(0);

/// [`TURNED`] for the log: the kinds that turned something down.
pub(crate) fn turned_down() -> String {
    let parts: Vec<String> = WHY_NAMES
        .iter()
        .zip(&TURNED)
        .filter_map(|(name, n)| match n.load(Ordering::Relaxed) {
            0 => None,
            n => Some(format!("{n} {name}")),
        })
        .collect();
    if parts.is_empty() { "none".to_owned() } else { parts.join(", ") }
}

/// An open part of the title's memory and the buffer over it.
pub(crate) struct Part {
    /// The title address of the buffer's first byte.
    base: u64,
    bytes: u64,
    heap: ID3D12Heap,
    buffer: ID3D12Resource,
    gpu: u64,
}

// SAFETY: D3D12 objects are free-threaded, and the heap stays alive with the
// buffer (both are references to it).
unsafe impl Send for Part {}
// SAFETY: as above.
unsafe impl Sync for Part {}

impl Part {
    /// What [`resolve`] gives for `[at, end)`, all of it inside this part.
    fn resolved(&self, at: u64, end: u64) -> Resolved {
        let offset = at - self.base;
        Resolved {
            resource: self.buffer.clone(),
            heap: self.heap.clone(),
            offset,
            gpu: self.gpu + offset,
            left: self.base + self.bytes - end,
            title: at,
        }
    }
}

/// Title memory `[title, title + bytes)` as the GPU's own: a buffer over the
/// part it lies in, its heap, and where in the buffer it begins.
pub(crate) struct Resolved {
    pub(crate) resource: ID3D12Resource,
    /// Kept alive beside the buffer: dropping either alone is fine, but the
    /// heap must outlive every use of the buffer.
    pub(crate) heap: ID3D12Heap,
    /// The title range's first byte in `resource`.
    pub(crate) offset: u64,
    /// Its host GPU address.
    pub(crate) gpu: u64,
    /// Bytes from it to the part's end.
    pub(crate) left: u64,
    /// Its first title address.
    pub(crate) title: u64,
}

enum Slot {
    Open(Part),
    /// Refused, with why: asked again only after the range is released (the
    /// memory it was refused over may then be other memory).
    Refused(Why),
}

/// The title's committed regions and the parts opened over them.
#[derive(Default)]
struct State {
    /// Committed private title memory by its start: bytes.
    regions: BTreeMap<u64, u64>,
    /// Parts by (start, bytes).
    parts: BTreeMap<(u64, u64), Slot>,
    /// Bytes of open parts.
    locked: u64,
}

static ARENA: Mutex<Option<State>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = ARENA.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(State::default))
}

/// The most bytes of parts open at once (`VICTORY_ARENA_MB`; by default a
/// quarter of the PC's memory): an open part locks the title's pages.
pub(crate) fn most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_ARENA_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb << 20)
            .unwrap_or_else(|| crate::total_system_ram_bytes().unwrap_or(8 << 30) >> 2)
    })
}

/// The runs of pages of `[from, to)` as Windows has them now: one
/// `VirtualQuery` per run, each `Ok` for committed private read-write pages
/// and `Err(why)` otherwise.
pub(crate) fn runs_of(from: u64, to: u64) -> Vec<(u64, u64, Result<(), Why>)> {
    use windows_sys::Win32::System::Memory as mem;
    let mut runs = Vec::new();
    let mut at = from;
    while at < to {
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { mem::VirtualQuery(at as *const std::ffi::c_void, &mut info, size_of_val(&info)) };
        if n == 0 || info.RegionSize == 0 {
            runs.push((at, to, Err(Why::NotCommitted)));
            break;
        }
        let end = (info.BaseAddress as u64).saturating_add(info.RegionSize as u64).min(to);
        let state = if info.State != mem::MEM_COMMIT || info.Type != mem::MEM_PRIVATE {
            Err(Why::NotCommitted)
        } else if info.Protect != mem::PAGE_READWRITE {
            Err(Why::Protection)
        } else {
            Ok(())
        };
        runs.push((at, end, state));
        at = end;
    }
    runs
}

/// Of a run `[lo, hi)` of committed pages (both on 64 KiB), the part that
/// holds `[at, end)` whole: the lowest part every [`STRIDE`] from `lo`, each
/// at most [`PART`] and inside the run.
pub(crate) fn grid_part(lo: u64, hi: u64, at: u64, end: u64) -> Result<(u64, u64), Why> {
    if at < lo || end > hi || end <= at {
        return Err(Why::NotCommitted);
    }
    let mut start = lo;
    while start <= at {
        let bytes = (hi - start).min(PART) & !(K64 as u64 - 1);
        if bytes > 0 && end <= start + bytes {
            return Ok((start, bytes));
        }
        start += STRIDE;
    }
    Err(Why::Size)
}

impl State {
    /// Committed private title memory `[at, at + len)`, joined with what it
    /// continues or overlaps.
    pub(crate) fn committed(&mut self, at: u64, len: u64) {
        let Some(to) = at.checked_add(len).and_then(|end| end.checked_next_multiple_of(K64 as u64)) else {
            return;
        };
        let (mut from, mut to) = (at & !(K64 as u64 - 1), to);
        let touching: Vec<u64> = self.regions.range(..=to).filter(|&(&b, &n)| b.saturating_add(n) >= from).map(|(&b, _)| b).collect();
        for base in touching {
            if let Some(n) = self.regions.remove(&base) {
                from = from.min(base);
                to = to.max(base + n);
            }
        }
        self.regions.insert(from, to - from);
    }

    /// `[at, at + len)` is no longer committed title memory (or is about to
    /// be protected): the regions forget it and every part over it is
    /// retired, its bytes given back to the budget.
    pub(crate) fn released(&mut self, at: u64, len: u64) {
        let end = at.saturating_add(len.max(1));
        let hit: Vec<u64> = self.regions.range(..end).filter(|&(&b, &n)| b.saturating_add(n) > at).map(|(&b, _)| b).collect();
        for base in hit {
            if let Some(n) = self.regions.remove(&base) {
                if base < at {
                    self.regions.insert(base, at - base);
                }
                if end < base + n {
                    self.regions.insert(end, base + n - end);
                }
            }
        }
        let gone: Vec<(u64, u64)> = self.parts.keys().copied().filter(|&(s, n)| s < end && at < s + n).collect();
        for key in gone {
            if let Some(Slot::Open(part)) = self.parts.remove(&key) {
                self.locked -= part.bytes;
                STATS[1].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The region that holds `[at, end)` whole, if one does.
    fn region_of(&self, at: u64, end: u64) -> Option<(u64, u64)> {
        let (&base, &n) = self.regions.range(..=at).next_back().filter(|&(&b, &n)| at < b.saturating_add(n))?;
        (end <= base + n).then_some((base, n))
    }

    /// The open part that holds `[at, end)`, if one is.
    fn open_part(&self, at: u64, end: u64) -> Option<&Part> {
        self.parts.range((at.saturating_sub(PART), 0)..=(at, u64::MAX)).find_map(|(&(s, n), slot)| match slot {
            Slot::Open(part) if s <= at && end <= s + n => Some(part),
            _ => None,
        })
    }

    /// `[at, at + len)` in place: the open part over it, or a new one.
    pub(crate) fn resolve(&mut self, device: &ID3D12Device, at: u64, len: u64, most: u64) -> Result<Resolved, Why> {
        let end = at.checked_add(len.max(1)).ok_or(Why::NotTitle)?;
        // View memory is not this lane (a section would need a view of its
        // own and the rest of the fork's line, `notes/arena-lane.md` §4).
        if cover(at as usize, len.max(1) as usize) != Cover::None {
            return Err(Why::View);
        }
        let (base, n) = self.region_of(at, end).ok_or(Why::NotTitle)?;
        let runs = runs_of(base, base + n);
        let i = runs.partition_point(|r| r.1 <= at);
        let &(lo, hi, state) = runs.get(i).ok_or(Why::NotCommitted)?;
        state?;
        if end > hi {
            return Err(match runs.get(i + 1) {
                Some(&(_, _, Err(why))) => why,
                _ => Why::NotCommitted,
            });
        }
        let lo = lo.next_multiple_of(K64 as u64);
        let hi = hi & !(K64 as u64 - 1);
        let (start, bytes) = grid_part(lo, hi, at, end)?;
        if let Some(slot) = self.parts.get(&(start, bytes)) {
            return match slot {
                Slot::Open(part) => Ok(part.resolved(at, end)),
                Slot::Refused(why) => Err(*why),
            };
        }
        if self.locked.saturating_add(bytes) > most {
            return Err(Why::Budget);
        }
        let made = make_part(device, start, bytes);
        let slot = match made {
            Ok(part) => {
                self.locked += bytes;
                STATS[0].fetch_add(1, Ordering::Relaxed);
                STATS[2].fetch_add(bytes, Ordering::Relaxed);
                Slot::Open(part)
            }
            Err(why) => Slot::Refused(why),
        };
        let why = match &slot {
            Slot::Open(_) => Ok(()),
            Slot::Refused(why) => Err(*why),
        };
        self.parts.insert((start, bytes), slot);
        why?;
        self.open_part(at, end).map(|part| part.resolved(at, end)).ok_or(Why::NotTitle)
    }

    /// Whether `[at, at + len)` is in an open part now.
    pub(crate) fn ready(&self, at: u64, len: u64) -> bool {
        self.open_part(at, at.saturating_add(len.max(1))).is_some()
    }

    /// Open parts, their bytes, and the canary's proven pages.
    pub(crate) fn census(&self) -> (u64, u64) {
        let parts = self.parts.values().filter(|s| matches!(s, Slot::Open(_))).count() as u64;
        (parts, self.locked)
    }
}

/// A part of `[start, start + bytes)` opened on the title's own pages, if it
/// can be and the canary finds the region both ways.
fn make_part(device: &ID3D12Device, start: u64, bytes: u64) -> Result<Part, Why> {
    // The pages may have changed since the runtime reported them.
    if !private_whole(start as usize, bytes as usize) {
        return Err(Why::NotCommitted);
    }
    let placed: Placed = place_part(device, start, bytes).map_err(|e| if e.is_none() { Why::NotCommitted } else { Why::Host })?;
    match prove(device, &placed.buffer, start, bytes) {
        Proof::Proven(pages) => {
            PROVEN.fetch_add(pages, Ordering::Relaxed);
            tracing::info!(
                start = format_args!("{start:#x}"),
                mb = bytes >> 20,
                pages,
                "arena: a part of the title's own memory is the GPU's, both ways (canary)"
            );
        }
        found => {
            // The heap and the buffer are dropped here: the pages are
            // unlocked and every ask after this one is a copy.
            if found == Proof::Unfinished {
                // The canary's copy may still read these pages: the heap
                // and the buffer stay with it rather than being released
                // under a submission (the fork does the same).
                std::mem::forget(placed);
            }
            tracing::warn!(
                start = format_args!("{start:#x}"),
                mb = bytes >> 20,
                "arena: a part did not prove out ({}): the import was refused and the host used memory of its own - this part and every ask over it stay copies, and this is counted",
                match found {
                    Proof::Proven(_) => unreachable!(),
                    Proof::Elsewhere => "the canary never came back both ways",
                    Proof::Unmade => "the canary could not be made",
                    Proof::Unfinished => "the canary's copy never finished",
                }
            );
            return Err(Why::Canary);
        }
    }
    Ok(Part { base: start, bytes, heap: placed.heap, buffer: placed.buffer, gpu: placed.gpu })
}

/// What the canary found of the memory behind a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// Both directions held, over this many pages.
    Proven(u64),
    /// The bytes did not come back: memory of the host's own.
    Elsewhere,
    /// No queue: nothing is proven.
    Unmade,
    /// A copy was handed to the GPU and its end is not known: nothing is
    /// proven, and the placement must not be dropped under it.
    Unfinished,
}

/// Proves that the memory behind `buffer` (a placement over `[base, base +
/// bytes)`) is the title's, both ways, at up to three pages of the part:
///
/// - the CPU writes a sentinel at the title's address and the GPU's buffer
///   must read it back (what the shaders would read is the title's bytes),
/// - the GPU writes a sentinel through the buffer and the title's address
///   must show it (what the shaders write is the title's memory).
///
/// The title's bytes are put back either way. A host that took the heap and
/// used memory of its own fails the first direction immediately; one whose
/// card writes never reach the pages fails the second.
fn prove(device: &ID3D12Device, buffer: &ID3D12Resource, base: u64, bytes: u64) -> Proof {
    if bytes < K64 as u64 {
        return Proof::Unmade;
    }
    let Ok(mut rig) = Rig::new(device) else {
        return Proof::Unmade;
    };
    let mut probes = vec![base + 0x100];
    if bytes >= 2 * K64 as u64 {
        probes.push(base + ((bytes / 2) & !(SAMPLE as u64 - 1)));
    }
    probes.push(base + bytes - SAMPLE as u64);
    probes.sort_unstable();
    probes.dedup();
    let mut proven = 0;
    for at in probes {
        let offset = at - base;
        let mut sent = [0u8; SAMPLE];
        sentinel(&mut sent, 0x5a);
        let mut their = [0u8; SAMPLE];
        // SAFETY: `[base, base + bytes)` is private memory committed
        // read-write (`private_whole`), and the sample lies inside it.
        unsafe {
            std::ptr::copy_nonoverlapping(at as *const u8, their.as_mut_ptr(), SAMPLE);
            std::ptr::copy_nonoverlapping(sent.as_ptr(), at as *mut u8, SAMPLE);
        }
        let read = rig.fetch(buffer, offset, SAMPLE);
        let mut ours = [0u8; SAMPLE];
        sentinel(&mut ours, 0xa5);
        let stored = rig.store(buffer, offset, &ours);
        let written = stored.is_ok() && shows(at, &ours);
        // SAFETY: as above: the title's bytes go back the way they came.
        unsafe { std::ptr::copy_nonoverlapping(their.as_ptr(), at as *mut u8, SAMPLE) };
        match read {
            Ok(got) if got == sent => {}
            Ok(_) => return Proof::Elsewhere,
            // The fetch's copy was handed to the GPU: its end is not known,
            // and the buffer must not be dropped under it.
            Err(_) => {
                std::mem::forget(rig);
                return Proof::Unfinished;
            }
        }
        if stored.is_err() {
            std::mem::forget(rig);
            return Proof::Unfinished;
        }
        if !written {
            return Proof::Elsewhere;
        }
        proven += 1;
    }
    Proof::Proven(proven)
}

/// `SAMPLE` bytes that differ from page to page and seed to seed.
fn sentinel(bytes: &mut [u8; SAMPLE], seed: u8) {
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(seed) | 1;
    }
}

/// Whether the title's memory at `at` holds `bytes`.
fn shows(at: u64, bytes: &[u8]) -> bool {
    // SAFETY: the caller's own committed memory of that length.
    unsafe { std::slice::from_raw_parts(at as *const u8, bytes.len()) == bytes }
}

/// Title memory `[at, at + len)` in place in an arena, if it can be.
pub(crate) fn resolve(at: u64, len: u64) -> Option<Resolved> {
    let host = host()?;
    resolve_on(&host.device, at, len)
}

/// [`resolve`] against a device, for the host and the tests.
pub(crate) fn resolve_on(device: &ID3D12Device, at: u64, len: u64) -> Option<Resolved> {
    if !on() {
        return None;
    }
    let (resolved, why) = with(|state| match state.resolve(device, at, len, most()) {
        Ok(resolved) => (Some(resolved), None),
        Err(why) => (None, Some(why)),
    });
    if let Some(why) = why {
        TURNED[why as usize].fetch_add(1, Ordering::Relaxed);
    }
    resolved
}

/// The host's device, if there is one.
fn host() -> Option<&'static crate::host::Host> {
    crate::host::host()
}

/// Whether `[at, at + len)` is in an open part now.
pub(crate) fn ready(at: u64, len: u64) -> bool {
    on() && with(|state| state.ready(at, len))
}

/// Open parts and their bytes.
pub(crate) fn census() -> (u64, u64) {
    with(|state| state.census())
}

/// The runtime committed private title memory `[at, at + len)` that the GPU
/// may use (`XMemVirtualAlloc`): a part over it may be opened at its first
/// GPU use. Only memory told here is ever opened as a part.
pub fn title_memory_committed(at: usize, len: usize) {
    if !on() || len == 0 {
        return;
    }
    with(|state| state.committed(at as u64, len as u64));
}

/// `[at, at + len)` is no longer committed private title memory, or is about
/// to be given another protection: no part may stay open over it.
pub fn title_memory_released(at: usize, len: usize) {
    if !on() {
        return;
    }
    with(|state| state.released(at as u64, len as u64));
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::{D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON, ID3D12Device13};
    use windows::core::Interface;
    use windows_sys::Win32::System::Memory as mem;

    fn device() -> Option<ID3D12Device> {
        crate::test_device::device(D3D_FEATURE_LEVEL_12_0)
    }

    /// Only a plain `1` turns the arena on: a typo falls back to copies.
    #[test]
    fn only_a_plain_one_turns_the_arena_on() {
        assert!(switch(Some("1")));
        assert!(switch(Some(" 1 ")));
        for value in [None, Some(""), Some("0"), Some("on"), Some("true"), Some("2"), Some("1 0")] {
            assert!(!switch(value), "{value:?} must not open title memory");
        }
    }

    /// The grid: parts every STRIDE, at most PART, none outside the run.
    #[test]
    fn the_grid_cuts_parts_every_stride() {
        let lo = 0x1_0000_0000u64;
        let big = 10 << 30;
        let part = PART & !((K64 as u64) - 1);
        // The whole first part holds everything up to 4 GiB - 64 KiB, so a
        // range at 3 GiB is still in it, and one at 5 GiB is in the second.
        assert_eq!(grid_part(lo, lo + big, lo + 0x100, lo + 0x100 + PART / 2), Ok((lo, part)));
        assert_eq!(grid_part(lo, lo + big, lo + (3 << 30), lo + (3 << 30) + 0x1000), Ok((lo, part)));
        assert_eq!(grid_part(lo, lo + big, lo + (5 << 30), lo + (5 << 30) + 0x1000), Ok((lo + STRIDE, part)));
        // A range longer than a part is turned down: the part that holds it
        // must begin at or before its first byte, so a range starting at the
        // run's first byte has to fit the first part (the fork's rule, and a
        // resident has to fit one part).
        assert_eq!(grid_part(lo, lo + big, lo, lo + (5 << 30)), Err(Why::Size));
        // A range that no part of the run holds.
        assert_eq!(grid_part(lo, lo + part + 0x1000, lo, lo + part + 0x1000), Err(Why::Size));
        // Outside the run, and backwards.
        assert_eq!(grid_part(lo, lo + big, lo - 1, lo + 0x10), Err(Why::NotCommitted));
        assert_eq!(grid_part(lo, lo + big, lo + 0x10, lo + big + 1), Err(Why::NotCommitted));
        assert_eq!(grid_part(lo, lo + big, lo + 0x10, lo + 0x10), Err(Why::NotCommitted));
    }

    /// Commits are joined, a release splits them, and parts over a release are
    /// retired. No device needed: the registry is addresses and lengths.
    #[test]
    fn a_commit_joins_and_a_release_splits_the_regions() {
        let base = 0x2_0000_0000u64;
        let mut state = State::default();
        state.committed(base, 32 << 20);
        state.committed(base + (48 << 20), 80 << 20);
        assert_eq!(state.regions.get(&base), Some(&(32u64 << 20)));
        // A commit that touches both joins them into one region.
        state.committed(base + (32 << 20), 16 << 20);
        assert_eq!(state.regions.len(), 1);
        assert_eq!(state.regions.get(&base), Some(&(128u64 << 20)));
        // A release in its middle splits it.
        state.released(base + (64 << 20), 64 << 20);
        assert_eq!(state.regions.get(&base), Some(&(64u64 << 20)));
        assert_eq!(state.regions.get(&(base + (64 << 20))), None);
        // Not reported: not the title's, whatever Windows says.
        assert_eq!(state.region_of(base + (200 << 20), base + (200 << 20) + 0x1000), None);
        assert_eq!(state.region_of(base + 0x100, base + 0x200), Some((base, 64 << 20)));
        // A refusal over released memory is forgotten (it may be other memory now).
        state.parts.insert((base, 4 << 20), Slot::Refused(Why::Host));
        state.released(base + (1 << 20), 1 << 20);
        assert!(!state.parts.contains_key(&(base, 4 << 20)));
        assert_eq!(state.regions.get(&base), Some(&(1u64 << 20)));
        assert_eq!(state.regions.get(&(base + (2 << 20))), Some(&(62u64 << 20)));
    }

    /// A part of the title's own pages opens, the canary proves it both ways,
    /// and the GPU's buffer is the title's bytes at the title's offset.
    #[test]
    fn warp_a_part_opens_on_the_titles_pages_and_the_canary_proves_it_both_ways() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let bytes = 8 * K64 as u64;
            let base = mem::VirtualAlloc(std::ptr::null(), bytes as usize, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            let at = base as u64;
            std::ptr::write_bytes(base, 0x11, bytes as usize);
            let mut state = State::default();
            state.committed(at, bytes);
            let resolved = state
                .resolve(&device, at + K64 as u64 + 0x30, 0x1000, u64::MAX)
                .unwrap_or_else(|why| panic!("not in place: {why:?}"));
            assert_eq!(resolved.title, at + K64 as u64 + 0x30);
            assert_eq!(resolved.offset, resolved.title - at, "the part begins at the run's first page");
            assert_eq!(resolved.gpu, resolved.resource.GetGPUVirtualAddress() + resolved.offset);
            assert_eq!(resolved.left, bytes - resolved.offset - 0x1000, "from the asked range's end to the part's end");
            // The GPU reads the title's bytes, and a write through the buffer
            // lands at the title's address.
            let mut rig = Rig::new(&device).unwrap();
            let got = rig.fetch(&resolved.resource, resolved.offset, SAMPLE).unwrap();
            assert!(shows(resolved.title, &got), "the buffer shows the title's bytes");
            rig.store(&resolved.resource, resolved.offset, &[0x77; SAMPLE]).unwrap();
            assert!(shows(resolved.title, &[0x77; SAMPLE]), "the GPU's write is at the title's address");
            // A second ask in the same part is served by it, not reopened.
            let again = state.resolve(&device, at + 0x100, 0x100, u64::MAX).unwrap();
            assert_eq!(again.gpu - again.offset, resolved.gpu - resolved.offset);
            // Asking outside what the runtime reported is not the title's.
            assert_eq!(state.resolve(&device, at + 2 * bytes, 0x100, u64::MAX).err(), Some(Why::NotTitle));
            drop((resolved, again));
            // Retired, the part is gone and the budget is whole again.
            state.released(at, bytes);
            assert_eq!(state.census(), (0, 0));
            assert_ne!(mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE), 0);
        }
    }

    /// The canary refuses memory of the host's own: a buffer that is not over
    /// the title's pages never comes back, so a part like it is never handed
    /// out. This is the hole of vkd3d-proton#3522, made visible on purpose.
    #[test]
    fn warp_the_canary_refuses_a_buffer_that_is_not_the_titles() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        let mut rig = Rig::new(&device).unwrap();
        let card = rig.committed(D3D12_HEAP_TYPE_DEFAULT, SAMPLE as u64, D3D12_RESOURCE_STATE_COMMON).unwrap();
        rig.store(&card, 0, &[0u8; SAMPLE]).unwrap();
        // SAFETY: an allocation of the test's own.
        unsafe {
            let bytes = K64;
            let base = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            std::ptr::write_bytes(base, 0x22, bytes);
            assert_eq!(
                prove(&device, &card, base as u64, bytes as u64),
                Proof::Elsewhere,
                "memory of the card's own cannot pass for the title's"
            );
            assert_ne!(mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE), 0);
        }
    }

    /// Allocation and first use of N regions, and the copies an arena saves:
    /// a region copied is uploaded (title → card) and copied back (card →
    /// title) per submission; one in place is neither. Costs are measured on
    /// this card, not assumed.
    #[test]
    fn warp_arena_bench_regions() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
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
        let mut state = State::default();
        let mut open_ms = 0.0f64;
        let mut resolved = Vec::new();
        let mut bases = std::collections::BTreeSet::new();
        for (i, &at) in title.iter().enumerate() {
            state.committed(at, bytes as u64);
            let asked = std::time::Instant::now();
            let part = state
                .resolve(&device, at + 0x100, 0x100, u64::MAX)
                .unwrap_or_else(|why| panic!("region {i} not in place: {why:?}"));
            open_ms += asked.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(part.title, at + 0x100, "the range sits in its own region");
            assert!(bases.insert(part.gpu - part.offset), "each region has its own part");
            resolved.push(part);
        }
        let mut rig = Rig::new(&device).unwrap();
        // What a copy of one region costs on this card (title → card; the
        // copy back is the same DMA the other way).
        let copy_ms = rig.copy_ms(&resolved[0].resource, bytes as u64).unwrap();
        eprintln!(
            "arena bench: {N} regions of {} KiB: {:.3} ms each to open a part and prove it both ways ({open_ms:.3} ms total), {} parts open, {} MB locked",
            bytes >> 10,
            open_ms / N as f64,
            state.census().0,
            state.census().1 >> 20
        );
        eprintln!(
            "arena bench: a copy of {} KiB on this card: {copy_ms:.3} ms; a copied region pays two per submission, an arena region none: {N} regions = {} copies ({} KiB) and {:.3} ms per submission saved",
            bytes >> 10,
            2 * N,
            2 * N * (bytes >> 10),
            2.0 * N as f64 * copy_ms
        );
        assert!(open_ms < 10_000.0, "opening {N} regions took {open_ms} ms");
        drop(resolved);
        for &at in &title {
            state.released(at, bytes as u64);
        }
        assert_eq!(state.census(), (0, 0));
        // SAFETY: the bench's own allocations, no part open over them.
        unsafe {
            for &at in &title {
                assert_ne!(mem::VirtualFree(at as *mut _, 0, mem::MEM_RELEASE), 0);
            }
        }
    }
}
