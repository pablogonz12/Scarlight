//! Experimental section-view dirty tracking. Unrecognized ranges retain guards.
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
};

const BLOCK: usize = crate::watch::PAGE;
const PAGE: usize = 4096;

struct State {
    gadget: usize,
    uffd: isize,
    pagemap: isize,
    ranges: BTreeMap<usize, usize>,
    scans: u64,
    micros: u128,
    pages: u64,
    fallbacks: [u64; 3],
    last_log: Instant,
}

// Calls the syscall instruction in the current process's executable libc.
unsafe fn syscall(g: usize, nr: usize, a: usize, b: usize, c: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "call {g}",
            g = in(reg) g,
            inlateout("rax") nr as isize => result,
            in("rdi") a, in("rsi") b, in("rdx") c,
            clobber_abi("C"),
        );
    }
    result
}

fn gadget() -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if ntdll.is_null() || GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_none() {
            return None;
        }
    }
    let exe = std::env::current_exe().ok()?.to_string_lossy().into_owned();
    let pids: Vec<_> = std::fs::read_dir(r"Z:\proc")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let pid = e.file_name().to_string_lossy().into_owned();
            if !pid.bytes().all(|v| v.is_ascii_digit()) {
                return None;
            }
            let cmd = std::fs::read(format!(r"Z:\proc\{pid}\cmdline")).ok()?;
            cmd.split(|v| *v == 0)
                .next()?
                .eq_ignore_ascii_case(exe.as_bytes())
                .then_some(pid)
        })
        .collect();
    if pids.len() != 1 {
        return None;
    }
    let maps = std::fs::read_to_string(format!(r"Z:\proc\{}\maps", pids[0])).ok()?;
    for line in maps.lines().filter(|v| v.contains("libc.so")) {
        let mut fields = line.split_whitespace();
        let (lo, hi) = fields.next()?.split_once('-')?;
        if !fields.next()?.starts_with("r-x") {
            continue;
        }
        let lo = usize::from_str_radix(lo, 16).ok()?;
        let hi = usize::from_str_radix(hi, 16).ok()?;
        let bytes = unsafe { std::slice::from_raw_parts(lo as *const u8, hi.checked_sub(lo)?) };
        if let Some(pos) = bytes.windows(3).position(|v| v == [0x0f, 0x05, 0xc3]) {
            return Some(lo + pos);
        }
    }
    None
}

fn state() -> &'static Mutex<Option<State>> {
    static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new((|| {
            if std::env::var_os("VICTORY_LINUX_DIRTY").is_none_or(|v| v != "1") {
                return None;
            }
            let g = gadget()?;
            unsafe {
                let uffd = syscall(g, 323, 1, 0, 0);
                if uffd < 0 {
                    return None;
                }
                let mut api = [0xaa_u64, (1 << 15) | (1 << 12), 0];
                if syscall(g, 16, uffd as usize, 0xc018aa3f, api.as_mut_ptr() as usize) != 0 {
                    syscall(g, 3, uffd as usize, 0, 0);
                    return None;
                }
                let pm = syscall(g, 2, c"/proc/self/pagemap".as_ptr() as usize, 0, 0);
                if pm < 0 {
                    syscall(g, 3, uffd as usize, 0, 0);
                    return None;
                }
                Some(State {
                    gadget: g,
                    uffd,
                    pagemap: pm,
                    ranges: BTreeMap::new(),
                    scans: 0,
                    micros: 0,
                    pages: 0,
                    fallbacks: [0; 3],
                    last_log: Instant::now(),
                })
            }
        })())
    })
}

impl State {
    fn unregister(&mut self, start: usize, end: usize) {
        let mut range = [start as u64, (end - start) as u64];
        unsafe {
            syscall(
                self.gadget,
                16,
                self.uffd as usize,
                0x8010aa01,
                range.as_mut_ptr() as usize,
            );
        }
        self.ranges.remove(&start);
    }
    fn forget(&mut self, start: usize, end: usize) {
        let hit: Vec<_> = self
            .ranges
            .iter()
            .filter(|&(&a, &b)| a < end && start < b)
            .map(|(&a, &b)| (a, b))
            .collect();
        for (a, b) in hit {
            self.unregister(a, b);
        }
    }
    fn log(&mut self) {
        if self.last_log.elapsed() < Duration::from_secs(10) {
            return;
        }
        let bytes: usize = self.ranges.iter().map(|(&a, &b)| b - a).sum();
        tracing::info!(
            registered_mib = bytes / (1024 * 1024),
            scans = self.scans,
            us = self.micros as u64,
            pages = self.pages,
            alias = self.fallbacks[0],
            range = self.fallbacks[1],
            error = self.fallbacks[2],
            "linux dirty"
        );
        self.scans = 0;
        self.micros = 0;
        self.pages = 0;
        self.fallbacks = [0; 3];
        self.last_log = Instant::now();
    }
}

