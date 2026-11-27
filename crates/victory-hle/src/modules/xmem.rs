//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let _stall = victory_gpu::stall_site!("xmem::XMemVirtualAlloc").enter(format_args!(""));
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if placeholder && base.is_null() && !at.is_null() {
        // The GPU side answers its placeholder questions from this table
        // (`victory_gpu::inplace`): nothing else maps or unmaps inside one.
        victory_gpu::title_memory_placeholder(at as usize, size);
    }
    if !at.is_null() && kind & MEM_COMMIT != 0 && protect == PAGE_READWRITE {
        // Committed private memory the GPU may use: the arena's registry
        // (`VICTORY_ARENA=1`, off by default; a no-op when off).
        victory_gpu::title_memory_committed(at as usize, size);
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let _stall = victory_gpu::stall_site!("xmem::XMemVirtualQuery").enter(format_args!(""));
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    let _stall = victory_gpu::stall_site!("xmem::XMemAlloc")
        .enter(format_args!("size={size} attributes={attributes:#x}"));
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    let _stall = victory_gpu::stall_site!("xmem::XMemFree").enter(format_args!(
        "address={address:p} attributes={attributes:#x}"
    ));
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    let _stall = victory_gpu::stall_site!("xmem::XMemAllocDefault").enter(format_args!(""));
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    let _stall = victory_gpu::stall_site!("xmem::XMemFreeDefault").enter(format_args!(""));
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    let _stall = victory_gpu::stall_site!("xmem::XMemIsToolsMemAvailable").enter(format_args!(""));
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section(HANDLE);

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // A null handle is the non-owning placeholder the tests build (and
        // `CloseHandle(NULL)` raises under a debugger, so it must not run).
        if self.0.is_null() {
            return;
        }
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.0) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[cfg(any(feature = "diagnostics", test))]
#[derive(Default)]
struct AliasCensus {
    // Weak references keep section identities unique without keeping handles open.
    pages: HashMap<(usize, usize), (std::sync::Weak<Section>, u32, bool)>,
    ever: usize,
    made: usize,
    destroyed: usize,
}

#[cfg(any(feature = "diagnostics", test))]
impl AliasCensus {
    fn change(&mut self, slice: &Slice, len: usize, add: bool) {
        let section = Arc::as_ptr(&slice.section) as usize;
        for index in slice.index..slice.index + len.div_ceil(PAGE) {
            let entry = self
                .pages
                .entry((section, index))
                .or_insert_with(|| (Arc::downgrade(&slice.section), 0, false));
            if add {
                entry.1 += 1;
                if entry.1 == 2 && !entry.2 {
                    entry.2 = true;
                    self.ever += 1;
                }
            } else {
                entry.1 -= 1;
            }
        }
        if add {
            self.made += 1;
        } else {
            self.destroyed += 1;
        }
    }

    fn counts(&self) -> [usize; 3] {
        let mut counts = [0; 3];
        for (_, views, _) in self.pages.values() {
            if *views != 0 {
                counts[(*views as usize).min(3) - 1] += 1;
            }
        }
        counts
    }
}

#[cfg(test)]
#[test]
fn physical_alias_census_counts_live_views() {
    let slice = Slice {
        section: Arc::new(Section(std::ptr::null_mut())),
        index: 0,
    };
    let mut census = AliasCensus::default();
    census.change(&slice, PAGE, true);
    census.change(&slice, PAGE, true);
    assert_eq!(census.counts(), [0, 1, 0]);
    assert_eq!(census.ever, 1);
    census.change(&slice, PAGE, false);
    assert_eq!(census.counts(), [1, 0, 0]);
    assert_eq!(census.ever, 1);
}

#[cfg(feature = "diagnostics")]
static CALL_TIMES: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

#[cfg(feature = "diagnostics")]
struct CallTimer {
    start: std::time::Instant,
    kind: usize,
}

#[cfg(feature = "diagnostics")]
impl CallTimer {
    fn new(kind: usize) -> Self {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            unsafe extern "C" {
                fn atexit(callback: extern "C" fn()) -> i32;
            }
            extern "C" fn report() {
                log_census(true);
            }
            // SAFETY: callback has static lifetime and the required C ABI.
            unsafe { atexit(report) };
        });
        Self {
            start: std::time::Instant::now(),
            kind,
        }
    }
}

