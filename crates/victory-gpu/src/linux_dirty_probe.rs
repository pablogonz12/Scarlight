//! Exploratory Linux syscall access from a Wine process; not a runtime backend.

use std::ffi::{c_char, c_void};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn VirtualAlloc(
        address: *mut c_void,
        size: usize,
        allocation: u32,
        protection: u32,
    ) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
}

// The gadget executes a Linux syscall, not a Windows ABI function.
unsafe fn linux_call(gadget: usize, nr: usize, arg: usize, arg2: usize, arg3: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "call {gadget}",
            gadget = in(reg) gadget,
            inlateout("rax") nr as isize => result,
            in("rdi") arg,
            in("rsi") arg2,
            in("rdx") arg3,
            clobber_abi("C"),
        );
    }
    result
}

#[test]
fn linux_dirty_syscall_probe() {
    if std::env::var_os("VICTORY_LINUX_DIRTY_PROBE").is_none() {
        eprintln!("set VICTORY_LINUX_DIRTY_PROBE to run this experimental probe");
        return;
    }
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr());
        if ntdll.is_null() || GetProcAddress(ntdll, c"wine_get_version".as_ptr()).is_null() {
            eprintln!("not Wine; skipped");
            return;
        }
    }
    eprintln!("Wine detected");
    let proc = std::fs::read_dir(r"Z:\proc").expect("read proc");
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let pids: Vec<_> = proc
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().into_owned();
            if !pid.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let cmd = std::fs::read(format!(r"Z:\proc\{pid}\cmdline")).ok()?;
            let args: Vec<_> = cmd.split(|b| *b == 0).collect();
            if args
                .first()
                .is_some_and(|arg| arg.eq_ignore_ascii_case(exe.as_bytes()))
            {
                Some(pid)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pids.len(), 1, "probe requires an isolated executable");
    let pid = &pids[0];
    eprintln!("candidate Linux pid {pid}");
    let maps = std::fs::read_to_string(format!(r"Z:\proc\{pid}\maps")).expect("read process maps");
    eprintln!("read maps, {} bytes", maps.len());
    let mut gadget = None;
    for line in maps.lines().filter(|line| line.contains("libc.so")) {
        let mut fields = line.split_whitespace();
        let range = fields.next().unwrap();
        let permissions = fields.next().unwrap();
        if !permissions.starts_with("r-x") {
            continue;
        }
        let (start, end) = range.split_once('-').unwrap();
        let start = usize::from_str_radix(start, 16).unwrap();
        let end = usize::from_str_radix(end, 16).unwrap();
        let bytes = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };
        if let Some(offset) = bytes.windows(3).position(|v| v == [0x0f, 0x05, 0xc3]) {
            gadget = Some(start + offset);
            break;
        }
    }
    let gadget = gadget.expect("no syscall/ret gadget in libc executable mapping");
    eprintln!("calling libc gadget");
    unsafe {
        let pid = linux_call(gadget, 39, 0, 0, 0);
        eprintln!("libc syscall gadget getpid={pid}");
        assert!(pid > 0);
        for flags in [0, 1] {
            let fd = linux_call(gadget, 323, flags, 0, 0);
            eprintln!("userfaultfd flags={flags}: {fd}");
            if fd >= 0 {
                linux_call(gadget, 3, fd as usize, 0, 0);
            }
        }
        scan_anonymous(gadget);
        scan_section(gadget);
        coexist_write_watch(gadget);
        benchmark(gadget);
    }
}

fn benchmark(g: usize) {
    for size in [65536, 1024 * 1024, 50 * 1024 * 1024, 256 * 1024 * 1024] {
        for workers in [1, 7] {
            for backend in 0..3 {
                let barrier = std::sync::Barrier::new(workers);
                let values = std::thread::scope(|scope| {
                    let mut threads = Vec::new();
                    for _ in 0..workers {
                        let barrier = &barrier;
                        threads.push(
                            scope.spawn(move || unsafe { bench_worker(g, size, backend, barrier) }),
                        );
                    }
                    threads
                        .into_iter()
                        .map(|h| h.join().unwrap())
                        .collect::<Vec<_>>()
                });
                let mean = values.iter().sum::<f64>() / workers as f64;
                eprintln!(
                    "bench size={size} threads={workers} backend={backend} mean_us={mean:.2}"
                );
            }
        }
    }
}

