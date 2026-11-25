//! Title memory as the card's own memory: a host heap opened on memory
//! the title already has, and a buffer placed in it, so that shaders read
//! and write the title's bytes themselves and no host copy stands between
//! (the console's one memory). This file holds the two openings Direct3D
//! has, a probe that says what this PC's driver makes of them
//! (`VICTORY_INPLACE_PROBE`), and a census of which title memory could be
//! opened.
//!
//! What the Direct3D runtime and the kernel answer, measured on this PC
//! with the software adapter (the rules are theirs, not the adapter's;
//! what the card's driver adds is what the probe is for):
//!
//! - `OpenExistingHeapFromAddress` takes the base address of a whole
//!   `VirtualAlloc` allocation and nothing inside it; every page must be
//!   committed and have one protection. Opening locks every page into the
//!   PC's memory at once (256 MiB never touched: 70 ms and 256 MB of
//!   working set).
//! - `OpenExistingHeapFromAddress1` (`ID3D12Device13`) takes any committed
//!   part of an allocation, with the same rule for its pages; parts may
//!   overlap, and the whole may be opened beside them. It also takes a
//!   committed view of a section.
//! - `OpenExistingHeapFromFileMapping` takes a section made with
//!   `SEC_COMMIT` and refuses one made with `SEC_RESERVE` (how `xmem`
//!   makes the title's page pool today). The heap is the section, not a
//!   view of it: views may be unmapped, mapped elsewhere or made
//!   read-only, and the section's handle closed, with the heap alive.
//!   Opening it locks nothing (256 MiB: 0.2 ms, no working set).
//! - The heap is a custom heap in the PC's memory (write-back pages, pool
//!   L0), shared and cross-adapter shared, 64 KiB aligned. A buffer in it
//!   must have `ALLOW_CROSS_ADAPTER`; unordered access beside it is taken.
//!   A texture must be row-major with one level. Tile mappings into such
//!   a heap are refused and REMOVE THE DEVICE: never call them.
//! - While a heap opened from an address stands, the title cannot
//!   decommit, release or protect the pages in it (`VirtualFree` and
//!   `VirtualProtect` fail with error 87), nor unmap a view in it. Pages
//!   outside the opened part are free to change.
//! - `OpenExistingHeapFromAddress` takes one shape and no other: the
//!   allocation's own base, one committed region, one protection
//!   (vkd3d-proton `device.c:7603-7627`). A `MapViewOfFile` view of a section
//!   is not that shape, and `OpenExistingHeapFromFileMapping` (`device.c:7637`)
//!   maps the section again and unmaps it before the GPU uses the heap, so
//!   neither is a way in for the page pool; memory we make ourselves is
//!   `VirtualAlloc`ed as one region, and the title's own pages go through
//!   `OpenExistingHeapFromAddress1`, which does take a part
//!   (`xcl-bin/notes/linux-stack-fixes.md` 2.1). Needs vkd3d-proton master at
//!   `563eb6c` (2026-11-04, "Do not force a failure in OpenHeapFromAddress"),
//!   which 3.1.0 predates; our bundled `ec20719` has it.
//! - A heap that opened is not a promise that the GPU uses those pages:
//!   when the import is refused vkd3d-proton allocates memory of its own
//!   and says nothing to the app (`xcl-bin/notes/zero-copy.md` §2, upstream
//!   `HansKristian-Work/vkd3d-proton#3522`). So [`check`] moves bytes both
//!   ways through a page of the process's own memory once, when the host
//!   device is made, and every copy stays a copy unless they arrive.
//! - That answer is about memory we made, not about the title's: a section
//!   through a view opens and still is not the GPU's pages (pablogonz12/
//!   Scarlight#144). So the first region placed in place has a canary
//!   written through the title's own address and read back through the
//!   GPU's copy of it ([`canary`]); the region is kept only if the bytes
//!   come back, and the whole run falls back to copies if they do not
//!   ([`title_pages_arrived`]). `VICTORY_INPLACE_CANARY=0` takes that
//!   answer from the startup self-check alone, as before.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

/// The unit heaps and placed buffers are cut on, and the console's page.
const K64: usize = 64 << 10;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// `VICTORY_INPLACE_PROBE`: 0 (unset) nothing; 1 the probe on memory of
/// its own at the first sweep, and the census 30 and 150 seconds later;
/// 2 also opens one part of the title's own memory for a moment at the
/// first census.
pub(crate) fn probe_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| std::env::var("VICTORY_INPLACE_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// `VICTORY_INPLACE`: 2 (unset) on, the measured-safe class only; 1: a
/// window of title memory outside buffers only; 0: off, every copy as
/// before. At 2 a window or a title buffer that lies in private memory
/// committed whole is not copied: a part of the title's allocation is
/// opened as a host heap and the window is a buffer placed over it
/// ([`place_for`]).
///
/// What 1 and 2 cover is private memory (`XMemVirtualAlloc`'s). View
/// memory (the page pool) is turned down here: its section is made
/// reserved, which the host refuses, and what a view shows changes when
/// the title maps pages.
///
/// On by default since the probe and the game runs of 24-25 Nov say this
/// class is safe and the rest is not (`xcl-bin/notes/zero-copy.md`):
/// private memory in place read true, the CPU read at its own address what
/// the GPU wrote with no flush and no map true, and write watching saw the
/// GPU's writes true; a committed section through a view read false and
/// wrote false, whatever the views did. The same split in the game: with
/// view memory in place (the fork's `4cbe9d76`) every presented frame was
/// black from frame 1, and with only that class refused the menus and the
/// intro were right. What it costs: opening 64 MB locked into the PC's
/// memory takes 15.3 ms once (4 MB: 1.6 ms), and the card reads such a heap
/// at 9.2 GB/s at 256 MB against 20.5 GB/s from a committed buffer of
/// today's kind - the CPU copy it replaces costs more than that difference.
///
/// What it assumes, for 2: nothing reads title memory that a submission
/// not yet run writes through such a buffer. With every submission waited
/// for (the default) that holds; with `VICTORY_LATE_WAIT` it does not,
/// since the flights' rules know shader writes only by the ranges copies
/// declare. Do not combine 2 with late wait.
///
/// shortcut: while a window is in place the title cannot decommit, free
/// or protect its pages (the call fails with error 87; the probe shows
/// it). The title's heaps are allocated once and kept, and nothing of the
/// kind was seen; [`title_memory_freeing`] is there for the runtime to
/// call before such a call once it takes them over.
pub(crate) fn level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        let level = level_from(std::env::var("VICTORY_INPLACE").ok().as_deref());
        if level > 1 && crate::host::late() {
            tracing::info!("title buffers are not used in place: VICTORY_LATE_WAIT is on");
            return 1;
        }
        level
    })
}

/// [`level`] from the variable's text: unset (or empty, or not a number) is
/// 2, the measured-safe class on; `0` off; `1` windows outside buffers only.
fn level_from(text: Option<&str>) -> u8 {
    match text.map(str::trim) {
        None | Some("") => 2,
        Some(text) => text.parse().unwrap_or(2),
    }
}

/// The most title memory in place at once, in bytes
/// (`VICTORY_INPLACE_MB`, 4096): opening locks the pages into the PC's
/// memory, also those the title never touched.
fn most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| std::env::var("VICTORY_INPLACE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096) << 20)
}

/// In place now: windows and the pages they lock, title buffers and theirs;
/// then, since the start: asks turned down because the memory is a view
/// or not private memory committed whole, because the host refused,
/// because of [`most`]; and what was let go because its memory changed;
/// and asks the canary turned down because the GPU never read the title's
/// own bytes through the placement.
static STATS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// A buffer placed over title memory, and its heap.
pub(crate) struct Placed {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) heap: ID3D12Heap,
    /// The host GPU address of the title memory's first byte.
    pub(crate) gpu: u64,
    /// Bytes from the buffer's first byte to the title memory's first.
    pub(crate) bias: u64,
}

/// Opens the 64 KiB pages that hold title memory `[start, start + len)`
/// as a heap of their own and places one buffer over all of them. `Err`
/// with nothing if the memory is not private memory committed whole
/// (nothing was asked of the host), with the host's error if it refused.
pub(crate) fn place_part(device: &ID3D12Device, start: u64, len: u64) -> Result<Placed, Option<windows::core::Error>> {
    let at = start & !(K64 as u64 - 1);
    let bytes = (start + len.max(1)).next_multiple_of(K64 as u64) - at;
    if !private_whole(at as usize, bytes as usize) {
        return Err(None);
    }
    let heap = open(device, From::Part(at as usize, bytes as usize)).map_err(Some)?;
    let buffer = place(device, &heap, 0, bytes).map_err(Some)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed { buffer, heap, gpu, bias: start - at })
}