#[cfg(feature = "diagnostics")]
impl Drop for CallTimer {
    fn drop(&mut self) {
        let ns = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        CALL_TIMES[self.kind * 2].fetch_add(ns, Ordering::Relaxed);
        CALL_TIMES[self.kind * 2 + 1].fetch_max(ns, Ordering::Relaxed);
        log_census(false);
    }
}

#[cfg(feature = "diagnostics")]
fn log_census(force: bool) {
    use std::time::{Duration, Instant};
    static LAST: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = last.elapsed();
    if !force && elapsed < Duration::from_secs(10) {
        return;
    }
    *last = Instant::now();
    let mut physical = physical();
    let census = &mut physical.census;
    let [one, two, many] = census.counts();
    let mapped = one + two + many;
    let made = std::mem::take(&mut census.made);
    let destroyed = std::mem::take(&mut census.destroyed);
    let times = CALL_TIMES
        .each_ref()
        .map(|v| v.swap(0, Ordering::Relaxed) as f64 / 1_000_000.0);
    tracing::info!(
        "physical pages census: mapped={mapped} ({:.2} MiB), 1_view={one} ({:.2} MiB), 2_views={two} ({:.2} MiB), 3plus_views={many} ({:.2} MiB), ever_aliased={}, views_made={made}, views_destroyed={destroyed}, interval_s={:.2}, map_ms={:.3} max_ms={:.3}, unmap_ms={:.3} max_ms={:.3}, scatter_ms={:.3} max_ms={:.3}",
        mapped as f64 * PAGE as f64 / MIB as f64,
        one as f64 * PAGE as f64 / MIB as f64,
        two as f64 * PAGE as f64 / MIB as f64,
        many as f64 * PAGE as f64 / MIB as f64,
        census.ever,
        elapsed.as_secs_f64(),
        times[0],
        times[1],
        times[2],
        times[3],
        times[4],
        times[5]
    );
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    /// Page numbers whose pages the title freed; handed out again before
    /// `next_page` grows (a long session must not walk off the number space).
    free_pages: Vec<usize>,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Secondary index by `(section_ptr, start_page, base)` -> view length in
    /// bytes, so `title_ranges` is O(log N + k) instead of scanning all views.
    by_section: BTreeMap<(usize, usize, usize), usize>,
    /// Largest view length in pages ever inserted for each section.
    max_pages: HashMap<usize, usize>,
    #[cfg(feature = "diagnostics")]
    census: AliasCensus,
}

/// Whether freed page numbers are handed out again (`VICTORY_PAGE_RECYCLE=0`
/// keeps the old ever-growing counter).
fn page_recycle() -> bool {
    #[cfg(test)]
    if let Some(on) = forced_recycle() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PAGE_RECYCLE").as_deref() != Ok("0"))
}