unsafe fn bench_worker(g: usize, size: usize, backend: usize, barrier: &std::sync::Barrier) -> f64 {
    use windows_sys::Win32::System::Memory::{GetWriteWatch, ResetWriteWatch};
    unsafe {
        let p = VirtualAlloc(
            std::ptr::null_mut(),
            size,
            if backend == 0 { 0x3000 } else { 0x203000 },
            4,
        ) as *mut u8;
        assert!(!p.is_null());
        for i in (0..size).step_by(4096) {
            p.add(i).write_volatile(0);
        }
        let mut fd = -1;
        let mut pm = -1;
        if backend == 0 {
            fd = linux_call(g, 323, 1, 0, 0);
            assert!(fd >= 0);
            let mut api = [0xaa_u64, 1 << 15, 0];
            assert_eq!(
                linux_call(g, 16, fd as usize, 0xc018aa3f, api.as_mut_ptr() as usize),
                0
            );
            let mut reg = [p as u64, size as u64, 2, 0];
            assert_eq!(
                linux_call(g, 16, fd as usize, 0xc020aa00, reg.as_mut_ptr() as usize),
                0
            );
            let mut wp = [p as u64, size as u64, 1];
            assert_eq!(
                linux_call(g, 16, fd as usize, 0xc018aa06, wp.as_mut_ptr() as usize),
                0
            );
            pm = linux_call(g, 2, c"/proc/self/pagemap".as_ptr() as usize, 0, 0);
            assert!(pm >= 0);
        } else {
            assert_eq!(ResetWriteWatch(p.cast(), size), 0);
        }
        let mut regions = vec![0u64; size / 4096 * 3];
        let mut addresses = vec![std::ptr::null_mut(); size / 4096];
        let mut total = std::time::Duration::ZERO;
        barrier.wait();
        for _ in 0..10 {
            for i in (0..size).step_by(4096 * 100) {
                p.add(i).write_volatile(1);
            }
            let start = std::time::Instant::now();
            if backend == 0 {
                let mut found = 0usize;
                let mut arg = [
                    96u64,
                    3,
                    p as u64,
                    (p as usize + size) as u64,
                    0,
                    regions.as_mut_ptr() as u64,
                    (regions.len() / 3) as u64,
                    0,
                    0,
                    2,
                    0,
                    2,
                ];
                while arg[2] < arg[3] {
                    let n = linux_call(g, 16, pm as usize, 0xc0606610, arg.as_mut_ptr() as usize);
                    assert!(n >= 0);
                    for region in regions.chunks_exact(3).take(n as usize) {
                        assert!(region[0] >= p as u64 && region[1] <= (p as usize + size) as u64);
                        found += (region[1] - region[0]) as usize / 4096;
                    }
                    assert!(arg[4] > arg[2]);
                    arg[2] = arg[4];
                }
                assert_eq!(found, (size / 4096).div_ceil(100));
            } else if backend == 1 {
                let mut count = addresses.len();
                let mut gran = 0;
                assert_eq!(
                    GetWriteWatch(
                        0,
                        p.cast(),
                        size,
                        addresses.as_mut_ptr(),
                        &mut count,
                        &mut gran
                    ),
                    0
                );
                assert_eq!(count, (size / 4096).div_ceil(100));
                assert_eq!(ResetWriteWatch(p.cast(), size), 0);
            } else {
                assert!(crate::watch::writes(p as usize, p as usize + size, false).is_some());
            }
            total += start.elapsed();
        }
        if pm >= 0 {
            linux_call(g, 3, pm as usize, 0, 0);
        }
        if fd >= 0 {
            linux_call(g, 3, fd as usize, 0, 0);
        }
        assert_ne!(VirtualFree(p.cast(), 0, 0x8000), 0);
        total.as_secs_f64() * 1e6 / 10.0
    }
}