/// Title memory `[start, start + len)` in place, for a window outside
/// buffers or a title buffer, if [`level`] asks for it and the memory can
/// be opened; `None` leaves the caller to make a copy, as before.
pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    if level() < if window { 1 } else { 2 } {
        return None;
    }
    // Late wait can come on *after* [`level`] was read (`switches::poll`): a
    // title buffer in place assumes every submission has been waited for, so
    // the question is asked here, not of the cached level.
    if !window && crate::host::late() {
        STATS[4].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // View memory: known from the runtime without asking Windows.
    if cover(start as usize, len as usize) != Cover::None {
        STATS[4].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // A window of a few KiB lies in a 64 KiB page that the heap locks whole,
    // so the cap is charged (and [`gone`] releases) the rounded span, never
    // the title's: seventeen 4 KiB spans can hold 1,088 KiB of imports, not
    // the 68 KiB their lengths add up to.
    let pages = charged(len);
    if STATS[1].load(Ordering::Relaxed) + STATS[3].load(Ordering::Relaxed) + pages > most() {
        STATS[6].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let host = crate::host::host()?;
    // The host may take the heap and still give the GPU other memory: the
    // import is refused and the driver falls back without a word, so only
    // the self-check's answer says whether a placed buffer shows the
    // title's bytes at all.
    if !in_place(check(&host.device)) {
        UNCHECKED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    match place_part(&host.device, start, len) {
        Ok(placed) => {
            // The heap and the buffer were taken, but the GPU may hold memory
            // of its own: the import was refused and vkd3d-proton fell back
            // without a word (`HansKristian-Work/vkd3d-proton#3522`). The
            // canary over the title's own bytes is the only way to see it,
            // and a region it does not find is a copy, never a wrong picture.
            match title_pages_arrived(&host.device, &placed, start, len) {
                Canary::Arrived => {}
                Canary::Unfinished => {
                    // The canary's copy was handed to the GPU and its end is
                    // not known: the heap and the buffer stay with it (as
                    // `ask_page` keeps his), rather than being released under
                    // a submission that may still read them.
                    STATS[8].fetch_add(1, Ordering::Relaxed);
                    std::mem::forget(placed);
                    return None;
                }
                _ => {
                    STATS[8].fetch_add(1, Ordering::Relaxed);
                    drop(placed);
                    return None;
                }
            }
            let at = if window { 0 } else { 2 };
            STATS[at].fetch_add(1, Ordering::Relaxed);
            STATS[at + 1].fetch_add(pages, Ordering::Relaxed);
            Some(placed)
        }
        Err(None) => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(Some(e)) => {
            if STATS[5].fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "title memory not opened in place: the host refused, a copy instead (and every ask after it until one is let go)");
            }
            None
        }
    }
}

/// What the cap is charged for `len` bytes of title memory at `start`: the
/// pages a heap locks for it, and never less. It is the upper bound of
/// [`place_part`]'s `bytes`, ((start + len).ceil to a page - (start's page)),
/// from `len` alone: `len` rounded up, plus the one page that the span's own
/// offset inside its first page can add. Charging the bound rather than the
/// exact count keeps the number a function of what [`gone`] is told (the
/// window's span, not its address) and can only ever hold the cap tighter,
/// never let it past: `len` bytes of `0x2000` starting at page offset
/// `0xf000` lock 128 KiB, and so do the 96 KiB this counts for `0x2000`.
fn charged(len: u64) -> u64 {
    len.max(1).next_multiple_of(K64 as u64) + K64 as u64
}

/// What [`place_for`] gave is gone. The bytes are the title's span, so they
/// are rounded here exactly as [`place_for`] rounded them: what a heap locked
/// and what it gives back must be the same number.
pub(crate) fn gone(window: bool, bytes: u64) {
    let at = if window { 0 } else { 2 };
    STATS[at].fetch_sub(1, Ordering::Relaxed);
    STATS[at + 1].fetch_sub(charged(bytes), Ordering::Relaxed);
}

/// What the startup self-check ([`check`]) found about this host: whether
/// memory handed to it as a heap is the memory its shaders read and write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Health {
    /// Not asked yet.
    Unknown,
    /// The GPU read the page the CPU wrote, and the CPU read the page the
    /// GPU wrote: title memory may be used in place.
    Sound,
    /// The host refused the heap or the buffer in it (no silent fallback
    /// yet, but nothing in place either).
    Refused,
    /// The heap and the buffer were taken and the bytes did not arrive: the
    /// driver fell back to memory of its own (vkd3d-proton does this
    /// without telling the app, `xcl-bin/notes/zero-copy.md` §2).
    Fallback,
    /// The check could not be made at all (no queue, no page to make it in).
    Unchecked,
}

/// What the self-check found, once, as a number (0 until it has run).
static HEALTH: AtomicU8 = AtomicU8::new(Health::Unknown as u8);
/// Asks turned down because the self-check's answer was not [`Health::Sound`].
static UNCHECKED: AtomicU64 = AtomicU64::new(0);

fn health_of(value: u8) -> Health {
    match value {
        1 => Health::Sound,
        2 => Health::Refused,
        3 => Health::Fallback,
        4 => Health::Unchecked,
        _ => Health::Unknown,
    }
}

/// What the check found from what the GPU answered: it read `reads` the
/// page the CPU wrote, and after it `wrote` the page the CPU read `writes`
/// at its own address.
fn arrived(reads: bool, writes: bool) -> Health {
    if reads && writes { Health::Sound } else { Health::Fallback }
}

/// Whether the run may use title memory in place: only a host whose check
/// said [`Health::Sound`]. A refusal, a silent fallback and a check that
/// could not be made all leave every copy a copy.
fn in_place(health: Health) -> bool {
    health == Health::Sound
}

/// `VICTORY_INPLACE_CANARY`: 1 (unset) on, 0 off. Off, a region over title
/// memory is kept on the startup self-check's answer alone, as before #144.
fn canary_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_INPLACE_CANARY").ok().is_none_or(|v| v.trim() != "0"))
}

/// The bytes a canary writes through the title's own address and reads back
/// through the GPU's copy of them. A page would be the surest canary and the
/// most intrusive: 64 bytes in the region's first cache line is past every
/// copy's first 16 by a wide margin, and all the title loses is the time the
/// round trip takes (its bytes are put back).
const CANARY: usize = 64;

/// What a canary found about the region it was made on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Canary {
    /// The GPU's copy of the region held the bytes written through the
    /// title's own address: the placement is the title's pages.
    Arrived,
    /// The GPU read memory of its own: the heap was taken and the import was
    /// refused (the silent fallback), so the region must not be drawn from.
    Elsewhere,
    /// No canary could be made (no queue, or a span too short for one):
    /// nothing was written and nothing was read.
    Unmade,
    /// The copy went to the GPU and its end is not known: the placement must
    /// not be released under it.
    Unfinished,
}

impl Canary {
    /// What this means for a placement: only [`Canary::Arrived`] is a yes.
    fn found(self) -> Health {
        match self {
            Canary::Arrived => Health::Sound,
            Canary::Elsewhere => Health::Fallback,
            Canary::Unmade | Canary::Unfinished => Health::Unchecked,
        }
    }
}

/// Whether the region [`place_part`] just placed is the title's own pages to
/// the GPU: a canary written at the title's address and read back through the
/// placed buffer ([`canary`]), asked once per run - vkd3d-proton imports host
/// memory by one path for every heap it makes, so the first region's answer
/// is every region's, and the region that answers "no" is turned down rather
/// than drawn from (`pablogonz12/Scarlight#144`).
fn title_pages_arrived(device: &ID3D12Device, placed: &Placed, at: u64, len: u64) -> Canary {
    static STATE: AtomicU8 = AtomicU8::new(Health::Unknown as u8);
    static LOCK: Mutex<()> = Mutex::new(());
    let known = health_of(STATE.load(Ordering::Relaxed));
    if known != Health::Unknown {
        return if in_place(known) { Canary::Arrived } else { Canary::Unmade };
    }
    if !canary_on() {
        return Canary::Arrived;
    }
    let _held = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let known = health_of(STATE.load(Ordering::Relaxed));
    if known != Health::Unknown {
        return if in_place(known) { Canary::Arrived } else { Canary::Unmade };
    }
    let found = canary(device, placed, at, len);
    // A submission whose end is not known is not an answer: it must not be
    // remembered as one (the next region asks again).
    if found != Canary::Unfinished {
        STATE.store(found.found() as u8, Ordering::Relaxed);
    }
    match found {
        Canary::Arrived => tracing::info!(
            "title memory in place: the canary at the title's own address ({at:#x}) came back through the GPU's placement: this region is the GPU's pages"
        ),
        found => tracing::warn!(
            placed = format_args!("{at:#x}"),
            "title memory in place: the canary at the title's own address never came back through the GPU's placement ({}): the import was refused and the host fell back to memory of its own - this region and every later one stay copies, and this is counted",
            health_name(found.found())
        ),
    }
    found
}

/// Writes a canary through the title's own address `at`, reads it back through
/// the GPU's copy of it (a 64-byte copy out of the placed buffer on a queue of
/// its own) and puts the title's bytes back. [`Canary::Arrived`] only if the
/// GPU's copy held the canary: a host that took the heap and then used memory
/// of its own answers [`Canary::Elsewhere`], whatever it said to the API.
///
/// The canary needs a 64-byte span in the title's memory the caller asked for
/// (`len`) and in the placed buffer: beyond either it would write into the
/// page after the title's memory or read past the buffer (a span too short is
/// [`Canary::Unmade`], and the region is a copy - a placement that short saves
/// nothing worth an access violation).
fn canary(device: &ID3D12Device, placed: &Placed, at: u64, len: u64) -> Canary {
    // SAFETY: a live buffer; the description is its own.
    let width = unsafe { placed.buffer.GetDesc() }.Width;
    let fits_buffer = placed.bias + CANARY as u64 <= width;
    if len < CANARY as u64 || !fits_buffer {
        return Canary::Unmade;
    }
    let Ok(mut rig) = Rig::new(device) else {
        return Canary::Unmade;
    };
    let mut sent = [0u8; CANARY];
    fill(sent.as_mut_ptr(), CANARY, 194);
    let mut kept = [0u8; CANARY];
    // SAFETY: `[at, at + len)` is private memory committed read-write
    // (`place_part`'s `private_whole`), and `len` is at least `CANARY`.
    unsafe {
        std::ptr::copy_nonoverlapping(at as *const u8, kept.as_mut_ptr(), CANARY);
        std::ptr::copy_nonoverlapping(sent.as_ptr(), at as *mut u8, CANARY);
    }
    let got = rig.fetch(&placed.buffer, placed.bias, CANARY);
    // SAFETY: as above; the title's bytes go back the way they came.
    unsafe { std::ptr::copy_nonoverlapping(kept.as_ptr(), at as *mut u8, CANARY) };
    match got {
        Ok(got) if got == sent => Canary::Arrived,
        Ok(_) => Canary::Elsewhere,
        // The copy was handed to the GPU: the readback stays with it
        // (`Rig::fetch`) and so must the placed buffer and its heap.
        Err(_) => Canary::Unfinished,
    }
}