// Tests set the policy they exercise on their own thread, so a run with
// `VICTORY_PAGE_RECYCLE=0` still tests the reuse (see `force_recycle`).
#[cfg(test)]
thread_local! {
    static FORCED_RECYCLE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The policy a test asked for on this thread, if any.
#[cfg(test)]
fn forced_recycle() -> Option<bool> {
    FORCED_RECYCLE.with(std::cell::Cell::get)
}

/// Tests: whether this thread's allocator reuses freed page numbers.
#[cfg(test)]
fn force_recycle(value: Option<bool>) {
    FORCED_RECYCLE.with(|c| c.set(value));
}

impl Physical {
    /// Reserves `numbers.len()` page numbers for `section`, reusing freed
    /// ones first. False when the number space would run past the title's
    /// 21-bit `number >> 4` field.
    fn allocate(&mut self, section: &Arc<Section>, numbers: &mut [MaybeUninit<usize>]) -> bool {
        let reuse = if page_recycle() {
            self.free_pages.len()
        } else {
            0
        };
        let fresh = numbers.len().saturating_sub(reuse);
        if fresh > (PAGE_NUMBER_LIMIT - self.next_page) / FRAMES {
            return false;
        }
        for (index, out) in numbers.iter_mut().enumerate() {
            let recycled = if page_recycle() {
                self.free_pages.pop()
            } else {
                None
            };
            let number = recycled.unwrap_or_else(|| {
                let number = self.next_page;
                self.next_page += FRAMES;
                number
            });
            self.pages.insert(
                number,
                Slice {
                    section: section.clone(),
                    index,
                },
            );
            out.write(number);
        }
        true
    }

    /// Drops a page and keeps its number for reuse.
    fn free(&mut self, number: usize) {
        if self.pages.remove(&number).is_some() {
            self.free_pages.push(number);
        }
    }

    fn insert_view(&mut self, base: usize, len: usize, slice: Slice) {
        let sec = Arc::as_ptr(&slice.section) as usize;
        let idx = slice.index;
        #[cfg(feature = "diagnostics")]
        self.census.change(&slice, len, true);
        if let Some((old_len, old)) = self.views.insert(base, (len, slice)) {
            #[cfg(feature = "diagnostics")]
            self.census.change(&old, old_len, false);
            #[cfg(not(feature = "diagnostics"))]
            let _ = old_len;
            let old_sec = Arc::as_ptr(&old.section) as usize;
            self.by_section.remove(&(old_sec, old.index, base));
        }
        self.by_section.insert((sec, idx, base), len);
        let pages = len.div_ceil(PAGE);
        let max = self.max_pages.entry(sec).or_insert(0);
        if pages > *max {
            *max = pages;
        }
    }

    fn remove_view(&mut self, at: usize) -> Option<(usize, Slice)> {
        let (size, slice) = self.views.remove(&at)?;
        #[cfg(feature = "diagnostics")]
        self.census.change(&slice, size, false);
        let sec = Arc::as_ptr(&slice.section) as usize;
        self.by_section.remove(&(sec, slice.index, at));
        Some((size, slice))
    }

    /// Title-address ranges showing bytes `[within, within + len)` of the
    /// `n`-page run starting at `slice`, via the O(log N + k) section index.
    fn ranges_for(
        &self,
        slice: &Slice,
        n: usize,
        within: usize,
        len: usize,
    ) -> Vec<(usize, usize)> {
        let sec = Arc::as_ptr(&slice.section) as usize;
        let Some(&max_pages) = self.max_pages.get(&sec) else {
            return Vec::new();
        };
        let end = within.saturating_add(len);
        let run_end_page = slice.index.saturating_add(n);
        let lo_page = slice.index.saturating_sub(max_pages);
        let mut out = Vec::new();
        for (&(_, view_index, at), &view_len) in self
            .by_section
            .range((sec, lo_page, 0)..(sec, run_end_page, 0))
        {
            let head = (view_index as i64 - slice.index as i64).saturating_mul(PAGE as i64);
            let lo = head.max(within as i64);
            let hi = head
                .saturating_add(view_len as i64)
                .min(end as i64)
                .min((n as i64).saturating_mul(PAGE as i64));
            if hi <= lo {
                continue;
            }
            out.push((at + (lo - head) as usize, (hi - lo) as usize));
        }
        out
    }
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: FIRST_PAGE_NUMBER,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    let _stall =
        victory_gpu::stall_site!("xmem::XMemAllocatePhysicalPages").enter(format_args!(""));
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section(handle));
    let mut physical = physical();
    // SAFETY: the title passes room for `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts_mut(pages.cast::<MaybeUninit<usize>>(), n) };
    if !physical.allocate(&section, numbers) {
        tracing::warn!(n, "XMemAllocatePhysicalPages: page numbers exhausted");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;
/// One past the largest page number the title's 21-bit `number >> 4` holds.
const PAGE_NUMBER_LIMIT: usize = (1 << 21) * FRAMES;
/// The first number handed out (0 stays unused).
const FIRST_PAGE_NUMBER: usize = 0x10_0000;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    let _stall = victory_gpu::stall_site!("xmem::XMemFreePhysicalPages").enter(format_args!(""));
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.free(unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.0,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len };
        commit(at.Value as usize, len).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether every page under `[offset, offset + len)` is known (a write that
/// fails with them known is a host failure).
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    runs(pages, offset, len).is_some()
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        // Before the write, never after: the guards are on the title's views
        // of the same pages, and told afterwards they save what we wrote
        // (Marvisak/VIctory #2503).
        mark_written(
            slice,
            *n,
            within,
            (data.len() - done).min(n * PAGE - within),
        );
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    true
}

/// Tells the GPU side's write watch that bytes
/// `[within, within + len)` of a run of `n` pages from `slice` are about to
/// change through a view of our own: it saves an armed page's bytes before
/// they change (`watch::writing`), so this has to come before the write.
/// Called afterwards, it saves the bytes just written, and
/// `copy_back_marked` then puts its stale copy back over them (streamed
/// textures lost after a DirectStorage burst; Marvisak/VIctory #2503).
fn mark_written(slice: &Slice, n: usize, within: usize, len: usize) {
    for (at, bytes) in title_ranges(slice, n, within, len) {
        victory_gpu::kernel_will_write(at, bytes);
    }
}

/// The title's own views over bytes `[within, within + len)` of a run of `n`
/// pages from `slice`, as addresses and lengths: the guards a write through
/// our own view must open first.
fn title_ranges(slice: &Slice, n: usize, within: usize, len: usize) -> Vec<(usize, usize)> {
    let physical = physical();
    physical.ranges_for(slice, n, within, len)
}

/// Views a worker keeps for its next page destination: a load sends
/// thousands of tiny requests, and each new view costs a map, a commit and
/// an unmap (which flushes every core's TLB, and under Wine is a wineserver
/// request): 0.2 ms a request when measured, more than the read itself
/// (perf-ledger.md). A few per thread, since a tile is asked for again.
struct Cached {
    /// (section, index of the run's first page, byte in the first page, len).
    key: (usize, usize, usize, usize),
    view: View,
}

const CACHE_SLOTS: usize = 16;
/// The most bytes one thread keeps mapped between requests.
const CACHE_BYTES: usize = 4 << 20;

thread_local! {
    static CACHED: std::cell::RefCell<Vec<Option<Cached>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a worker keeps its views mapped between requests
/// (`VICTORY_DSTORAGE_VIEW_CACHE_MB`, 0 for off: the bench's before).
fn cache_bytes() -> usize {
    static BYTES: LazyLock<usize> = LazyLock::new(|| {
        std::env::var("VICTORY_DSTORAGE_VIEW_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(CACHE_BYTES, |mb| mb << 20)
    });
    *BYTES
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy. The view of a cached window goes back to its worker's
/// cache instead of being unmapped, so the next request in the same pages
/// costs no map at all.
pub(crate) struct PageWindow {
    view: Option<View>,
    key: (usize, usize, usize, usize),
    /// The cache slot the view goes back to, and whose old view it replaces.
    slot: usize,
    within: usize,
    len: usize,
    /// Mapped for this window alone: unmapped when it drops.
    own: bool,
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        let view = self.view.as_mut().expect("a window's view until it drops");
        &mut view.bytes()[within..within + len]
    }
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        let Some(view) = self.view.take() else {
            return;
        };
        let (key, slot) = (self.key, self.slot);
        if self.own {
            drop(view);
            return;
        }
        CACHED.with(|cached| {
            let mut cached = cached.borrow_mut();
            if slot >= cached.len() {
                cached.resize_with(slot + 1, || None);
            }
            if let Some(old) = cached[slot].replace(Cached { key, view }) {
                drop(old);
            }
        });
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let within = offset % PAGE;
    let key = (
        Arc::as_ptr(&slice.section) as usize,
        slice.index,
        within,
        len,
    );
    // Before the write, as in `write_physical` (Marvisak/VIctory #2503).
    mark_written(slice, *n, within, len);
    let budget = cache_bytes();
    let hit = CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.len() < CACHE_SLOTS {
            cached.resize_with(CACHE_SLOTS, || None);
        }
        let slot = cached
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.key == key))?;
        let taken = cached[slot].take()?;
        Some((slot, taken.view))
    });
    if let Some((slot, view)) = hit {
        return Some(PageWindow {
            view: Some(view),
            key,
            slot,
            within,
            len,
            own: false,
        });
    }
    let view = View::map(slice, *n)?;
    // A window longer than the budget (a big tile) is this request's alone:
    // keeping it would hold that much of the title's memory.
    if budget == 0 || len > budget {
        return Some(PageWindow {
            view: Some(view),
            key,
            slot: 0,
            within,
            len,
            own: true,
        });
    }
    let slot = CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.len() < CACHE_SLOTS {
            cached.resize_with(CACHE_SLOTS, || None);
        }
        let held: usize = cached.iter().flatten().map(|c| c.view.len).sum();
        match cached.iter().position(Option::is_none) {
            Some(slot) if held + view.len <= budget => slot,
            _ => NEXT.with(|next| {
                let slot = next.get() % cached.len().max(1);
                next.set(slot + 1);
                slot
            }),
        }
    });
    Some(PageWindow {
        view: Some(view),
        key,
        slot,
        within,
        len,
        own: false,
    })
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    #[cfg(feature = "diagnostics")]
    let _timer = CallTimer::new(if pages.is_null() { 1 } else { 0 });
    let _stall = victory_gpu::stall_site!("xmem::XMemMapPhysicalPages").enter(format_args!(""));
    let base = address as usize;
    let mut physical = physical();
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section, but never
        // past the placeholder the run starts in: the title splits its pool
        // into placeholders of its own, and consecutive pages say nothing
        // about where one ends. A call across two is refused, and its range
        // has already been unmapped. Ask the host where that placeholder
        // ends (Marvisak/VIctory #1264's triage), before the scan, so it
        // covers only the pages this call maps (a page per placeholder
        // would otherwise be searched once more on every page).
        let at = base + i * PAGE;
        // 0: not a reserved placeholder, so the run is as long as it comes.
        let room = if clipping() {
            placeholder_room(at) / PAGE
        } else {
            0
        };
        let most = if room > 0 { room.min(n - i) } else { n - i };
        let run = 1 + pages[i + 1..i + most]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        // The scan stopped at the room and pages went on: clipped there.
        if run == most && most < n - i {
            let clipped = CLIPPED_RUNS.fetch_add(1, Ordering::Relaxed) + 1;
            if clipped == 1 || clipped.is_multiple_of(1024) {
                tracing::info!(
                    address = format_args!("{at:#x}"),
                    pages = run,
                    clipped,
                    "a mapping run stops at the end of its placeholder"
                );
            } else {
                tracing::debug!(
                    address = format_args!("{at:#x}"),
                    pages = run,
                    clipped,
                    "a mapping run stops at the end of its placeholder"
                );
            }
        }
        if !map(&mut physical, at, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    #[cfg(feature = "diagnostics")]
    let _timer = CallTimer::new(2);
    let _stall =
        victory_gpu::stall_site!("xmem::XMemMapPhysicalPagesScatter").enter(format_args!(""));
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// How many bytes from `base` to the end of the placeholder it lies in: the
/// most one call can map as one view there. 0 when the host does not report
/// a reserved placeholder at `base` (an address already shown, or the title's
/// own memory), so the caller maps page by page as before.
fn placeholder_room(base: usize) -> usize {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `VirtualQueryEx` fills `info` for the region holding `base`.
    let got = unsafe {
        VirtualQueryEx(
            GetCurrentProcess(),
            base as *const c_void,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 || info.State != MEM_RESERVE {
        return 0;
    }
    (info.BaseAddress as usize)
        .saturating_add(info.RegionSize)
        .saturating_sub(base)
}

/// Batched mapping runs that stopped at the end of their placeholder
/// (`VICTORY_MAP_CLIP`; reported in the counters line of `victory-gpu`).
static CLIPPED_RUNS: AtomicUsize = AtomicUsize::new(0);

/// Whether a batched run is clipped to the placeholder it starts in;
/// `VICTORY_MAP_CLIP=0` maps it as it comes (a title that splits its
/// placeholders would get the refused batch of before).
fn clip_from(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

fn clipping() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| clip_from(std::env::var("VICTORY_MAP_CLIP").ok().as_deref()));
    *ON
}

#[cfg(test)]
pub(crate) fn clipped_runs() -> usize {
    CLIPPED_RUNS.load(Ordering::Relaxed)
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut physical = physical();
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.0,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(
        base,
        len,
        slice.section.0 as usize,
        (slice.index * PAGE) as u64,
    );
    physical.insert_view(base, len, slice);
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(len, &error, queried.then_some(&info)));
    } else {
        tracing::warn!(len, %error, "committing physical pages failed");
    }
    false
}