unsafe fn coexist_write_watch(g: usize) {
    use windows_sys::Win32::System::Memory::{GetWriteWatch, ResetWriteWatch};
    const LEN: usize = 65536;
    unsafe {
        let p = VirtualAlloc(std::ptr::null_mut(), LEN, 0x203000, 4) as *mut u8;
        assert!(!p.is_null());
        for i in (0..LEN).step_by(4096) {
            p.add(i).write_volatile(0);
        }
        assert_eq!(ResetWriteWatch(p.cast(), LEN), 0);
        let fd = linux_call(g, 323, 1, 0, 0);
        assert!(fd >= 0);
        let ioctl = |op: usize, ptr: *mut u64| linux_call(g, 16, fd as usize, op, ptr as usize);
        let mut api = [0xaa, 1 << 15, 0];
        assert_eq!(ioctl(0xc018aa3f, api.as_mut_ptr()), 0);
        let mut reg = [p as u64, LEN as u64, 2, 0];
        let register = ioctl(0xc020aa00, reg.as_mut_ptr());
        eprintln!("write-watch coexist registration: {register}");
        if register != 0 {
            linux_call(g, 3, fd as usize, 0, 0);
            assert_ne!(VirtualFree(p.cast(), 0, 0x8000), 0);
            return;
        }
        let mut wp = [p as u64, LEN as u64, 1];
        assert_eq!(ioctl(0xc018aa06, wp.as_mut_ptr()), 0);
        p.add(4096).write_volatile(9);
        assert_eq!(p.add(4096).read_volatile(), 9);
        let mut out = [std::ptr::null_mut(); 16];
        let mut count = out.len();
        let mut gran = 0u32;
        let rc = GetWriteWatch(0, p.cast(), LEN, out.as_mut_ptr(), &mut count, &mut gran);
        eprintln!("write-watch coexist result: rc={rc}, pages={count}, granularity={gran}");
        assert_eq!(rc, 0);
        assert_eq!(count, 1, "Wine write-watch missed a write");
        assert_eq!(out[0] as usize, p as usize + 4096);
        linux_call(g, 3, fd as usize, 0, 0);
        assert_ne!(VirtualFree(p.cast(), 0, 0x8000), 0);
    }
}