/// The one line the self-check leaves in the log, whatever it found, so
/// that a run's log says whether title memory is in place or copied, and
/// why, before a single frame is drawn.
fn say_health(found: Health, detail: &str) {
    match found {
        Health::Sound => tracing::info!(
            "title memory in place: the host reads and writes our own pages (the self-check moved a page both ways, no flush and no map; {detail}): title memory is used in place"
        ),
        Health::Refused => tracing::warn!(
            "title memory in place: the host refused the import of a page of our own memory ({detail}): every copy stays a copy, and this is counted"
        ),
        Health::Fallback => tracing::warn!(
            "title memory in place: the host took the heap and the GPU used memory of its own (the self-check's bytes never arrived at our address): the import was refused and the driver fell back silently - every copy stays a copy, and this is counted"
        ),
        Health::Unchecked => tracing::warn!(
            "title memory in place: the self-check could not be made ({detail}): every copy stays a copy, and this is counted"
        ),
        Health::Unknown => {}
    }
}

/// Asks the host once, when its device is made (and before any title
/// memory is asked for), whether a host heap on our own pages really is
/// those pages. What vkd3d-proton does when the import fails is `vkAllocateMemory` of its own
/// memory, a `WARN` and `allocation->cpu_address = host_ptr`
/// (`libs/vkd3d/memory.c:1630`): the CPU goes on using the title's bytes
/// and the GPU does not, which is a black picture from frame 1 with every
/// draw still running (the belobug run). Nothing in the API says so, so the
/// only way to see it is to move bytes and look.
fn check(device: &ID3D12Device) -> Health {
    static LOCK: Mutex<()> = Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let known = health_of(HEALTH.load(Ordering::Relaxed));
    if known != Health::Unknown {
        return known;
    }
    let (found, detail) = ask(device);
    HEALTH.store(found as u8, Ordering::Relaxed);
    say_health(found, &detail);
    found
}

/// [`check`] for the host's startup, which has no use for the answer: the
/// line it logs and [`HEALTH`] (which [`place_for`] reads) are the whole of
/// it.
pub(crate) fn check_at_startup(device: &ID3D12Device) {
    check(device);
}

/// [`check`]'s work, on objects of its own: four 64 KiB pages of private
/// memory, opened as a part the way [`place_part`] opens the title's, and
/// the GPU asked to read what the CPU wrote there and to write what the CPU
/// then reads at its own address. `Health::Sound` is the only answer that
/// allows anything in place.
fn ask(device: &ID3D12Device) -> (Health, String) {
    let mut rig = match Rig::new(device) {
        Ok(rig) => rig,
        Err(e) => return (Health::Unchecked, format!("no queue of its own: {e}")),
    };
    // SAFETY: an allocation of the check's own, released before it returns.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), 4 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
        if base.is_null() {
            return (Health::Unchecked, "no private memory to make it in".to_owned());
        }
        // The check's own pages are made the way an import needs them: one
        // `VirtualAlloc`, 64 KiB aligned, one committed region of one
        // protection (`whole_allocation`). The title's pages are not always
        // that shape, which is why they go through `From::Part` instead.
        if !whole_allocation(base as usize, 4 * K64) {
            return (Health::Unchecked, "the check's own pages are not one allocation region".to_owned());
        }
        let (found, detail) = ask_page(&mut rig, device, base);
        if found != Health::Sound {
            // Not a sound host: the GPU's work on these pages may not have
            // finished (a failed signal or wait is exactly one of the ways
            // this is not sound), so the pages stay rather than being freed
            // under it (256 KiB, once, and nothing is used in place).
            return (found, detail);
        }
        let freed = mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) != 0;
        if !freed && found == Health::Sound {
            return (Health::Unchecked, "the check's own page could not be released".to_owned());
        }
        (found, detail)
    }
}

/// [`ask`] on `base`, the first of four pages of private memory of its own,
/// which is the least that can be committed whole. `base` is [`ask`]'s own
/// committed memory, of four pages.
fn ask_page(rig: &mut Rig, device: &ID3D12Device, base: *mut u8) -> (Health, String) {
    const SIZE: usize = K64;
    let heap = match open(device, From::Part(base as usize, SIZE)) {
        Ok(heap) => heap,
        Err(e) => return (Health::Refused, format!("the heap: {e}")),
    };
    let buffer = match place(device, &heap, 0, SIZE as u64) {
        Ok(buffer) => buffer,
        Err(e) => return (Health::Refused, format!("the buffer in it: {e}")),
    };
    fill(base, SIZE, 3);
    let reads = match rig.fetch(&buffer, 0, SIZE) {
        Ok(got) => same(base, &got),
        Err(e) => {
            // A submission whose end is not known: the heap and the buffer
            // stay with it (`std::mem::forget`) rather than being released
            // while the GPU may still read them.
            std::mem::forget(heap);
            std::mem::forget(buffer);
            return (Health::Fallback, format!("the GPU's read failed: {e}"));
        }
    };
    // A page's worth, so that nothing of the copy is the host's idea of a
    // minimum: the pattern differs from the read pass's.
    let mut sent = [0u8; K64];
    fill(sent.as_mut_ptr(), sent.len(), 200);
    if let Err(e) = rig.store(&buffer, 0, &sent) {
        std::mem::forget(heap);
        std::mem::forget(buffer);
        return (Health::Fallback, format!("the GPU's write failed: {e}"));
    }
    let writes = same(base, &sent);
    let found = arrived(reads, writes);
    if found != Health::Sound {
        return (
            found,
            if reads {
                "the GPU read our page but its write never reached our address"
            } else {
                "the GPU never read our address"
            }
            .to_owned(),
        );
    }
    // The strict opening (an allocation's own base, one region) beside the
    // part the title's buffers use: both are ours to make here, and a host
    // that refuses the first but takes the second is worth a word in the line.
    let strict = match open(device, From::Region(base as usize, 4 * K64)) {
        Ok(_) => "the whole-allocation opening takes these pages too",
        Err(_) => "but the whole-allocation opening refuses them",
    };
    (Health::Sound, strict.to_owned())
}

/// Title memory `[at, at + len)` is about to show something else, or the
/// title is about to decommit, free or protect it: what is in place there
/// is let go (the tables forget it; its heap goes when the last list and
/// slot that hold it do). Called for memory the runtime maps anew; for
/// the runtime to call before the title's own `VirtualFree` once it takes
/// that call over.
pub fn title_memory_freeing(at: usize, len: usize) {
    if STATS[0].load(Ordering::Relaxed) + STATS[2].load(Ordering::Relaxed) == 0 {
        return;
    }
    let gone = crate::shadow::in_place_let_go(at as u64, len as u64);
    STATS[7].fetch_add(gone, Ordering::Relaxed);
}

/// [`Health`] in words, for the line of what is in place.
fn health_name(found: Health) -> &'static str {
    match found {
        Health::Unknown => "not asked yet",
        Health::Sound => "our own pages, both ways",
        Health::Refused => "the host refused the import",
        Health::Fallback => "the host fell back to memory of its own",
        Health::Unchecked => "could not be made",
    }
}

/// The line of what is in place, every ten seconds while [`level`] is on.
fn say() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 10_000 || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        "title memory in place: {} windows {} MB, {} title buffers {} MB (no copy, no refresh, no copy-back); turned down: {} view memory or not private memory committed whole, \
         {} refused by the host, {} over VICTORY_INPLACE_MB; {} let go because their memory changed; {} refused because the canary never came back; the self-check says {}, {} asks turned down for it",
        s[0],
        s[1] >> 20,
        s[2],
        s[3] >> 20,
        s[4],
        s[5],
        s[6],
        s[7],
        s[8],
        health_name(health_of(HEALTH.load(Ordering::Relaxed))),
        UNCHECKED.load(Ordering::Relaxed)
    );
}

/// What a heap is opened on.
pub(crate) enum From {
    /// A whole `VirtualAlloc` allocation: its base address and its bytes. It
    /// is the strict opening, and it takes nothing else - vkd3d-proton
    /// refuses (`device.c:7603-7627`) unless `VirtualQuery` says the address
    /// *is* the allocation's base, the range begins at that base and every
    /// page of it has one protection, i.e. one region. A view of a section
    /// from `MapViewOfFile` is not such a base, which is why the section path
    /// ([`From::Section`], `OpenExistingHeapFromFileMapping`, `device.c:7637`,
    /// which maps again and unmaps before the GPU uses the heap) always fails;
    /// it is [`From::Part`] or memory of our own that can be imported
    /// (`xcl-bin/notes/linux-stack-fixes.md` 2.1).
    Region(usize, usize),
    /// A committed part of one: address and bytes. `ID3D12Device13` takes a
    /// part, so the title's own pages do not have to be the allocation's base
    /// (or one region) for this.
    Part(usize, usize),
    /// A section made with `SEC_COMMIT`, by its handle.
    Section(usize),
}

