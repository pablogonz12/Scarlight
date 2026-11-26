//! Compatibility hooks placed in the title build being run.
//!
//! What to patch and where comes from that build's profile
//! (`victory_core::profile`), which the user keeps outside the repository;
//! this module knows how to place a byte-checked patch, not what any one
//! title needs. Nothing is applied unless `VICTORY_HOOKS` is set and the
//! profile for the mapped image lists hooks.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_GUARD, PAGE_NOACCESS,
    VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Environment variable that turns the profile's hooks on.
pub(crate) const HOOKS_ENV: &str = "VICTORY_HOOKS";
/// What the same switch was called before profiles carried the patches.
pub(crate) const LEGACY_HOOKS_ENV: &str = "VICTORY_NO_WATCHDOG";

/// Whether the user asked for the profile's hooks to be placed.
pub(crate) fn enabled() -> bool {
    [HOOKS_ENV, LEGACY_HOOKS_ENV]
        .iter()
        .any(|k| std::env::var_os(k).is_some())
}

/// Applies the hooks the profile for `hash` lists. Returns how many applied.
///
/// A profile is the user's file, so every hook is checked before it is
/// touched: the bytes must decode, `new` must be as long as `expect`, and
/// `rva..rva+expect.len()` must lie inside the mapped image. Anything else is
/// logged and skipped; a bad profile never faults the process.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash` and whose size
/// is `size`, and no title code is running yet.
pub(crate) unsafe fn apply_profile(base: usize, size: usize, hash: &blake3::Hash) -> usize {
    let Some(profile) = victory_core::profile::current() else {
        tracing::debug!("no title profile: no hooks to place");
        return 0;
    };
    if !profile.matches(&hash.to_hex()) {
        return 0;
    }
    let mut applied = 0;
    // The switches a hook may depend on: one profile per process, so the
    // environment's answer is asked once, here.
    let scale = profile.render_scale();
    let own = profile.own_upscaler();
    for hook in &profile.hooks {
        if !hook.wanted(scale, own) {
            tracing::debug!(what = hook.what, "hook skipped: its switch is off");
            continue;
        }
        let (Ok(expect), Ok(new)) = (
            hook.expected_at(base as u64),
            hook.replacement_at(base as u64, scale),
        ) else {
            tracing::warn!(what = hook.what, "hook skipped: its bytes do not decode");
            continue;
        };
        if let Some(why) = unusable(&expect, &new, hook.rva, size) {
            tracing::warn!(what = hook.what, "hook skipped: {why}");
            continue;
        }
        // SAFETY: forwarded from this function's contract, and `unusable`
        // checked that the whole hook lies inside the image.
        if unsafe { apply(base, hook.rva as usize, &expect, &new, &hook.what) } {
            applied += 1;
        }
    }
    applied
}

/// Why a hook cannot be applied to an image of `size` bytes, if it cannot.
fn unusable(expect: &[u8], new: &[u8], rva: u64, size: usize) -> Option<&'static str> {
    if expect.is_empty() {
        return Some("it has no bytes");
    }
    if new.len() != expect.len() {
        return Some("its new bytes differ in length from its expect bytes");
    }
    match usize::try_from(rva) {
        Ok(at) if at.checked_add(expect.len()).is_some_and(|end| end <= size) => None,
        _ => Some("it lies outside the mapped image"),
    }
}

/// One run of the hook's range and the protection it had: a range can span
/// pages with different protections (code read-execute next to read-only
/// data), and each must be given back what it had.
struct Region {
    at: usize,
    len: usize,
    protect: u32,
}