pub(crate) fn forget(start: usize, len: usize) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.forget(start, start.saturating_add(len));
    }
}

/// Returns `None` for the guard path, conservatively forgetting overlaps first.
pub(crate) fn ask(start: usize, end: usize) -> Option<Vec<usize>> {
    let mut lock = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = lock.as_mut() else {
        if std::env::var_os("VICTORY_LINUX_DIRTY").is_some_and(|v| v == "1") {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("Linux dirty initialization failed; using guards"));
            static FAILED: OnceLock<Mutex<(Instant, u64)>> = OnceLock::new();
            let mut failed = FAILED
                .get_or_init(|| Mutex::new((Instant::now(), 0)))
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            failed.1 += 1;
            if failed.0.elapsed() >= Duration::from_secs(10) {
                tracing::info!(
                    registered_mib = 0,
                    scans = 0,
                    us = 0,
                    pages = 0,
                    initialization = failed.1,
                    "linux dirty"
                );
                *failed = (Instant::now(), 0);
            }
        }
        return None;
    };
    s.log();
    if start >= end || !start.is_multiple_of(BLOCK) || !end.is_multiple_of(BLOCK) {
        s.fallbacks[1] += 1;
        s.forget(start, end);
        return None;
    }
    if (start..end).step_by(BLOCK).any(crate::inplace::aliased) {
        s.fallbacks[0] += 1;
        s.forget(start, end);
        return None;
    }
    if s.ranges.get(&start).copied() != Some(end) {
        s.forget(start, end);
        if crate::guards::any_protected(start / BLOCK, end / BLOCK) {
            s.fallbacks[1] += 1;
            return None;
        }
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe { VirtualQuery(start as _, &mut info, size_of_val(&info)) };
        if n == 0
            || info.State != MEM_COMMIT
            || info.Type != MEM_MAPPED
            || info.Protect != PAGE_READWRITE
            || (info.BaseAddress as usize).saturating_add(info.RegionSize) < end
        {
            s.fallbacks[1] += 1;
            return None;
        }
        let mut reg = [start as u64, (end - start) as u64, 2, 0];
        let rc = unsafe {
            syscall(
                s.gadget,
                16,
                s.uffd as usize,
                0xc020aa00,
                reg.as_mut_ptr() as usize,
            )
        };
        if rc != 0 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(error = rc, "Linux dirty registration failed; using guards")
            });
            s.fallbacks[2] += 1;
            return None;
        }
        let mut wp = [start as u64, (end - start) as u64, 1];
        if unsafe {
            syscall(
                s.gadget,
                16,
                s.uffd as usize,
                0xc018aa06,
                wp.as_mut_ptr() as usize,
            )
        } != 0
        {
            s.unregister(start, end);
            s.fallbacks[2] += 1;
            return None;
        }
        s.ranges.insert(start, end);
        // Initial copy must see all contents, including writes before registration.
        return Some((start / BLOCK..end / BLOCK).collect());
    }
    let begin = Instant::now();
    let mut vec = vec![0u64; (end - start) / PAGE * 3];
    let mut arg = [
        96u64,
        3,
        start as u64,
        end as u64,
        0,
        vec.as_mut_ptr() as u64,
        (vec.len() / 3) as u64,
        0,
        0,
        2,
        0,
        2,
    ];
    let count = unsafe {
        syscall(
            s.gadget,
            16,
            s.pagemap as usize,
            0xc0606610,
            arg.as_mut_ptr() as usize,
        )
    };
    s.scans += 1;
    s.micros += begin.elapsed().as_micros();
    if count < 0 || arg[4] != end as u64 {
        s.unregister(start, end);
        s.fallbacks[2] += 1;
        return None;
    }
    let mut written = Vec::new();
    for r in vec.chunks_exact(3).take(count as usize) {
        if r[0] < start as u64 || r[1] > end as u64 || r[0] >= r[1] {
            s.unregister(start, end);
            s.fallbacks[2] += 1;
            return None;
        }
        s.pages += (r[1] - r[0]) / PAGE as u64;
        written.extend(r[0] as usize / BLOCK..(r[1] as usize).div_ceil(BLOCK));
    }
    written.sort_unstable();
    written.dedup();
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile3, UnmapViewOfFile2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn linux_dirty_section_lifecycle() {
        if std::env::var_os("VICTORY_LINUX_DIRTY").is_none_or(|v| v != "1") {
            return;
        }
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                std::ptr::null(),
                0,
                BLOCK,
                0,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert!(!view.Value.is_null());
            let a = view.Value as usize;
            assert_eq!(ask(a, a + BLOCK), Some(vec![a / BLOCK]));
            assert_eq!(ask(a, a + BLOCK), Some(vec![]));
            (a as *mut u8).write_volatile(1);
            assert_eq!(ask(a, a + BLOCK), Some(vec![a / BLOCK]));
            assert_eq!(ask(a, a + BLOCK), Some(vec![]));
            assert_ne!(UnmapViewOfFile2(GetCurrentProcess(), view, 0), 0);
            crate::watch::title_memory_remapped(a, BLOCK);
            let view2 = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                std::ptr::null(),
                0,
                BLOCK,
                0,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert!(!view2.Value.is_null());
            let b = view2.Value as usize;
            assert_eq!(ask(b, b + BLOCK), Some(vec![b / BLOCK]));
            forget(b, BLOCK);
            assert_ne!(UnmapViewOfFile2(GetCurrentProcess(), view2, 0), 0);
            CloseHandle(section);
        }
    }

    #[test]
    fn linux_dirty_alias_falls_back() {
        if std::env::var_os("VICTORY_LINUX_DIRTY").is_none_or(|v| v != "1") {
            return;
        }
        let _lock = crate::watch::journal_test_lock();
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let map = || {
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    std::ptr::null(),
                    0,
                    BLOCK,
                    0,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            let a = map();
            let b = map();
            assert!(!a.Value.is_null() && !b.Value.is_null());
            let (a0, b0) = (a.Value as usize, b.Value as usize);
            crate::inplace::title_memory_view(a0, BLOCK, section as usize, 0);
            assert_eq!(ask(a0, a0 + BLOCK), Some(vec![a0 / BLOCK]));
            crate::inplace::title_memory_view(b0, BLOCK, section as usize, 0);
            assert!(ask(a0, a0 + BLOCK).is_none());
            (b0 as *mut u8).write_volatile(42);
            assert_eq!((a0 as *const u8).read_volatile(), 42);
            let answer = crate::watch::writes(a0, a0 + BLOCK, true).unwrap();
            assert!(answer.1.iter().any(|&(page, _)| page == a0 / BLOCK));
            UnmapViewOfFile2(GetCurrentProcess(), a, 0);
            UnmapViewOfFile2(GetCurrentProcess(), b, 0);
            crate::watch::title_memory_remapped(a0, BLOCK);
            crate::watch::title_memory_remapped(b0, BLOCK);
            CloseHandle(section);
        }
    }

    #[test]
    fn linux_dirty_busy_falls_back() {
        if std::env::var_os("VICTORY_LINUX_DIRTY").is_none_or(|v| v != "1") {
            return;
        }
        unsafe {
            let g = gadget().unwrap();
            let fd = syscall(g, 323, 1, 0, 0);
            assert!(fd >= 0);
            let mut api = [0xaa_u64, (1 << 15) | (1 << 12), 0];
            assert_eq!(
                syscall(g, 16, fd as usize, 0xc018aa3f, api.as_mut_ptr() as usize),
                0
            );
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            );
            let view = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                std::ptr::null(),
                0,
                BLOCK,
                0,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert!(!view.Value.is_null());
            let a = view.Value as usize;
            let mut reg = [a as u64, BLOCK as u64, 2, 0];
            assert_eq!(
                syscall(g, 16, fd as usize, 0xc020aa00, reg.as_mut_ptr() as usize),
                0
            );
            assert_eq!(ask(a, a + BLOCK), None);
            let answer = crate::watch::writes(a, a + BLOCK, true);
            assert!(answer.is_some());
            syscall(g, 3, fd as usize, 0, 0);
            UnmapViewOfFile2(GetCurrentProcess(), view, 0);
            crate::watch::title_memory_remapped(a, BLOCK);
            CloseHandle(section);
        }
    }

    #[test]
    fn linux_dirty_racing_writes() {
        if std::env::var_os("VICTORY_LINUX_DIRTY").is_none_or(|v| v != "1") {
            return;
        }
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            );
            let view = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                std::ptr::null(),
                0,
                BLOCK,
                0,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert!(!view.Value.is_null());
            let a = view.Value as usize;
            assert!(ask(a, a + BLOCK).is_some());
            for _ in 0..1000 {
                let barrier = std::sync::Barrier::new(2);
                let race = std::thread::scope(|scope| {
                    let join = scope.spawn(|| {
                        barrier.wait();
                        ask(a, a + BLOCK)
                    });
                    barrier.wait();
                    ((a + BLOCK - PAGE) as *mut u8).write_volatile(1);
                    join.join().unwrap()
                });
                let drain = ask(a, a + BLOCK);
                assert!(
                    race.as_ref().is_none_or(|v| v.contains(&(a / BLOCK)))
                        || drain.as_ref().is_none_or(|v| v.contains(&(a / BLOCK)))
                );
            }
            forget(a, BLOCK);
            UnmapViewOfFile2(GetCurrentProcess(), view, 0);
            CloseHandle(section);
        }
    }
}