unsafe fn scan_section(g: usize) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile3, PAGE_READWRITE, UnmapViewOfFile2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const LEN: usize = 256 * 1024 * 1024;
    unsafe {
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            LEN as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let a = MapViewOfFile3(
            section,
            GetCurrentProcess(),
            std::ptr::null(),
            0,
            LEN,
            0,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        );
        let b = MapViewOfFile3(
            section,
            GetCurrentProcess(),
            std::ptr::null(),
            0,
            LEN,
            0,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        );
        assert!(!a.Value.is_null() && !b.Value.is_null());
        let (a0, b0) = (a.Value as usize, b.Value as usize);
        for i in (0..LEN).step_by(4096) {
            ((a0 + i) as *mut u8).write_volatile(0);
            let _ = ((b0 + i) as *const u8).read_volatile();
        }
        let uffd = linux_call(g, 323, 1, 0, 0);
        assert!(uffd >= 0);
        let ioctl =
            |fd: isize, op: usize, ptr: *mut u64| linux_call(g, 16, fd as usize, op, ptr as usize);
        let mut api = [0xaa, (1 << 15) | (1 << 12), 0];
        assert_eq!(ioctl(uffd, 0xc018aa3f, api.as_mut_ptr()), 0);
        for base in [a0, b0] {
            let mut reg = [base as u64, LEN as u64, 2, 0];
            assert_eq!(ioctl(uffd, 0xc020aa00, reg.as_mut_ptr()), 0);
            let mut wp = [base as u64, LEN as u64, 1];
            assert_eq!(ioctl(uffd, 0xc018aa06, wp.as_mut_ptr()), 0);
        }
        let pm = linux_call(g, 2, c"/proc/self/pagemap".as_ptr() as usize, 0, 0);
        assert!(pm >= 0);
        std::thread::spawn(move || {
            for i in (0..LEN / 4096).step_by(100) {
                ((a0 + i * 4096) as *mut u8).write_volatile(1);
            }
        })
        .join()
        .unwrap();
        let scan = |base: usize| {
            let mut buf = vec![0u64; (LEN / 4096) * 3];
            let mut arg = [
                96u64,
                3,
                base as u64,
                (base + LEN) as u64,
                0,
                buf.as_mut_ptr() as u64,
                (buf.len() / 3) as u64,
                0,
                0,
                2,
                0,
                2,
            ];
            let mut pages = Vec::new();
            while arg[2] < arg[3] {
                let n = ioctl(pm, 0xc0606610, arg.as_mut_ptr());
                assert!(n >= 0, "section scan {n}");
                for r in buf.chunks_exact(3).take(n as usize) {
                    for p in (r[0] as usize..r[1] as usize).step_by(4096) {
                        pages.push((p - base) / 4096);
                    }
                }
                assert!(arg[4] > arg[2]);
                arg[2] = arg[4];
            }
            pages
        };
        let a_first = scan(a0);
        let b_first = scan(b0);
        let expected: Vec<_> = (0..LEN / 4096).step_by(100).collect();
        eprintln!("section write A: A={} B={}", a_first.len(), b_first.len());
        assert_eq!(a_first, expected);
        assert_eq!(((b0) as *const u8).read_volatile(), 1);
        (b0 as *mut u8).write_volatile(2);
        let a_second = scan(a0);
        let b_second = scan(b0);
        eprintln!("section write B: A={:?} B={:?}", a_second, b_second);
        assert_eq!((a0 as *const u8).read_volatile(), 2);
        let started = std::time::Instant::now();
        for _ in 0..1000 {
            let barrier = std::sync::Barrier::new(2);
            let raced = std::thread::scope(|scope| {
                let join = scope.spawn(|| {
                    barrier.wait();
                    scan_small(g, pm, a0)
                });
                barrier.wait();
                (a0 as *mut u8).write_volatile(5);
                join.join().unwrap()
            });
            let after = scan_small(g, pm, a0);
            assert!(
                raced.contains(&0) || after.contains(&0),
                "lost racing write"
            );
        }
        eprintln!(
            "race: 1000 iterations, no missed writes, {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let mut old = 0;
        assert_ne!(VirtualProtect(a0 as _, 65536, PAGE_READONLY, &mut old), 0);
        assert_eq!((a0 as *const u8).read_volatile(), 5);
        assert_ne!(VirtualProtect(a0 as _, 65536, PAGE_READWRITE, &mut old), 0);
        (a0 as *mut u8).write_volatile(3);
        eprintln!("after VirtualProtect, section dirty pages: {:?}", scan(a0));
        let _lock = crate::watch::journal_test_lock();
        let first = crate::watch::writes(a0, a0 + 65536, true).expect("guard init");
        let second = crate::watch::writes(a0, a0 + 65536, true).expect("guard reset");
        eprintln!(
            "guard initialized: first={} second={}",
            first.1.len(),
            second.1.len()
        );
        (a0 as *mut u8).write_volatile(4);
        let guarded = crate::watch::writes(a0, a0 + 65536, true).expect("guard after write");
        let fresh = guarded
            .1
            .iter()
            .any(|&(page, epoch)| page == a0 / 65536 && epoch > second.0);
        let async_pages = scan(a0);
        eprintln!("guard and UFFD after write: fresh={fresh} async={async_pages:?}");
        assert!(fresh);
        assert_eq!(async_pages, [0]);
        linux_call(g, 3, pm as usize, 0, 0);
        linux_call(g, 3, uffd as usize, 0, 0);
        UnmapViewOfFile2(GetCurrentProcess(), a, 0);
        UnmapViewOfFile2(GetCurrentProcess(), b, 0);
        CloseHandle(section);
    }
}