/// Opens a heap on memory the process has.
pub(crate) fn open(device: &ID3D12Device, from: From) -> windows::core::Result<ID3D12Heap> {
    // SAFETY: the calls take an address or a handle and check both
    // themselves; a refusal is an error code.
    unsafe {
        match from {
            From::Region(at, bytes) => {
                // The host's own rule, checked here so a refusal is ours and
                // not a silent `E_INVALIDARG`: an allocation's base, one
                // region, one protection.
                if !whole_allocation(at, bytes) {
                    return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
                }
                device.cast::<ID3D12Device3>()?.OpenExistingHeapFromAddress(at as *const c_void)
            }
            From::Part(at, bytes) => device.cast::<ID3D12Device13>()?.OpenExistingHeapFromAddress1(at as *const c_void, bytes),
            From::Section(handle) => device.cast::<ID3D12Device3>()?.OpenExistingHeapFromFileMapping(HANDLE(handle as *mut c_void)),
        }
    }
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

/// A buffer of `size` bytes at `offset` (a multiple of 64 KiB) of an
/// opened heap, which shaders may read and write, in the common state.
pub(crate) fn place(device: &ID3D12Device, heap: &ID3D12Heap, offset: u64, size: u64) -> windows::core::Result<ID3D12Resource> {
    let mut placed: Option<ID3D12Resource> = None;
    // The heap is cross-adapter shared, so the buffer must say so too.
    let flags = D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // SAFETY: plain creation in a live heap.
    unsafe { device.CreatePlacedResource(heap, offset, &buffer_desc(size, flags), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)? };
    placed.ok_or_else(windows::core::Error::empty)
}

// The title's views of its physical pages, as the runtime maps them: the
// census needs them to say which view memory is one run of a section (one
// placed buffer could show it) and which is scattered pages.

struct Views {
    /// By address: bytes, the section's handle, the offset in it.
    views: BTreeMap<usize, (usize, usize, u64)>,
    /// Sections by handle: bytes, and whether made committed.
    sections: Vec<(usize, u64, bool)>,
}

static VIEWS: RwLock<Views> = RwLock::new(Views { views: BTreeMap::new(), sections: Vec::new() });

/// The views, for reading. Every in-place ask asks [`cover`], and the level-2
/// default asks it for every window and every title buffer, while the runtime
/// writes a view only when it maps one: a read lock keeps those asks off each
/// other instead of queueing them behind one mutex.
fn views_read() -> RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

/// The views, for the runtime to change them.
fn views_write() -> RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// The runtime made a section of `bytes` for physical pages; `committed`
/// if with `SEC_COMMIT` (only such a section can be opened as a heap).
pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    let mut v = views_write();
    v.sections.retain(|s| s.0 != section);
    v.sections.push((section, bytes, committed));
}

/// The runtime mapped `[at, at + len)` as a view of `section` from
/// `offset` on. Call after [`crate::title_memory_remapped`] of the range.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    views_write().views.insert(at, (len, section, offset));
}

/// What `[at, at + len)` showed is gone ([`crate::title_memory_remapped`]):
/// the views that touch it are forgotten (the runtime tells again those it
/// maps anew).
pub(crate) fn views_gone(at: usize, len: usize) {
    let mut v = views_write();
    if v.views.is_empty() {
        return;
    }
    let end = at.saturating_add(len);
    let first = v.views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at).map(|(base, _)| *base);
    let gone: Vec<usize> = first.into_iter().chain(v.views.range(at.saturating_add(1)..end).map(|(base, _)| *base)).collect();
    for base in gone {
        v.views.remove(&base);
    }
}

/// How the views cover `[at, at + len)`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Cover {
    /// No view holds its first byte.
    None,
    /// Views hold a part of it only.
    Holes,
    /// Views hold all of it, from more than one place.
    Scattered,
    /// All of it is one run of one section: the section and the offset of
    /// `at` in it.
    Run(usize, u64),
}

fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at) else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one { Cover::Run(first.0, first.1) } else { Cover::Scattered }
}

/// [`Cover`] of `[at, at + len)` by the title's views as they are now.
pub(crate) fn cover(at: usize, len: usize) -> Cover {
    cover_in(&views_read().views, at, len)
}

/// Whether `[at, at + len)` is private memory, all committed and
/// read-write: what [`From::Part`] takes. One question to Windows (it
/// walks the page tables to the end of the pages that share the first
/// one's state: a millisecond in the title's big blocks, so ask when a
/// copy is made, not when it is used).
pub(crate) fn private_whole(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}

/// Whether `[at, at + len)` is a whole allocation as `OpenExistingHeapFromAddress`
/// takes it (vkd3d-proton `device.c:7603-7627`: the address must be the
/// allocation's own base, the range must begin there, and every page must have
/// one protection, i.e. be one region). Memory we make ourselves is
/// `VirtualAlloc`ed this way, one region per import, which is the fix
/// `xcl-bin/notes/linux-stack-fixes.md` 2.1 asks for; a view of a section is
/// not, and is never handed to this opening.
fn whole_allocation(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.AllocationBase as usize == at
        && info.BaseAddress as usize == at
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}

fn hr<T>(result: &windows::core::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("REFUSED {:#010x} ({})", e.code().0, e.message().trim()),
    }
}

/// A queue, a list and a fence of the probe's own on `device`: nothing of
/// the host's submissions is touched.
struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    done: u64,
    event: HANDLE,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our event.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

impl Rig {
    fn new(device: &ID3D12Device) -> windows::core::Result<Rig> {
        // SAFETY: plain creation of objects of our own.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Rig { device: device.clone(), queue, allocator, list, fence, done: 0, event })
        }
    }

    /// Records with `record`, runs it and waits (ten seconds at most).
    fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
        // SAFETY: our own list and queue, one run at a time.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            record(&self.list);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done)?;
            self.fence.SetEventOnCompletion(self.done, self.event)?;
            if WaitForSingleObject(self.event, 10_000).0 != 0 {
                return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
            }
            self.device.GetDeviceRemovedReason()
        }
    }

    fn committed(&self, kind: D3D12_HEAP_TYPE, size: u64, state: D3D12_RESOURCE_STATES) -> windows::core::Result<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE), state, None, &mut buffer)?
        };
        buffer.ok_or_else(windows::core::Error::empty)
    }

    /// What the GPU reads of `size` bytes of `resource` from `offset`.
    fn fetch(&mut self, resource: &ID3D12Resource, offset: u64, size: usize) -> windows::core::Result<Vec<u8>> {
        let back = self.committed(D3D12_HEAP_TYPE_READBACK, size as u64, D3D12_RESOURCE_STATE_COPY_DEST)?;
        // SAFETY: two live buffers, the range inside both.
        // A run that does not complete leaves `back` the GPU's, not ours: it
        // stays (a `forget`, 64 KiB once) rather than being released under a
        // copy whose end is unknown.
        if let Err(e) = self.run(|list| unsafe { list.CopyBufferRegion(&back, 0, resource, offset, size as u64) }) {
            std::mem::forget(back);
            return Err(e);
        }
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds `size` bytes until unmapped.
        unsafe {
            back.Map(0, None, Some(&mut data))?;
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
            back.Unmap(0, None);
            Ok(bytes)
        }
    }

    /// The GPU writes `bytes` at `offset` of `resource` (a copy).
    fn store(&mut self, resource: &ID3D12Resource, offset: u64, bytes: &[u8]) -> windows::core::Result<()> {
        let up = self.committed(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds the buffer's bytes until unmapped.
        unsafe {
            up.Map(0, None, Some(&mut data))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
            up.Unmap(0, None);
        }
        // SAFETY: two live buffers, the range inside both.
        // As in `fetch`: a submission whose end is not known keeps its source.
        let run = self.run(|list| unsafe { list.CopyBufferRegion(resource, offset, &up, 0, bytes.len() as u64) });
        if run.is_err() {
            std::mem::forget(up);
        }
        run
    }

    /// A compute shader stores, through a root unordered-access view at
    /// `gpu`, the sum of the buffer's first two words in its third.
    fn shade(&mut self, gpu: u64) -> windows::core::Result<()> {
        const SOURCE: &str = "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n[RootSignature(RS)]\n[numthreads(1, 1, 1)]\nvoid main() { b.Store(8, b.Load(0) + b.Load(4)); }\n";
        let bytes = crate::dxc::compile(SOURCE, "main", "cs_5_1")?;
        // SAFETY: the root signature is inside the shader; the description
        // points at live bytes and never releases its pointer.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, &bytes)?;
            let pso = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            })?;
            (root, pso)
        };
        // SAFETY: an open list of ours; the view names a live buffer.
        self.run(|list| unsafe {
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pso);
            list.SetComputeRootUnorderedAccessView(0, gpu);
            list.Dispatch(1, 1, 1);
        })
    }

    /// Milliseconds the card takes to copy `bytes` of `from` into its own
    /// memory, the best of three.
    fn copy_ms(&mut self, from: &ID3D12Resource, bytes: u64) -> windows::core::Result<f64> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut to: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE), D3D12_RESOURCE_STATE_COMMON, None, &mut to)?
        };
        let to = to.ok_or_else(windows::core::Error::empty)?;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            // SAFETY: two live buffers of `bytes` bytes or more.
            self.run(|list| unsafe { list.CopyBufferRegion(&to, 0, from, 0, bytes) })?;
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(best)
    }
}