/// Makes `at..at+len` writable, one memory region at a time, returning what
/// each region's protection was, or `None` if any part of it is not committed
/// readable memory (a hook must never touch a guard page or a free range).
///
/// # Safety
/// `at` must be an address a query may be asked about.
unsafe fn writable(at: usize, len: usize) -> Option<Vec<Region>> {
    let mut regions: Vec<Region> = Vec::new();
    let end = at.checked_add(len)?;
    let mut start = at;
    while start < end {
        // SAFETY: the buffer is a full `MEMORY_BASIC_INFORMATION`.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: as above; `VirtualQuery` writes at most one struct.
        let asked = unsafe {
            VirtualQuery(
                start as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if asked == 0 {
            // SAFETY: the regions taken so far are ours to give back.
            unsafe { restore(&regions) };
            return None;
        }
        let base = info.BaseAddress as usize;
        let region_end = base.saturating_add(info.RegionSize);
        let stop = region_end.min(end);
        if info.State != MEM_COMMIT || !readable(info.Protect) || stop <= start {
            // SAFETY: the regions taken so far are ours to give back.
            unsafe { restore(&regions) };
            return None;
        }
        let mut old = 0;
        // SAFETY: the bytes belong to this image and stay mapped.
        if unsafe { VirtualProtect(start as _, stop - start, PAGE_EXECUTE_READWRITE, &mut old) }
            == 0
        {
            // SAFETY: the regions taken so far are ours to give back.
            unsafe { restore(&regions) };
            return None;
        }
        regions.push(Region {
            at: start,
            len: stop - start,
            protect: old,
        });
        start = stop;
    }
    Some(regions)
}

/// Whether a protection lets us read the bytes.
fn readable(protect: u32) -> bool {
    protect & 0xff != PAGE_NOACCESS && protect & PAGE_GUARD == 0
}

/// Gives each region back the protection it had.
///
/// # Safety
/// The ranges must still be the ones [`writable`] changed.
unsafe fn restore(regions: &[Region]) {
    for region in regions {
        let mut old = 0;
        // SAFETY: this range was made writable by `writable`.
        unsafe { VirtualProtect(region.at as _, region.len, region.protect, &mut old) };
    }
}

/// Writes `new` over `expect` at `base + rva` if the bytes there are the
/// ones the profile says they are.
///
/// # Safety
/// See [`apply_profile`].
unsafe fn apply(base: usize, rva: usize, expect: &[u8], new: &[u8], what: &str) -> bool {
    let at = (base + rva) as *mut u8;
    // The hook may cross a page boundary, whose other side can be freed or
    // guarded by the time we get here: ask before reading.
    // SAFETY: the range was checked to lie inside the image; the query only
    // reads the page tables.
    let Some(regions) = (unsafe { writable(at as usize, expect.len()) }) else {
        tracing::warn!(
            what,
            "hook skipped: its range is not committed readable memory"
        );
        return false;
    };
    // SAFETY: the range is committed, readable and now writable for its
    // whole length.
    let now = unsafe { std::slice::from_raw_parts(at, expect.len()) };
    if now != expect {
        // SAFETY: the regions are the ones `writable` changed.
        unsafe { restore(&regions) };
        tracing::warn!(what, "hook skipped: the bytes are not the expected ones");
        return false;
    }
    // SAFETY: writable now; the bytes are the ones checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(new.as_ptr(), at, new.len());
        FlushInstructionCache(GetCurrentProcess(), at as _, new.len());
    }
    // SAFETY: the regions are the ones `writable` changed.
    unsafe { restore(&regions) };
    tracing::info!(what, "hook applied");
    true
}

#[cfg(test)]
mod tests {
    use super::unusable;
    use windows_sys::Win32::System::Memory::{
        MEM_RELEASE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_READONLY, PAGE_READWRITE,
        VirtualFree, VirtualProtect, VirtualQuery,
    };

    /// The protection `*at` has now, one page into its region.
    fn protection(at: usize) -> u32 {
        // SAFETY: a caller-chosen address in this process.
        unsafe {
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            let asked = VirtualQuery(
                at as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            assert_ne!(asked, 0, "VirtualQuery failed");
            info.Protect
        }
    }

    /// Two committed pages, `[0..page)` read-only and `[page..2page)` executable
    /// read, filled with 0xAA.
    struct Pages {
        base: usize,
    }

    impl Pages {
        fn new() -> Self {
            use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, VirtualAlloc};
            // SAFETY: a fresh reservation of two pages, filled by us.
            unsafe {
                let base = VirtualAlloc(
                    std::ptr::null(),
                    2 * page(),
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                ) as usize;
                assert_ne!(base, 0, "VirtualAlloc failed");
                std::ptr::write_bytes(base as *mut u8, 0xAA, 2 * page());
                let mut old = 0;
                assert_ne!(
                    VirtualProtect(base as _, page(), PAGE_READONLY, &mut old),
                    0
                );
                assert_ne!(
                    VirtualProtect((base + page()) as _, page(), PAGE_EXECUTE_READ, &mut old),
                    0
                );
                Self { base }
            }
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: the reservation made in `new`.
            unsafe { VirtualFree(self.base as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// x86-64 Windows pages are 4 KiB, which is what these tests need to put
    /// a hook across a boundary.
    fn page() -> usize {
        4096
    }

    #[test]
    fn a_hook_across_two_pages_gives_each_page_its_own_protection_back() {
        let pages = Pages::new();
        // Four bytes centred on the boundary: two in each page.
        let rva = page() - 2;
        // SAFETY: both pages are ours, readable, and the hook lies inside them.
        let applied = unsafe { super::apply(pages.base, rva, &[0xAA; 4], &[0xBB; 4], "test") };
        assert!(applied);
        // SAFETY: the four bytes were just written inside our own pages.
        let bytes = unsafe { std::slice::from_raw_parts((pages.base + rva) as *const u8, 4) };
        assert_eq!(bytes, [0xBB; 4]);
        assert_eq!(protection(pages.base), PAGE_READONLY);
        assert_eq!(protection(pages.base + page()), PAGE_EXECUTE_READ);
    }

    #[test]
    fn a_hook_whose_range_is_not_committed_is_skipped_not_faulted() {
        let pages = Pages::new();
        // One byte into the second page, four bytes long: the last two are
        // past the reservation, an unmapped range.
        let rva = 2 * page() - 1;
        // SAFETY: the first page is ours; the rest is not touched, only queried.
        let applied = unsafe { super::apply(pages.base, rva, &[0xAA; 4], &[0xBB; 4], "test") };
        assert!(!applied);
        assert_eq!(protection(pages.base), PAGE_READONLY);
    }

    /// An image of 0x1000 bytes with a five-byte hook at 0x100.
    const SIZE: usize = 0x1000;

    #[test]
    fn a_hook_inside_the_image_is_usable() {
        assert_eq!(unusable(&[1, 2, 3], &[4, 5, 6], 0x100, SIZE), None);
        // The last byte of the image is still inside it.
        assert_eq!(unusable(&[1, 2, 3], &[4, 5, 6], 0xffd, SIZE), None);
    }

    #[test]
    fn a_hook_past_the_end_of_the_image_is_refused() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5, 6], 0xffe, SIZE),
            Some("it lies outside the mapped image")
        );
        assert_eq!(
            unusable(&[1], &[4], 0x1000, SIZE),
            Some("it lies outside the mapped image")
        );
    }

    #[test]
    fn an_rva_beyond_the_address_space_is_refused_not_wrapped() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5, 6], u64::MAX, SIZE),
            Some("it lies outside the mapped image")
        );
        // The end would overflow `usize`: refused, not wrapped to a low one.
        assert_eq!(
            unusable(&[1, 2], &[3, 4], u64::MAX - 1, usize::MAX),
            Some("it lies outside the mapped image")
        );
    }

    #[test]
    fn a_hook_whose_new_bytes_differ_in_length_is_refused() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5], 0x100, SIZE),
            Some("its new bytes differ in length from its expect bytes")
        );
        assert_eq!(unusable(&[], &[], 0x100, SIZE), Some("it has no bytes"));
    }
}