fn commit_limit_message(
    len: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of physical pages: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.remove_view(at) else {
            continue;
        };
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    let _stall =
        victory_gpu::stall_site!("xmem::XMemGetWorkingSetStatistics").enter(format_args!(""));
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    let _stall = victory_gpu::stall_site!("xmem::XMemTransferMemory").enter(format_args!(""));
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, &error, Some(&info));
        assert!(report.contains("4194304 bytes"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, &error, None);
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A scatter of pages at consecutive addresses maps them in one call:
    /// each address shows its own page, pages asked for in a different order
    /// still land where they are asked for, a zero page in the middle unmaps
    /// only its own address, and an unmap run goes in one call
    /// (idea from ivall/xcl#254).
    #[test]
    fn scatter_maps_runs_of_consecutive_addresses() {
        let pages = allocate(3);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        for k in 0..3 {
            assert!(write_physical(&pages[k..], 0, &[b'0' + k as u8]));
        }

        // Three pages, three consecutive addresses: one call.
        let addresses = [at(0), at(1), at(2)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, pages.as_ptr()),
            1
        );
        for k in 0..3 {
            assert_eq!(
                // SAFETY: mapped read-write.
                unsafe { at(k).cast::<u8>().read() },
                b'0' + k as u8,
                "page {k} at its address"
            );
        }

        // Ask for them out of order: each still lands where it is named.
        let addresses = [at(2), at(0)];
        let swapped = [pages[0], pages[2]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, swapped.as_ptr()),
            1
        );
        assert_eq!(
            // SAFETY: mapped read-write.
            unsafe { at(2).cast::<u8>().read() },
            b'0'
        );
        assert_eq!(
            // SAFETY: mapped read-write.
            unsafe { at(0).cast::<u8>().read() },
            b'2'
        );

        // A zero page ends the run it is in: its address is unmapped, the
        // page after it still maps.
        let addresses = [at(1), at(2)];
        let mixed = [0usize, pages[1]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, mixed.as_ptr()),
            1
        );
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(2).cast::<u8>().read() }, b'1');

        // Three consecutive addresses without pages unmap as one run.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, std::ptr::null()),
            1
        );
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        assert_eq!(
            XMemMapPhysicalPagesScatter([at(0)].as_ptr(), 1, std::ptr::null()),
            1
        );
        assert_eq!(query(at(0)).host.State, MEM_RESERVE);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// Titles split one pool of 64 KiB placeholders into per-page ones, and
    /// the pages of a batch say nothing about where their placeholder ends:
    /// a run of consecutive slices that starts in one placeholder and ends
    /// outside it must be clipped to the room the host reports, not handed
    /// to the host whole (which is refused, after the range was unmapped).
    #[test]
    fn a_run_of_mappings_stops_at_the_end_of_its_placeholder() {
        let pages = allocate(2);
        for (k, page) in pages.iter().enumerate() {
            assert!(write_physical(
                std::slice::from_ref(page),
                0,
                &[b'0' + k as u8]
            ));
        }
        // Two 64 KiB pages reserved as one pool, then split in two
        // placeholders of their own, as the title hands its pool out.
        let pool = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!pool.is_null());
        let second = pool.wrapping_byte_add(PAGE);
        // SAFETY: our own placeholder; the range freed stays a placeholder.
        assert_ne!(
            unsafe { VirtualFree(second, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,) },
            0
        );
        for at in [pool, second] {
            assert_eq!(
                query(at).host.State,
                MEM_RESERVE,
                "a placeholder of its own"
            );
        }
        // Two consecutive slices of one section, the run crossing from the
        // first placeholder into the second.
        let before = clipped_runs();
        assert_eq!(XMemMapPhysicalPages(pool, 2, pages.as_ptr()), pool);
        assert_eq!(clipped_runs(), before + 1, "one run clipped, none refused");
        // Each page shows its own bytes: the clip maps the first, then the
        // rest in the placeholder that follows.
        for (k, at) in [pool, second].into_iter().enumerate() {
            assert_eq!(
                // SAFETY: mapped read-write.
                unsafe { at.cast::<u8>().read() },
                b'0' + k as u8,
                "page {k} at its own address"
            );
        }
        assert_eq!(XMemMapPhysicalPages(pool, 2, std::ptr::null()), pool);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// The clip's off switch: `VICTORY_MAP_CLIP=0` maps a run as it comes,
    /// everything else (unset, empty, `1`) clips.
    #[test]
    fn the_map_clip_can_be_turned_off() {
        assert!(clip_from(None));
        assert!(clip_from(Some("1")));
        assert!(!clip_from(Some("0")));
        assert!(!clip_from(Some(" 0 ")));
        // An unser switch is not one.
        assert!(clip_from(Some("off")));
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// An owned allocator, so the page-number tests touch no shared state and
    /// no other test can hand out or free numbers under them.
    fn owned_physical(next_page: usize) -> Physical {
        Physical {
            next_page,
            ..Default::default()
        }
    }

    /// A section that owns nothing: the number bookkeeping under test never
    /// looks inside it (`Drop` closes a null handle, which does nothing).
    fn empty_section() -> Arc<Section> {
        Arc::new(Section(std::ptr::null_mut()))
    }

    fn take(numbers: &[MaybeUninit<usize>]) -> Vec<usize> {
        // SAFETY: `allocate` wrote every entry.
        numbers.iter().map(|n| unsafe { n.assume_init() }).collect()
    }

    /// Streaming allocate/free must reuse page numbers: without a free list
    /// `next_page` climbs by `FRAMES` a page, and a long session packs past
    /// the title's 21-bit `number >> 4` field, after which allocation fails.
    #[test]
    fn freed_page_numbers_are_handed_out_again() {
        // The policy is the test's, not the environment's.
        force_recycle(Some(true));
        let section = empty_section();
        let mut physical = owned_physical(FIRST_PAGE_NUMBER);
        let mut numbers = [MaybeUninit::<usize>::uninit(); 2];
        assert!(physical.allocate(&section, &mut numbers));
        let first = take(&numbers);
        let ceiling = physical.next_page;
        for &n in &first {
            physical.free(n);
        }
        for round in 0..1024 {
            assert!(physical.allocate(&section, &mut numbers), "round {round}");
            let again = take(&numbers);
            assert_eq!(physical.next_page, ceiling, "round {round}: next_page grew");
            let mut got = again.clone();
            got.sort_unstable();
            let mut want = first.clone();
            want.sort_unstable();
            assert_eq!(got, want, "round {round}: the same numbers come back");
            for &n in &again {
                physical.free(n);
            }
        }
        force_recycle(None);
    }

    /// `VICTORY_PAGE_RECYCLE=0` is the old ever-growing counter, which the
    /// switch keeps testable without the environment.
    #[test]
    fn the_page_recycle_switch_keeps_the_counter_growing() {
        force_recycle(Some(false));
        let section = empty_section();
        let mut physical = owned_physical(FIRST_PAGE_NUMBER);
        let mut numbers = [MaybeUninit::<usize>::uninit(); 2];
        assert!(physical.allocate(&section, &mut numbers));
        for &n in &take(&numbers) {
            physical.free(n);
        }
        let ceiling = physical.next_page;
        assert!(physical.allocate(&section, &mut numbers));
        let grown = physical.next_page;
        force_recycle(None);
        assert!(grown > ceiling, "VICTORY_PAGE_RECYCLE=0 must not reuse");
        assert_eq!(take(&numbers), vec![ceiling, ceiling + FRAMES]);
    }

    /// A page number at the top of the title's 21-bit field still allocates;
    /// one round past it is refused, which the caller turns into "no pages".
    #[test]
    fn page_numbers_stop_at_the_title_field() {
        let section = empty_section();
        let mut physical = owned_physical(PAGE_NUMBER_LIMIT - FRAMES);
        let mut numbers = [MaybeUninit::<usize>::uninit(); 1];
        assert!(physical.allocate(&section, &mut numbers));
        let last = take(&numbers)[0];
        // The title keeps `number >> 4` in 21 bits.
        assert!(last >> 4 < 1 << 21, "{last:#x}");
        assert!(
            !physical.allocate(&section, &mut numbers),
            "one round past the field must be refused"
        );
    }

    #[test]
    fn section_view_index_matches_linear_overlap() {
        let s1 = empty_section();
        let s2 = empty_section();
        let mut p = owned_physical(FIRST_PAGE_NUMBER);
        p.insert_view(
            0x1000_0000,
            4 * PAGE,
            Slice {
                section: Arc::clone(&s1),
                index: 2,
            },
        );
        p.insert_view(
            0x2000_0000,
            2 * PAGE,
            Slice {
                section: Arc::clone(&s1),
                index: 4,
            },
        );
        p.insert_view(
            0x3000_0000,
            2 * PAGE,
            Slice {
                section: Arc::clone(&s2),
                index: 2,
            },
        );

        let query = Slice {
            section: Arc::clone(&s1),
            index: 3,
        };
        let mut got = p.ranges_for(&query, 3, PAGE / 2, 2 * PAGE);
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                (0x1000_0000 + PAGE + PAGE / 2, 2 * PAGE),
                (0x2000_0000, PAGE + PAGE / 2),
            ]
        );
        // Removing a view updates the secondary index too.
        assert!(p.remove_view(0x2000_0000).is_some());
        assert_eq!(
            p.ranges_for(&query, 3, PAGE / 2, 2 * PAGE),
            vec![(0x1000_0000 + PAGE + PAGE / 2, 2 * PAGE)]
        );
    }
}