fn fill(at: *mut u8, len: usize, seed: u8) {
    for i in 0..len {
        // SAFETY: the caller's own committed memory of `len` bytes.
        unsafe { at.add(i).write((i as u8).wrapping_mul(31).wrapping_add(seed)) };
    }
}

fn same(at: *const u8, bytes: &[u8]) -> bool {
    // SAFETY: the caller's own committed memory of that length.
    unsafe { std::slice::from_raw_parts(at, bytes.len()) == bytes }
}

/// The process's working set in MB.
fn working_set_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut counters = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut counters, counters.cb) };
    counters.WorkingSetSize as u64 >> 20
}

/// What the probe found. The lines are the whole account; the flags are
/// what the stages build on.
#[derive(Default)]
pub(crate) struct Probe {
    pub(crate) lines: Vec<String>,
    /// A part of private, write-watched memory opens, a buffer in it shows
    /// the memory's bytes to the GPU, also those the CPU wrote afterwards.
    pub(crate) part_reads: bool,
    /// What the GPU writes there (a copy, and a shader through unordered
    /// access) the CPU reads at the memory's address.
    pub(crate) part_writes: bool,
    /// The same two for a committed section, through a view of it.
    pub(crate) section_reads: bool,
    pub(crate) section_writes: bool,
    /// The section's heap outlives the view: the GPU's write with no view
    /// mapped shows in a view mapped afterwards.
    pub(crate) section_outlives_views: bool,
    /// A section made with `SEC_RESERVE` (as `xmem` makes the page pool)
    /// was refused.
    pub(crate) reserved_section_refused: bool,
}

/// Tries the openings on memory of the probe's own, made the way `xmem`
/// makes title memory, with a queue of its own on `device`; `memory`
/// gives the card's memory account (`Host::video_memory`) where there is
/// one. Nothing of the title's is touched. `mb` is the size of the cost
/// measurement (0 leaves it out).
pub(crate) fn probe(device: &ID3D12Device, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) -> Probe {
    let mut p = Probe::default();
    let mut rig = match Rig::new(device) {
        Ok(rig) => rig,
        Err(e) => {
            p.lines.push(format!("no queue of its own: {e}"));
            return p;
        }
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: a plain query that fills our structure.
    let _ = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32) };
    p.lines.push(format!(
        "the device: ID3D12Device3 {}, ID3D12Device13 {} (parts of an allocation); cross-adapter row-major textures {}; heap tier {}",
        device.cast::<ID3D12Device3>().is_ok(),
        device.cast::<ID3D12Device13>().is_ok(),
        options.CrossAdapterRowMajorTextureSupported.as_bool(),
        options.ResourceHeapTier.0
    ));
    // SAFETY: memory of the probe's own, made and released here; every
    // address used lies inside it.
    unsafe {
        private(&mut p, &mut rig);
        section(&mut p, &mut rig);
        if mb > 0 {
            cost(&mut p, &mut rig, memory, mb);
        }
    }
    p.lines.push(format!(
        "VERDICT: private memory in place: reads {}, writes {}; the page pool's section in place: reads {}, writes {}, whatever the views do {}; \
         a reserved section is refused {} (the pool must be made committed to be opened)",
        p.part_reads, p.part_writes, p.section_reads, p.section_writes, p.section_outlives_views, p.reserved_section_refused
    ));
    p
}

fn describe(heap: &ID3D12Heap) -> String {
    // SAFETY: a plain query.
    let d = unsafe { heap.GetDesc() };
    format!(
        "heap of {} KiB: type {} (4 = custom), CPU pages {} (3 = write-back), pool {} (1 = the PC's memory), alignment {}, flags {:#x} (0x1 shared, 0x20 cross-adapter, 0x200 write watch, 0x400 shader atomics)",
        d.SizeInBytes >> 10,
        d.Properties.Type.0,
        d.Properties.CPUPageProperty.0,
        d.Properties.MemoryPoolPreference.0,
        d.Alignment,
        d.Flags.0
    )
}

/// Private memory, reserved as `xmem` reserves the title's heaps
/// (committed, read-write, write-watched): a part of it opened.
unsafe fn private(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 64 * K64;
    // SAFETY (the whole function): the allocation below, `SIZE` bytes.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if base.is_null() {
            p.lines.push("private memory: none to try with".to_owned());
            return;
        }
        // The part: 16 pages from the 16th on.
        let (at, bytes) = (base.add(16 * K64), 16 * K64);
        let whole = open(&rig.device, From::Region(base as usize, SIZE));
        p.lines.push(format!("private memory, the whole allocation (OpenExistingHeapFromAddress): {}", hr(&whole)));
        drop(whole);
        let opened = open(&rig.device, From::Part(at as usize, bytes));
        p.lines.push(format!("private memory, a part of it (OpenExistingHeapFromAddress1): {}", hr(&opened)));
        if let Ok(heap) = &opened {
            p.lines.push(format!("  {}", describe(heap)));
            let placed = place(&rig.device, heap, K64 as u64, 4 * K64 as u64);
            p.lines.push(format!("  a buffer placed in it (cross-adapter, unordered access): {}", hr(&placed)));
            if let Ok(buffer) = &placed {
                let memory = at.add(K64);
                fill(memory, 4 * K64, 1);
                let first = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                fill(memory, 4 * K64, 77);
                let second = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                p.part_reads = first && second;
                p.lines.push(format!("  the GPU reads the memory's bytes: {first}; and those the CPU wrote afterwards, with no call in between: {second}"));
                mem::ResetWriteWatch(base.cast(), SIZE);
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x5a).collect();
                let copied = rig.store(buffer, 4096, &new).is_ok() && same(memory.add(4096), &new);
                let (mut found, mut count, mut granularity) = ([0usize; 64], 64usize, 0u32);
                let watched = mem::GetWriteWatch(0, base.cast(), SIZE, found.as_mut_ptr().cast(), &mut count, &mut granularity) == 0;
                memory.cast::<[u32; 3]>().write([40, 2, 0]);
                let shaded = rig.shade(buffer.GetGPUVirtualAddress());
                let shader = shaded.is_ok() && memory.cast::<[u32; 3]>().read()[2] == 42;
                p.part_writes = copied && shader;
                p.lines.push(format!(
                    "  the CPU reads at the memory's address what the GPU wrote: by a copy {copied}; by a shader through unordered access {shader} ({}); \
                     write watching saw {} of the copy's 16 pages{}",
                    hr(&shaded),
                    if watched { count } else { 0 },
                    if watched { "" } else { " (the asking failed)" }
                ));
            }
            // What the title can still do with the pages while the heap stands.
            let mut old = 0;
            let protect_in = mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
            if protect_in {
                mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READWRITE, &mut old);
            }
            let decommit_in = mem::VirtualFree(at.add(9 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            let decommit_out = mem::VirtualFree(base.add(40 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            p.lines.push(format!(
                "  with the heap standing, the title's own calls: read-only inside the part {protect_in}, decommit inside {decommit_in}, decommit outside {decommit_out} \
                 (false inside means the heap must go before the title frees or guards those pages)"
            ));
            drop(placed);
        }
        drop(opened);
        let freed = mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) != 0;
        p.lines.push(format!("  released after the heap went: {freed}"));
    }
}

/// A section and a view of it in a placeholder, as `xmem` maps physical
/// pages: reserved (as it is made today) and committed.
unsafe fn section(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 32 * K64;
    // SAFETY (the whole function): the sections, placeholder and views
    // made below.
    unsafe {
        let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let reserved = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_RESERVE, 0, SIZE as u32, std::ptr::null());
        if !reserved.is_null() {
            let opened = open(&rig.device, From::Section(reserved as usize));
            p.reserved_section_refused = opened.is_err();
            p.lines.push(format!("a section made reserved, as the page pool is today (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
            drop(opened);
            windows_sys::Win32::Foundation::CloseHandle(reserved);
        }
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, SIZE as u32, std::ptr::null());
        let hole = mem::VirtualAlloc2(process, std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER, mem::PAGE_NOACCESS, std::ptr::null_mut(), 0).cast::<u8>();
        if handle.is_null() || hole.is_null() {
            p.lines.push("a committed section: none to try with".to_owned());
            return;
        }
        // Pages 4..12 of the section at the placeholder's start.
        mem::VirtualFree(hole.cast(), 8 * K64, mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER);
        let view = mem::MapViewOfFile3(handle, process, hole.cast(), (4 * K64) as u64, 8 * K64, mem::MEM_REPLACE_PLACEHOLDER, mem::PAGE_READWRITE, std::ptr::null_mut(), 0);
        let opened = open(&rig.device, From::Section(handle as usize));
        p.lines.push(format!("a section made committed (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
        let mut unmapped = false;
        if let (Ok(heap), false) = (&opened, view.Value.is_null()) {
            p.lines.push(format!("  {}", describe(heap)));
            // The section's page 5 is the view's second page.
            let placed = place(&rig.device, heap, 5 * K64 as u64, 2 * K64 as u64);
            p.lines.push(format!("  a buffer placed at the section's offset of a view's pages: {}", hr(&placed)));
            if let Ok(buffer) = &placed {
                let memory = hole.add(K64);
                fill(memory, 2 * K64, 3);
                p.section_reads = rig.fetch(buffer, 0, 2 * K64).is_ok_and(|got| same(memory, &got));
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x33).collect();
                p.section_writes = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // The guard of `watch.rs`: the view read-only.
                let mut old = 0;
                let guarded = mem::VirtualProtect(memory.cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x44).collect();
                let through = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // The view goes; the GPU writes; another view shows it.
                unmapped = mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x55).collect();
                let stored = rig.store(buffer, 0, &new).is_ok();
                let other = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, (5 * K64) as u32, K64);
                p.section_outlives_views = unmapped && stored && !other.Value.is_null() && same(other.Value.cast(), &new);
                p.lines.push(format!(
                    "  the GPU reads the view's bytes {}; the CPU reads through the view what the GPU wrote {}; the view made read-only under the heap {guarded}, \
                     and the GPU writes it all the same {through}; the view unmapped under the heap {unmapped}, the GPU's write then shows in a view mapped afterwards {}",
                    p.section_reads, p.section_writes, p.section_outlives_views
                ));
                if !other.Value.is_null() {
                    mem::UnmapViewOfFile(other);
                }
            }
        }
        drop(opened);
        if !unmapped && !view.Value.is_null() {
            mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER);
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
        // The placeholder was split in two: each half is released.
        mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
        mem::VirtualFree(hole.add(8 * K64).cast(), 0, mem::MEM_RELEASE);
    }
}

