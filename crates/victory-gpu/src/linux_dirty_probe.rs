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
    }
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