fn scan_small(g: usize, pm: isize, base: usize) -> Vec<usize> {
    let mut buf = [0u64; 16 * 3];
    let mut args = [
        96u64,
        3,
        base as u64,
        (base + 65536) as u64,
        0,
        buf.as_mut_ptr() as u64,
        16,
        0,
        0,
        2,
        0,
        2,
    ];
    let mut pages = Vec::new();
    while args[2] < args[3] {
        let count =
            unsafe { linux_call(g, 16, pm as usize, 0xc0606610, args.as_mut_ptr() as usize) };
        assert!(count >= 0, "scan {count}");
        for r in buf.chunks_exact(3).take(count as usize) {
            for address in (r[0] as usize..r[1] as usize).step_by(4096) {
                pages.push((address - base) / 4096);
            }
        }
        assert!(args[4] > args[2]);
        args[2] = args[4];
    }
    pages
}

unsafe fn scan_anonymous(g: usize) {
    const LEN: usize = 256 * 1024 * 1024;
    let p = unsafe { VirtualAlloc(std::ptr::null_mut(), LEN, 0x3000, 4) } as *mut u8;
    assert!(!p.is_null());
    for offset in (0..LEN).step_by(4096) {
        unsafe { p.add(offset).write_volatile(0) };
    }
    let fd = unsafe { linux_call(g, 323, 1, 0, 0) };
    assert!(fd >= 0);
    let ioctl = |fd: isize, op: usize, ptr: *mut u64| unsafe {
        linux_call(g, 16, fd as usize, op, ptr as usize)
    };
    let mut api = [0xaa_u64, 1 << 15, 0];
    assert_eq!(ioctl(fd, 0xc018aa3f, api.as_mut_ptr()), 0);
    assert_ne!(api[1] & (1 << 15), 0);
    let mut register = [p as u64, LEN as u64, 2, 0];
    assert_eq!(ioctl(fd, 0xc020aa00, register.as_mut_ptr()), 0);
    let mut wp = [p as u64, LEN as u64, 1];
    assert_eq!(ioctl(fd, 0xc018aa06, wp.as_mut_ptr()), 0);
    let path = c"/proc/self/pagemap";
    let pm = unsafe { linux_call(g, 2, path.as_ptr() as usize, 0, 0) };
    assert!(pm >= 0, "open pagemap: {pm}");
    let address = p as usize;
    std::thread::spawn(move || {
        for i in (0..LEN / 4096).step_by(100) {
            unsafe { ((address + i * 4096) as *mut u8).write_volatile(1) };
        }
    })
    .join()
    .unwrap();
    let mut regions = vec![0_u64; (LEN / 4096) * 3];
    let mut args = [
        96_u64,
        3,
        p as u64,
        (p as usize + LEN) as u64,
        0,
        regions.as_mut_ptr() as u64,
        (regions.len() / 3) as u64,
        0,
        0,
        2,
        0,
        2,
    ];
    let begin = std::time::Instant::now();
    let mut pages = Vec::new();
    let mut calls = 0;
    while args[2] < args[3] {
        let count = ioctl(pm, 0xc0606610, args.as_mut_ptr());
        assert!(count >= 0, "scan: {count}");
        for region in regions.chunks_exact(3).take(count as usize) {
            for address in (region[0] as usize..region[1] as usize).step_by(4096) {
                pages.push((address - p as usize) / 4096);
            }
        }
        assert!(args[4] > args[2], "scan made no progress");
        args[2] = args[4];
        calls += 1;
    }
    eprintln!(
        "anonymous scan: pages={}, calls={calls}, elapsed={:.1} us",
        pages.len(),
        begin.elapsed().as_secs_f64() * 1e6
    );
    let expected: Vec<_> = (0..LEN / 4096).step_by(100).collect();
    assert_eq!(pages, expected);
    unsafe {
        linux_call(g, 3, pm as usize, 0, 0);
        linux_call(g, 3, fd as usize, 0, 0);
        assert_ne!(VirtualFree(p.cast(), 0, 0x8000), 0);
    }
}