/// What opening costs, on `mb` MB each of private memory never touched
/// and of a committed section never touched: the time, the working set,
/// the card's account of the PC's memory shared with it; and how fast the
/// card copies out of such a heap against a shared buffer of today's kind.
unsafe fn cost(p: &mut Probe, rig: &mut Rig, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) {
    let bytes = mb << 20;
    let shared = || memory().map_or(0, |m| m[3] >> 20);
    let ms = |speed: Option<f64>| speed.map_or("?".to_owned(), |ms| format!("{ms:.1}"));
    // SAFETY (the whole function): the allocation and the section below.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if !base.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Part(base as usize, bytes));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of private memory never touched: {}; opened in {took:.1} ms; working set {set} -> {} MB; shared with the card {account} -> {} MB; \
                 the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes as u64 >> 32) as u32, bytes as u32, std::ptr::null());
        if !handle.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Section(handle as usize));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let (set_opened, account_opened) = (working_set_mb(), shared());
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of a committed section never touched: {}; opened in {took:.1} ms; working set {set} -> {set_opened} -> after the card read all of it {} MB; \
                 shared with the card {account} -> {account_opened} -> {} MB; the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        // Today's kind of host copy, for the comparison.
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = rig.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(bytes as u64, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        );
        if let (Ok(()), Some(buffer)) = (made, buffer) {
            let speed = rig.copy_ms(&buffer, bytes as u64).ok();
            p.lines.push(format!("cost, for comparison: the card copies {mb} MB out of a shared buffer of today's kind in {} ms", ms(speed)));
        }
    }
}

// The census: which of the title memory that has host copies today could
// be opened, and by which opening.

/// Counts and bytes by kind, in the order of [`KINDS`].
#[derive(Default, Clone, Copy)]
struct Tally([(u64, u64); 6]);

const KINDS: [&str; 6] = [
    "private, committed whole (a part can be opened)",
    "view memory, one run of a section (one placed buffer)",
    "view memory, scattered pages",
    "view memory with unmapped pages",
    "private but not committed whole, or guarded",
    "other",
];

fn kind_of(at: usize, len: usize, known: &mut Vec<(usize, usize)>) -> usize {
    match cover(at, len) {
        Cover::Run(..) => return 1,
        Cover::Scattered => return 2,
        Cover::Holes => return 3,
        Cover::None => {}
    }
    let end = at + len;
    if known.iter().any(|&(from, to)| from <= at && end <= to) {
        return 0;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 5;
    }
    let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    if info.Type == mem::MEM_PRIVATE && info.State == mem::MEM_COMMIT && info.Protect == mem::PAGE_READWRITE {
        known.push((info.BaseAddress as usize, reach));
        return if reach >= end { 0 } else { 4 };
    }
    match info.Type {
        mem::MEM_PRIVATE => 4,
        mem::MEM_MAPPED => 3,
        _ => 5,
    }
}

/// The census lines for `spans` (start, bytes, whether a window outside
/// buffers).
fn census_of(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let mut known = Vec::new();
    let (mut buffers, mut windows) = (Tally::default(), Tally::default());
    for &(start, len, window) in spans {
        let kind = kind_of(start as usize, len as usize, &mut known);
        let tally = if window { &mut windows } else { &mut buffers };
        tally.0[kind].0 += 1;
        tally.0[kind].1 += len;
    }
    let mut lines = Vec::new();
    for (name, tally) in [("title buffers", buffers), ("windows outside buffers", windows)] {
        let all = tally.0.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        lines.push(format!("census, {name}: {} of {} MB", all.0, all.1 >> 20));
        for (kind, &(n, bytes)) in KINDS.iter().zip(&tally.0) {
            if n > 0 {
                lines.push(format!("census,   {kind}: {n} of {} MB", bytes >> 20));
            }
        }
    }
    let v = views_read();
    let (count, bytes) = (v.views.len(), v.views.values().map(|view| view.0 as u64).sum::<u64>());
    let pages = v.views.values().filter(|view| view.0 == K64).count();
    let big = v.views.values().map(|view| view.0).max().unwrap_or(0);
    lines.push(format!(
        "census, the title's views of physical pages: {count} of {} MB; {pages} of them one 64 KiB page; the longest {} MB",
        bytes >> 20,
        big >> 20
    ));
    for &(_, bytes, committed) in &v.sections {
        lines.push(format!("census, a section of {} MB, {}", bytes >> 20, section_verdict(committed)));
    }
    lines
}

/// What the census says of a section's memory, by whether it was made with
/// `SEC_COMMIT`. Not "it can be opened as one heap": measured on this machine
/// (`xcl-bin/notes/zero-copy.md` 6), a committed section cannot be. Its handle
/// and its view both open, and the import under them fails (RADV refuses a
/// file-backed host pointer, `VkAllocateMemory` giving `hr 0x8007000e`), after
/// which vkd3d-proton quietly hands the buffers memory of its own: the API
/// says yes, the GPU reads and writes somewhere else, and 267,386,880 of
/// 268,435,456 bytes read back at the title's own address are wrong. Only
/// private memory committed whole is placed, which is what [`From`] has.
fn section_verdict(committed: bool) -> &'static str {
    if committed {
        "committed: not openable under Wine (the import of a section's pages is refused; measured, notes/zero-copy.md 6)"
    } else {
        "reserved: refused as a heap until it is made with SEC_COMMIT, and to no use then (the same refusal)"
    }
}

/// With [`probe_level`] 2: one part of the title's own private memory
/// (the start of the biggest such title buffer, 4 MiB at most) is opened
/// for a moment, a buffer placed, 64 KiB read by the GPU and compared
/// with what the CPU reads there (the title may be writing it meanwhile:
/// the count of equal bytes is given), and all of it released. While the
/// heap stands (milliseconds) a `VirtualFree` of the title's on those
/// pages would fail.
fn real_part(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let Some(host) = crate::host::host() else {
        return Vec::new();
    };
    let mut known = Vec::new();
    let found = spans
        .iter()
        .filter(|s| !s.2 && s.1 >= 2 * K64 as u64)
        .filter(|s| kind_of(s.0 as usize, s.1 as usize, &mut known) == 0)
        .max_by_key(|s| s.1);
    let Some(&(start, len, _)) = found else {
        return vec!["the title's memory: no title buffer in private, committed memory to try".to_owned()];
    };
    let at = (start as usize).next_multiple_of(K64);
    let bytes = ((start + len) as usize).saturating_sub(at).min(4 << 20) & !(K64 - 1);
    if bytes == 0 || !private_whole(at, bytes) {
        return vec!["the title's memory: the buffer found holds no whole 64 KiB page that is still committed".to_owned()];
    }
    let mut lines = Vec::new();
    let (set, started) = (working_set_mb(), std::time::Instant::now());
    let opened = open(&host.device, From::Part(at, bytes));
    lines.push(format!(
        "the title's memory, {} KiB at {at:#x} (of the title buffer {start:#x}+{len:#x}): {}; in {:.1} ms; working set {set} -> {} MB",
        bytes >> 10,
        hr(&opened),
        started.elapsed().as_secs_f64() * 1000.0,
        working_set_mb()
    ));
    if let Ok(heap) = &opened {
        let placed = place(&host.device, heap, 0, bytes as u64);
        lines.push(format!("  a buffer placed over it: {}", hr(&placed)));
        if let (Ok(buffer), Ok(mut rig)) = (&placed, Rig::new(&host.device)) {
            let mut before = vec![0u8; K64];
            crate::host::copy_mapped(before.as_mut_ptr(), at as *const u8, K64, false);
            let got = rig.fetch(buffer, 0, K64);
            let mut after = vec![0u8; K64];
            crate::host::copy_mapped(after.as_mut_ptr(), at as *const u8, K64, false);
            match got {
                Ok(got) => {
                    let equal = (0..K64).filter(|&i| got[i] == before[i] || got[i] == after[i]).count();
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    lines.push(format!(
                        "  the GPU read 64 KiB of the title's own bytes: {equal} of {K64} equal to what the CPU read just before or just after ({zeros} of them zero)"
                    ));
                }
                Err(e) => lines.push(format!("  the GPU's read failed: {e}")),
            }
        }
    }
    lines
}

/// Called once a second by the thread that submits, before a submission
/// (`host::sweep`): with [`probe_level`], the probe at the first call, the
/// census (on a thread of its own: it asks Windows about every copy) 30
/// and 150 seconds later.
pub(crate) fn tick() {
    if level() > 0 {
        // The self-check (the host device makes it too, when it is made;
        // this catches a device made before the level was known): one line
        // on whether title memory can be used in place at all.
        if let Some(host) = crate::host::host() {
            check(&host.device);
        }
        say();
    }
    if probe_level() == 0 {
        return;
    }
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static DONE: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms().max(1);
    let started = match STARTED.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => {
            let Some(host) = crate::host::host() else {
                return;
            };
            let mb = std::env::var("VICTORY_INPLACE_PROBE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(64usize);
            let took = std::time::Instant::now();
            let probe = probe(&host.device, &|| host.video_memory(), mb.min(1024));
            for line in &probe.lines {
                tracing::info!("in place probe: {line}");
            }
            tracing::info!("in place probe: took {:.0} ms", took.elapsed().as_secs_f64() * 1000.0);
            now
        }
        Err(started) => started,
    };
    let due = match DONE.load(Ordering::Relaxed) {
        0 => now >= started + 30_000,
        1 => now >= started + 150_000,
        _ => false,
    };
    if due {
        let which = DONE.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let spans = crate::shadow::spans();
            for line in census_of(&spans) {
                tracing::info!("in place {line}");
            }
            if which == 0 && probe_level() >= 2 {
                for line in real_part(&spans) {
                    tracing::info!("in place probe: {line}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Views as the runtime tells them: a range inside one view, or over
    /// views that continue each other in one section, is one run; over
    /// views from elsewhere it is scattered; over a gap it has holes.
    #[test]
    fn views_say_what_one_placed_buffer_could_show() {
        let mut views = BTreeMap::new();
        let at = 0x7_0000_0000usize;
        views.insert(at, (4 * K64, 9, 16 * K64 as u64));
        // Continues the first in the section.
        views.insert(at + 4 * K64, (2 * K64, 9, 20 * K64 as u64));
        // Follows in address, from elsewhere in the section.
        views.insert(at + 6 * K64, (K64, 9, 3 * K64 as u64));
        // After a gap of one page.
        views.insert(at + 8 * K64, (K64, 9, 4 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 2 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 5 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at, 7 * K64), Cover::Scattered);
        assert_eq!(cover_in(&views, at + 6 * K64, 3 * K64), Cover::Holes);
        assert_eq!(cover_in(&views, at + 7 * K64, K64), Cover::None);
        assert_eq!(cover_in(&views, at - K64, 2 * K64), Cover::None);
    }

    /// What the runtime forgets when memory is mapped anew.
    #[test]
    fn remapped_memory_forgets_its_views() {
        let at = 0x7_4000_0000usize;
        title_memory_view(at, 4 * K64, 77, 0);
        title_memory_view(at + 4 * K64, K64, 77, 4 * K64 as u64);
        assert_eq!(cover(at, 5 * K64), Cover::Run(77, 0));
        // One page inside the first view goes: the whole view is forgotten
        // (the runtime maps and tells again what stays of it).
        views_gone(at + K64, K64);
        assert_eq!(cover(at, K64), Cover::None);
        assert_eq!(cover(at + 4 * K64, K64), Cover::Run(77, 4 * K64 as u64));
        views_gone(at + 4 * K64, K64);
        assert_eq!(cover(at + 4 * K64, K64), Cover::None);
    }

    /// The switch: unset is on (the measured-safe class only), `0` off,
    /// `1` windows outside buffers only; a value that is not a number keeps
    /// the default rather than silently turning it off.
    #[test]
    fn the_switch_is_on_by_default() {
        assert_eq!(level_from(None), 2);
        assert_eq!(level_from(Some("")), 2);
        assert_eq!(level_from(Some("0")), 0);
        assert_eq!(level_from(Some("1")), 1);
        assert_eq!(level_from(Some(" 2 ")), 2);
        assert_eq!(level_from(Some("on")), 2);
    }

    /// The self-check decides the whole run: only "the bytes arrived both
    /// ways" keeps title memory in place; a refusal, a silent fallback and a
    /// check that could not be made leave every copy a copy.
    #[test]
    fn the_census_does_not_promise_a_section_heap() {
        // #1117 places buffers over committed sections; under Wine the import
        // is refused and vkd3d silently substitutes its own memory, so the
        // census must never read as an invitation to place over one.
        for committed in [true, false] {
            let said = section_verdict(committed);
            assert!(!said.contains("can be opened"), "{said}");
            assert!(said.contains("not openable") || said.contains("no use"), "{said}");
        }
    }

    #[test]
    fn only_a_host_that_reads_our_pages_keeps_memory_in_place() {
        assert!(in_place(Health::Sound));
        assert!(!in_place(Health::Unknown));
        assert!(!in_place(Health::Refused));
        assert!(!in_place(Health::Fallback));
        assert!(!in_place(Health::Unchecked));
        assert_eq!(arrived(true, true), Health::Sound);
        assert_eq!(arrived(true, false), Health::Fallback, "the GPU read our page but wrote elsewhere");
        assert_eq!(arrived(false, true), Health::Fallback, "the GPU wrote at our address but read elsewhere");
        assert_eq!(arrived(false, false), Health::Fallback);
        // The stored numbers are the ones `place_for` reads back.
        for found in [Health::Unknown, Health::Sound, Health::Refused, Health::Fallback, Health::Unchecked] {
            assert_eq!(health_of(found as u8), found);
        }
        assert_eq!(health_of(9), Health::Unknown, "a number nobody wrote is not asked yet");
    }

    /// A region is kept in place only on the canary's yes: the GPU reading
    /// memory of its own (the silent fallback, `vkd3d-proton#3522`), a canary
    /// that could not be made and one whose submission never finished are all
    /// copies (§144).
    #[test]
    fn only_a_region_the_canary_reached_is_kept_in_place() {
        assert_eq!(Canary::Arrived.found(), Health::Sound);
        assert!(in_place(Canary::Arrived.found()), "the bytes came back");
        assert_eq!(Canary::Elsewhere.found(), Health::Fallback);
        assert!(!in_place(Canary::Elsewhere.found()), "the GPU read memory of its own: a copy instead");
        assert_eq!(Canary::Unmade.found(), Health::Unchecked);
        assert!(!in_place(Canary::Unmade.found()), "a canary that could not be made is not a yes");
        assert_eq!(Canary::Unfinished.found(), Health::Unchecked);
        assert!(!in_place(Canary::Unfinished.found()), "a submission whose end is not known is not a yes");
    }

    /// The cap counts the pages a heap locks, not the title's span, and what
    /// it gives back is the same number: seventeen 4 KiB spans are 1,088 KiB
    /// of imports under a 1 MiB cap, not the 68 KiB their lengths add up to.
    #[test]
    fn the_cap_is_charged_the_pages_that_are_locked() {
        // Never zero for a span place_part would import at least one page for.
        assert_eq!(charged(0), 2 * K64 as u64);
        assert!(charged(4 * 1024) >= K64 as u64);
        // A span that crosses a page line locks one page more than its own
        // rounded length: 0x2000 from page offset 0xf000 is 128 KiB, and the
        // charge is at least that.
        let (start, len) = (0xf000u64, 0x2000u64);
        let pages = (start + len).next_multiple_of(K64 as u64) - (start & !(K64 as u64 - 1));
        assert_eq!(pages, 2 * K64 as u64);
        assert!(charged(len) >= pages, "the charge is an upper bound of what place_part imports");
        // What a heap locked and what gone() gives back are the same number.
        assert_eq!(charged(len) - charged(len), 0);
        // And the seventeen 4 KiB spans of a 1 MiB cap are over it.
        assert!(charged(4 * 1024) * 17 > 1 << 20);
    }

    /// The strict opening takes one shape only (vkd3d-proton
    /// `device.c:7603-7627`): the allocation's own base, one committed region
    /// of one protection. Our own pages are made that way; a part of them, or
    /// a range that spans a second protection, is not.
    #[test]
    fn only_an_allocations_own_base_is_a_whole_allocation() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            assert!(whole_allocation(base as usize, 4 * K64), "a VirtualAlloc's own base, one region");
            assert!(!whole_allocation(base as usize + K64, 2 * K64), "an offset inside it is a part, not its base");
            let mut old = 0;
            assert_ne!(mem::VirtualProtect(base.add(K64).cast(), K64, mem::PAGE_READONLY, &mut old), 0);
            assert!(!whole_allocation(base as usize, 4 * K64), "a page with a protection of its own makes a second region");
            mem::VirtualProtect(base.add(K64).cast(), K64, mem::PAGE_READWRITE, &mut old);
            assert!(whole_allocation(base as usize, 4 * K64));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// Only private memory committed whole, read-write, is taken for a part.
    #[test]
    fn only_private_memory_committed_whole_is_a_part() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE, mem::PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert!(!private_whole(base as usize, K64), "reserved only");
            mem::VirtualAlloc(base.cast(), 4 * K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(private_whole(base as usize, 4 * K64));
            assert!(private_whole(base as usize + K64, 2 * K64));
            assert!(!private_whole(base as usize, 5 * K64), "reaches into the reserved half");
            let mut old = 0;
            mem::VirtualProtect(base.cast(), K64, mem::PAGE_READONLY, &mut old);
            assert!(!private_whole(base as usize, 2 * K64), "a guarded page");
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// The device these tests run on (`VICTORY_TEST_ADAPTER`): WARP where DXGI
    /// has it, the card otherwise. `skipped: no device` when there is none.
    fn device() -> Option<ID3D12Device> {
        crate::test_device::device(windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0)
    }

    /// The probe, whole, on the test device: the same calls the card gets. The
    /// runtime's and the kernel's rules hold there; the card's driver is what a
    /// run with `VICTORY_INPLACE_PROBE=1` adds.
    #[test]
    fn probe_opens_title_like_memory_and_the_gpu_sees_its_bytes() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };

        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        let probe = probe(&device, &|| None, 4);
        for line in &probe.lines {
            eprintln!("in place probe: {line}");
        }
        assert!(probe.part_reads, "the GPU reads private memory in place");
        assert!(probe.part_writes, "GPU writes land in private memory");
        // A section through a view is not a promise of a card's driver:
        // vkd3d-proton maps the section again and unmaps it before the GPU
        // uses the heap (`device.c:7637`), and where that import is refused
        // it falls back without a word (`vkd3d-proton#3522`), so the page
        // pool cannot be opened that way there (module docs, pablogonz12/
        // Scarlight#144). What the canary makes of it - and that a region it
        // does not find is never kept in place - is asserted in
        // `a_canary_says_whether_a_section_through_a_view_is_the_gpus_pages`.
        eprintln!(
            "in place probe: a committed section in place: reads {} writes {}, outlives views {}",
            probe.section_reads, probe.section_writes, probe.section_outlives_views
        );
        assert!(probe.reserved_section_refused, "a reserved section is refused");
    }

    /// A committed section through a view: the canary is what the GPU really
    /// reads of the title's own address, the only answer that may keep a
    /// region in place is "the bytes came back", and the title's bytes are put
    /// back afterwards. Two hosts answer this two ways, and both are asserted:
    /// WARP maps the section itself, so the GPU reads the view's bytes and the
    /// region may be kept (acceptance); RADV with vkd3d-proton has the import
    /// refused and falls back to memory of its own
    /// (`HansKristian-Work/vkd3d-proton#3522`), so the canary finds nothing
    /// and the region is turned down (§144).
    #[test]
    fn a_canary_says_whether_a_section_through_a_view_is_the_gpus_pages() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        // SAFETY (the whole test): the section, the placeholder and the view
        // made below, of sizes this test owns.
        unsafe {
            let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
            let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, (32 * K64) as u32, std::ptr::null());
            assert!(!handle.is_null(), "a committed section to try with");
            let hole = mem::VirtualAlloc2(process, std::ptr::null(), 32 * K64, mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER, mem::PAGE_NOACCESS, std::ptr::null_mut(), 0).cast::<u8>();
            assert!(!hole.is_null(), "a placeholder to map a view into");
            // Pages 4..12 of the section at the placeholder's start; the
            // section's page 5 is the view's second page, at `hole + K64`.
            assert_ne!(mem::VirtualFree(hole.cast(), 8 * K64, mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER), 0);
            let view = mem::MapViewOfFile3(handle, process, hole.cast(), (4 * K64) as u64, 8 * K64, mem::MEM_REPLACE_PLACEHOLDER, mem::PAGE_READWRITE, std::ptr::null_mut(), 0);
            assert!(!view.Value.is_null(), "a view of the section's pages");
            let heap = open(&device, From::Section(handle as usize)).expect("the section opens as a heap");
            let buffer = place(&device, &heap, 5 * K64 as u64, 2 * K64 as u64).expect("a buffer placed at the section's page 5");
            let memory = hole.add(K64);
            let placed = Placed { buffer: buffer.clone(), heap: heap.clone(), gpu: buffer.GetGPUVirtualAddress(), bias: 0 };
            // What the GPU reads of the view with no canary in it.
            let mut ours = [0u8; K64];
            fill(ours.as_mut_ptr(), K64, 3);
            std::ptr::copy_nonoverlapping(ours.as_ptr(), memory, K64);
            let mut rig = Rig::new(&device).expect("a queue of the test's own");
            let reads = rig.fetch(&buffer, 0, K64).is_ok_and(|got| same(memory, &got));
            // The canary over the same placement, at the title's own address.
            let found = canary(&device, &placed, memory as u64, K64 as u64);
            let kept = in_place(found.found());
            eprintln!(
                "in place canary: a committed section through a view, on {}: the GPU reads the view's bytes {reads}, the canary answers {found:?}: {}",
                if crate::test_device::is_software(&device) { "the software adapter" } else { "the card" },
                if kept { "kept in place" } else { "turned down, a copy instead" }
            );
            assert_eq!(found, if reads { Canary::Arrived } else { Canary::Elsewhere }, "the canary is what the GPU really reads of the title's address");
            assert_eq!(kept, reads, "a region is kept in place only where the GPU reads the title's own bytes");
            assert!(same(memory, &ours[..CANARY]), "the canary put the title's bytes back");
            std::mem::forget(placed);
            drop((buffer, heap));
            assert_ne!(mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER), 0);
            windows_sys::Win32::Foundation::CloseHandle(handle);
            // The placeholder was split in two: each half is released.
            mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
            mem::VirtualFree(hole.add(8 * K64).cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// The canary over memory the title allocates itself: the bytes written at
    /// the title's address come back through the GPU's placement, so this
    /// region may be used in place (the "acceptance where it works" half of
    /// §144, beside the refusal the section's answer gives on RADV).
    #[test]
    fn the_canary_finds_our_own_pages_through_a_placement() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            fill(base, 2 * K64, 11);
            let mut theirs = [0u8; CANARY];
            std::ptr::copy_nonoverlapping(base, theirs.as_mut_ptr(), CANARY);
            let placed = place_part(&device, base as u64, 2 * K64 as u64).expect("a part of private memory is placed");
            let found = canary(&device, &placed, base as u64, 2 * K64 as u64);
            assert_eq!(found, Canary::Arrived, "the GPU reads the canary at the title's own address");
            assert_eq!(found.found(), Health::Sound);
            assert!(same(base, &theirs), "the canary put the title's bytes back");
            // A span shorter than the canary is not touched at all: it is a
            // copy, and the bytes after it are nobody's to write.
            let short = canary(&device, &placed, base as u64 + 2 * K64 as u64 - 16, 16);
            assert_eq!(short, Canary::Unmade, "a 16-byte span is left alone");
            drop(placed);
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// Stage 1's own call on the software adapter: a title buffer that
    /// begins and ends off the 64 KiB lines is placed over its pages, the
    /// GPU reads the buffer's bytes at the offset the views use, and its
    /// write lands at the title's address. Memory that is not committed
    /// whole is turned down without asking the host.
    #[test]
    fn a_title_buffer_in_place_shows_its_bytes_at_its_offset() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };

        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
            let (start, len) = (base as u64 + K64 as u64 + 0x1230, 2 * K64 as u64 + 5);
            let placed = place_part(&device, start, len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(placed.bias, 0x1230);
            assert_eq!(placed.gpu, placed.buffer.GetGPUVirtualAddress() + 0x1230);
            assert_eq!(placed.buffer.GetDesc().Width, 3 * K64 as u64, "the pages that hold the buffer");
            let mut rig = Rig::new(&device).unwrap();
            fill(start as *mut u8, len as usize, 7);
            let got = rig.fetch(&placed.buffer, placed.bias, len as usize).unwrap();
            assert!(same(start as *const u8, &got), "the GPU reads the title buffer's bytes");
            rig.store(&placed.buffer, placed.bias + 16, &[0xa5; 64]).unwrap();
            assert!(same((start + 16) as *const u8, &[0xa5; 64]), "the GPU's write is at the title's address");
            drop(placed);
            // Half of it decommitted: turned down, the host not asked.
            assert_ne!(mem::VirtualFree(base.add(2 * K64).cast(), K64, mem::MEM_DECOMMIT), 0);
            assert!(matches!(place_part(&device, start, len), Err(None)));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// The self-check on the test device: the same calls the card gets, so the
    /// answer there is that our own page is the GPU's page too.
    #[test]
    fn the_self_check_reads_and_writes_our_own_page() {
        let _once = crate::warptest::lock();
        let Some(device) = device() else { return };

        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("skipped: no device: no ID3D12Device13, a part of an allocation cannot be opened");
            return;
        }
        let (found, detail) = ask(&device);
        assert_eq!(found, Health::Sound, "the test device reads and writes a page we hand it: {detail}");
    }

    /// The census names each kind of memory.
    #[test]
    fn census_tells_private_memory_from_views() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as u64;
            let view = 0x7_8000_0000u64;
            title_memory_view(view as usize, 2 * K64, 5, 0);
            title_memory_view(view as usize + 2 * K64, K64, 5, 9 * K64 as u64);
            let lines = census_of(&[(base, 2 * K64 as u64, false), (base + 4 * K64 as u64, K64 as u64, true), (view, K64 as u64, true), (view, 3 * K64 as u64, false)]);
            let text = lines.join("\n");
            assert!(text.contains("census, title buffers: 2 of"), "{text}");
            assert!(text.contains("census,   private, committed whole (a part can be opened): 1 of"), "{text}");
            assert!(text.contains("census,   view memory, scattered pages: 1 of"), "{text}");
            assert!(text.contains("census,   view memory, one run of a section (one placed buffer): 1 of"), "{text}");
            views_gone(view as usize, 3 * K64);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }
}
