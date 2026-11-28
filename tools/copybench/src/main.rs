//! copybw: replays the frame's copy pattern (base-fx-b counters) to attribute
//! the 185.5 ms the frame line spends in "copies" + "copying back".
//!
//! Legs, each can be run alone or together, with `--contend` for the title's
//! address-space churn (commit + first touch + free of 32 MB blocks) in 8 other
//! threads, which is what the world does while our copying thread works:
//!
//!   a  copies      14700 x ReadProcessMemory, sizes as measured (mean 470 B)
//!   b  watches      2409 x GetWriteWatch(RESET) over 10.7 GB of spans (mean 4.5 MB)
//!   c  copyback    409.9 MB declared, 20.5% of it GPU-written (84.2 MB)
//!                  old: memcmp piece, then ReadProcessMemory + the three-way
//!                  byte loop + WriteProcessMemory per changed 64 KiB piece
//!                  new: memcmp piece, and when the title did not write there,
//!                  one bulk copy (vectorised) instead of that loop
//!
//!   d  guard probe does a guard (VirtualProtect read-only) on write-watched
//!                  memory reach our handler, and does the kernel's watch still
//!                  see the write?  plus the cost of one guard fault
//!   e  guarded      the same 2409 spans, watched by our own guards instead of
//!                  the kernel: cost per frame at 240 written blocks a frame,
//!                  with the kernel's asking every 8th frame as the verifier
//!   f  rpm pieces  the frame's 14,700 copies, each its own ReadProcessMemory
//!                  into the piece's own host bytes: what coalescing starts from
//!   g  coalesced   the pieces of a run (a site, a page wide, 64 pieces) read in
//!                  one call of their union and cut up here: the same bytes
//!
//! usage: copybw [all|a|b|c|d|e|f|g] [--contend]
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const PAGE: usize = 64 << 10;
const WRITE_WATCH: u32 = 0x0020_0000;
const MIB: usize = 1 << 20;

fn alloc(size: usize) -> *mut u8 {
    let p = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            size,
            MEM_RESERVE | MEM_COMMIT | WRITE_WATCH,
            PAGE_READWRITE,
        )
    } as *mut u8;
    assert!(!p.is_null(), "alloc {size:#x}");
    p
}

fn touch(p: *mut u8, size: usize, one_in: usize) {
    let mut at = 0;
    while at < size {
        unsafe { *p.add(at) = 1 };
        at += 4096 * one_in;
    }
}

fn rpm(src: *const u8, dst: *mut u8, len: usize) {
    let mut done = 0;
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) };
}

/// Whether the kernel read all of it (a page of the source may not be there).
fn rpm_ok(src: *const u8, dst: *mut u8, len: usize) -> bool {
    let mut done = 0;
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

fn write_watch(base: *const u8, len: usize, found: &mut [usize]) -> usize {
    let mut count = found.len();
    let mut g = 0;
    let ok = unsafe { GetWriteWatch(1, base.cast(), len, found.as_mut_ptr().cast(), &mut count, &mut g) } == 0;
    if ok { count } else { 0 }
}

// The guard model (legs d, e): one byte per 64 KiB block, 0 while the block is
// read-only by our hand, 1 once its first write has faulted and been noted.
const STATE_MAX: usize = 1 << 17;
static STATE: AtomicUsize = AtomicUsize::new(0);
static BASE_BLOCK: AtomicUsize = AtomicUsize::new(0);
static FAULTS: AtomicUsize = AtomicUsize::new(0);

fn state(table: *mut u8, first_block: usize) {
    STATE.store(table as usize, Ordering::Relaxed);
    BASE_BLOCK.store(first_block, Ordering::Relaxed);
}

fn block_state(block: usize) -> u8 {
    let base = STATE.load(Ordering::Relaxed) as *const u8;
    let i = block.wrapping_sub(BASE_BLOCK.load(Ordering::Relaxed));
    if base.is_null() || i >= STATE_MAX { return 1; }
    unsafe { *base.add(i) }
}

fn set_block_state(block: usize, v: u8) {
    let base = STATE.load(Ordering::Relaxed) as *mut u8;
    let i = block.wrapping_sub(BASE_BLOCK.load(Ordering::Relaxed));
    if base.is_null() || i >= STATE_MAX { return; }
    unsafe { *base.add(i) = v };
}

fn protect_ro(block: usize) -> bool {
    let mut old = 0u32;
    unsafe { VirtualProtect((block * PAGE) as *const _, PAGE, PAGE_READONLY, &mut old) != 0 }
}

fn protection(at: *const u8) -> u32 {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe { VirtualQuery(at as *const _, &mut info, std::mem::size_of_val(&info)) };
    info.Protect
}

/// The first write to a block we made read-only: note it, give the write
/// access back (what `guards::open` does in the layer).
unsafe extern "system" fn on_write(
    info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    };
    let r = unsafe { &*(*info).ExceptionRecord };
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION || r.NumberParameters < 2 || r.ExceptionInformation[0] != 1 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let block = r.ExceptionInformation[1] / PAGE;
    let mut old = 0u32;
    unsafe { VirtualProtect((block * PAGE) as *const _, PAGE, PAGE_READWRITE, &mut old) };
    set_block_state(block, 1);
    FAULTS.fetch_add(1, Ordering::Relaxed);
    EXCEPTION_CONTINUE_EXECUTION
}

fn install_handler() {
    use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
    unsafe { AddVectoredExceptionHandler(1, Some(on_write)) };
}

/// The frame's 2409 spans (leg b's), as absolute 64 KiB blocks: `(first, last)`.
fn frame_spans(base: usize, size: usize) -> Vec<(usize, usize)> {
    let n = 2409;
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut spans = Vec::with_capacity(n);
    for i in 0..n {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        let s = if i % 5 == 4 {
            (4 * MIB) + (x as usize % (28 * MIB))
        } else {
            (256 << 10) + (x as usize % (4 * MIB - (256 << 10)))
        };
        let at = base + (i * 77_003) % (size - s);
        spans.push((at / PAGE, (at + s).div_ceil(PAGE)));
    }
    spans
}

/// The three-way compare-and-select of `Shadow::copy_back`, per 64 KiB piece.
/// Returns the bytes written back.
unsafe fn loop_three_way(title: *mut u8, host: *mut u8, old: &[u8], n: usize) -> usize {
    let mut changed = false;
    for i in 0..n {
        let h = &mut *host.add(i);
        let t = &mut *title.add(i);
        if *h != old[i] && *t != *h {
            *t = *h;
            changed = true;
        }
        // the shadow takes the title's byte where the GPU did not write
        *h = *t;
    }
    if changed { n } else { 0 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let which = args.first().cloned().unwrap_or_else(|| "all".into());
    let contend = args.iter().any(|a| a == "--contend");
    // `all`, one leg, or several at once (`be`).
    let run = |leg: &str| which == "all" || which.contains(leg);

    let stop = Arc::new(AtomicBool::new(false));
    let mut churn = Vec::new();
    if contend {
        for _ in 0..8 {
            let stop = stop.clone();
            churn.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let p = unsafe {
                        VirtualAlloc(std::ptr::null(), 32 * MIB, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
                    } as *mut u8;
                    if p.is_null() { continue; }
                    touch(p, 32 * MIB, 1);
                    unsafe { VirtualFree(p.cast(), 0, MEM_RELEASE) };
                }
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let title_size = 384 * MIB;
    let title = alloc(title_size);
    touch(title, title_size, 1);
    // the title's bytes: the same as the copy it holds
    unsafe { std::ptr::write_bytes(title, 7, title_size) };
    let mut host: Vec<u8> = vec![7u8; title_size];
    let snapshot: Vec<u8> = host.clone();
    let mut found = vec![0usize; 64 * MIB / 4096];

    if run("a") {
        // 14700 copies a frame, 6.9 MB in all: mean 470 B, from 64 B to 4 KiB.
        let n = 14_700;
        let mut sizes = Vec::with_capacity(n);
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut total = 0;
        for i in 0..n {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            // 3 of 4 tiny (64..1024), the rest up to 4 KiB; mean lands near 470 B
            let len = if i % 4 == 3 { 1024 + (x as usize % ((4 << 10) - 1023)) } else { 64 + (x as usize % (1024 - 63)) };
            sizes.push(len);
            total += len;
        }
        let mut dst = vec![0u8; 8 << 10];
        let started = Instant::now();
        for (i, &len) in sizes.iter().enumerate() {
            let at = (i * 8191) % (title_size - 8192);
            rpm(unsafe { title.add(at) }, dst.as_mut_ptr(), len);
            std::hint::black_box(&dst[..len]);
        }
        let us = started.elapsed().as_secs_f64() * 1e6;
        println!(
            "a copies     : {n} calls, {:.2} MB, {:.1} ms, {:.2} us/call",
            total as f64 / 1e6,
            us as f64 / 1e3,
            us / n as f64
        );
    }

    if run("b") {
        // 2409 askings, 10.7 GB of spans: 4 of 5 in 256 KiB..4 MiB, the rest to 32 MiB.
        let n = 2409;
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut spans = Vec::with_capacity(n);
        let mut total = 0;
        for i in 0..n {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            let s = if i % 5 == 4 {
                (4 * MIB) + (x as usize % (28 * MIB))
            } else {
                (256 << 10) + (x as usize % (4 * MIB - (256 << 10)))
            };
            spans.push(s);
            total += s;
        }
        let started = Instant::now();
        for (i, &s) in spans.iter().enumerate() {
            let at = (i * 77_003) % (title_size - s);
            let _ = write_watch(unsafe { title.add(at) }, s, &mut found);
        }
        let us = started.elapsed().as_secs_f64() * 1e6;
        println!(
            "b watches    : {n} calls, {:.2} MB, {:.1} ms, {:.1} us/call",
            total as f64 / 1e6,
            us as f64 / 1e3,
            us / n as f64
        );
    }

    if run("c") {
        // 409.9 MB declared, 84.2 MB (20.5%) written by shaders, title writes 4%.
        let declared = title_size;
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for piece in (0..declared).step_by(PAGE) {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            if x % 100 < 21 {
                // the GPU wrote this piece: the shadow moved on
                for i in (0..PAGE).step_by(64) {
                    host[piece + i] = host[piece + i].wrapping_add(3);
                }
            }
            let mut y = x ^ 0xa5a5_a5a5_a5a5_a5a5;
            y ^= y << 13; y ^= y >> 7; y ^= y << 17;
            if y % 100 < 4 {
                for i in (0..PAGE).step_by(128) {
                    unsafe { *title.add(piece + i) = 9 };
                }
            }
        }
        let pieces = declared / PAGE;
        let gen_host = host.clone();
        let mut gen_title = vec![0u8; declared];
        unsafe { std::ptr::copy_nonoverlapping(title, gen_title.as_mut_ptr(), declared) };
        let scale = 409.9 / (declared as f64 / 1e6);

        // old: exactly shadow.rs's snapshot path.
        let started = Instant::now();
        let mut title_buf = vec![0u8; PAGE];
        let mut written = 0;
        for p in 0..pieces {
            let at = p * PAGE;
            let host_at = &host[at..at + PAGE];
            let old_at = &snapshot[at..at + PAGE];
            if host_at == old_at {
                continue;
            }
            let mut d = 0;
            let ok = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    title.add(at).cast(),
                    title_buf.as_mut_ptr().cast(),
                    PAGE,
                    &mut d,
                )
            } != 0;
            if !ok { continue; }
            written += unsafe {
                loop_three_way(title.add(at), host.as_mut_ptr().add(at), old_at, PAGE)
            };
        }
        let old_ms = started.elapsed().as_secs_f64() * 1e3 * scale;
        println!("c old        : {:.1} ms for {:.1} MB written ({:.1} MB declared, x{scale:.2})", old_ms, written as f64 / 1e6, declared as f64 / 1e6);

        // new: memcmp the title against the snapshot; untouched -> one bulk copy.
        unsafe { std::ptr::copy_nonoverlapping(gen_title.as_ptr(), title, declared) };
        let mut host2 = gen_host.clone();
        let started = Instant::now();
        let mut written2 = 0;
        let mut bulk = 0;
        let mut mixed = 0;
        let mut short = 0;
        for p in 0..pieces {
            let at = p * PAGE;
            if host2[at..at + PAGE] == snapshot[at..at + PAGE] {
                continue;
            }
            let mut d = 0;
            let ok = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    title.add(at).cast(),
                    title_buf.as_mut_ptr().cast(),
                    PAGE,
                    &mut d,
                )
            } != 0;
            if !ok { continue; }
            if d < PAGE { short += 1; }
            if title_buf[..] == snapshot[at..at + PAGE] {
                // the title did not write here: put the whole piece back
                unsafe {
                    std::ptr::copy_nonoverlapping(host2.as_ptr().add(at), title.add(at), PAGE)
                };
                written2 += PAGE;
                bulk += PAGE;
            } else {
                mixed += PAGE;
                written2 += unsafe {
                    loop_three_way(title.add(at), host2.as_mut_ptr().add(at), &snapshot[at..at + PAGE], PAGE)
                };
            }
        }
        let new_ms = started.elapsed().as_secs_f64() * 1e3 * scale;
        println!("c new        : {:.1} ms for {:.1} MB written ({:.1} MB of it bulk, {:.1} MB three-way, {short} short reads)", new_ms, written2 as f64 / 1e6, bulk as f64 / 1e6, mixed as f64 / 1e6);

        // and the write leg alone, the way the old build did it
        let started = Instant::now();
        for p in (0..pieces).step_by(5) {
            let at = p * PAGE;
            let mut d = 0;
            unsafe {
                WriteProcessMemory(
                    GetCurrentProcess(),
                    title.add(at).cast(),
                    host.as_ptr().add(at).cast(),
                    PAGE,
                    &mut d,
                )
            };
        }
        let wpm = started.elapsed().as_secs_f64() * 1e3 * scale;
        let started = Instant::now();
        for p in (0..pieces).step_by(5) {
            let at = p * PAGE;
            unsafe {
                std::ptr::copy_nonoverlapping(host.as_ptr().add(at), title.add(at), PAGE)
            };
        }
        let plain = started.elapsed().as_secs_f64() * 1e3 * scale;
        println!(
            "c write leg  : WriteProcessMemory {:.1} ms vs plain {:.1} ms for {:.1} MB",
            wpm,
            plain,
            (pieces as f64 / 5.0 * PAGE as f64 / 1e6) * scale
        );
    }

    if run("d") {
        // Does a guard on write-watched memory reach our handler at all, and
        // does the kernel's watch still see the page after we gave the write
        // back? (If Wine's own watch handler swallows the fault, guarding a
        // write-watched pool is useless and the asking must stay.)
        let size = 8 * MIB;
        let p = alloc(size);
        touch(p, size, 1);
        let blocks = size / PAGE;
        let table = Box::leak(vec![0u8; blocks].into_boxed_slice());
        state(table.as_mut_ptr(), p as usize / PAGE);
        install_handler();
        // The first touch's writes are the baseline.
        let _ = write_watch(p, size, &mut found);
        let before = protection(p);
        let ro = protect_ro(p as usize / PAGE);
        set_block_state(p as usize / PAGE, 0);
        FAULTS.store(0, Ordering::Relaxed);
        unsafe { *p = 9 };
        let faults = FAULTS.load(Ordering::Relaxed);
        let after = protection(p);
        let n = write_watch(p, size, &mut found);
        let saw = (0..n).any(|k| found[k] == p as usize);
        println!(
            "d guard probe: VirtualProtect {ro}, protection {before:#x} -> {after:#x}, our handler ran \
             {faults}x on the write, the kernel's watch sees that page: {saw} ({n} pages reported)"
        );
        // The cost of one guard fault: protect a block, write it, repeat.
        let n = 120;
        FAULTS.store(0, Ordering::Relaxed);
        let started = Instant::now();
        for i in 1..=n {
            let at = i * PAGE;
            let block = (p as usize + at) / PAGE;
            protect_ro(block);
            set_block_state(block, 0);
            unsafe { *p.add(at) = 7 };
        }
        let us = started.elapsed().as_secs_f64() * 1e6;
        println!(
            "d fault cost : {:.2} us per guard fault over {n} blocks ({:.1} ms, {} faults)",
            us / n as f64,
            us / 1e3,
            FAULTS.load(Ordering::Relaxed)
        );
    }

    if run("e") {
        // The frame's 2409 spans watched by our own guards: every block
        // read-only until its first write faults; an asking about a span with
        // no opened block costs a search, one with opened blocks re-protects
        // just those. `writes` a frame: 240 blocks (the 4% of 384 MB the
        // copy-back bench assumes the title writes).
        let size = title_size;
        let blocks = size / PAGE;
        let table = Box::leak(vec![0u8; blocks].into_boxed_slice());
        state(table.as_mut_ptr(), title as usize / PAGE);
        install_handler();
        // Every block counts as written at the start: the first frame guards
        // them all, as the first asking would.
        let mut dirty: BTreeSet<usize> = (title as usize / PAGE..title as usize / PAGE + blocks).collect();
        let spans = frame_spans(title as usize, size);
        let mb = spans.iter().map(|&(a, b)| (b - a) * PAGE).sum::<usize>() as f64 / 1e6;
        let mut x: u64 = 0x5d5d_1234_5678_9abc;
        let write = |block: usize| {
            if block_state(block) == 0 {
                // Still read-only: the write faults, the handler opens it.
                unsafe { *((block * PAGE) as *mut u8) = 1 };
            }
            block_state(block) == 1
        };
        const ROUNDS: usize = 8;
        const PER_FRAME: usize = 240;
        let mut reasks = 0;
        let mut wrote = 0;
        let mut us = 0.0f64;
        let mut verify = 0.0f64;
        for round in 0..ROUNDS {
            // The title's writes of this frame.
            for _ in 0..PER_FRAME {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                let block = title as usize / PAGE + (x as usize % blocks);
                if write(block) {
                    wrote += 1;
                    dirty.insert(block);
                }
            }
            let started = Instant::now();
            for &(first, last) in &spans {
                if dirty.range(first..last).next().is_none() {
                    continue;
                }
                let mine: Vec<usize> = dirty.range(first..last).copied().collect();
                for block in mine {
                    protect_ro(block);
                    set_block_state(block, 0);
                    dirty.remove(&block);
                }
                reasks += 1;
            }
            us += started.elapsed().as_secs_f64() * 1e6;
            if round % 8 == 0 {
                // The kernel's asking as the verifier: it sees writes the
                // guards cannot, and costs what leg b costs.
                let started = Instant::now();
                for &(first, last) in &spans {
                    let _ = write_watch((first * PAGE) as *const u8, (last - first) * PAGE, &mut found);
                }
                verify += started.elapsed().as_secs_f64() * 1e6;
            }
        }
        let _ = table;
        let frames = ROUNDS as f64;
        println!(
            "e guarded    : {:.1} ms/frame over {} spans ({mb:.1} MB), {:.0} re-guard askings and {:.0} writes a \
             frame; the verifier {:.1} ms every 8th frame = {:.1} ms/frame",
            us / 1e3 / frames,
            2409,
            reasks as f64 / frames,
            wrote as f64 / frames,
            verify / 1e3,
            verify / 1e3 / 8.0
        );
    }

    if run("f") || run("g") {
        // The frame's 14,700 pieces of title memory, 6.9 MB: mean 470 B. The
        // layer's copies that lie close together, in clusters of a buffer's
        // pieces (gaps of 8..256 B: a copy that went on exactly where the last
        // ended is one step already, `defer_copy`), which is what coalescing
        // can join.
        let n = 14_700;
        let mut pieces: Vec<(usize, usize)> = Vec::with_capacity(n);
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut total = 0;
        let mut at = 0;
        let mut cluster_left = 0;
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Mean 461 B: 6.8 MB in 14,700 pieces, as the frame line counts.
            let len = if i % 5 == 4 {
                512 + (x as usize % (1600 - 511))
            } else {
                64 + (x as usize % (560 - 63))
            };
            if cluster_left == 0 {
                // A cluster of a buffer's pieces, inside one page.
                at += (x as usize % 4096) + 4096;
                cluster_left = 18 + (x as usize % 12);
            }
            total += len;
            pieces.push((at, len));
            at += len + 8 + (x as usize % 248);
            cluster_left -= 1;
        }
        assert!(at < title_size, "the pieces fit the title region");
        let span: usize = pieces.last().unwrap().0 + pieces.last().unwrap().1;
        let mb = total as f64 / 1e6;

        // `f`: one `ReadProcessMemory` a piece, into the piece's own host
        // bytes: what the layer did before coalescing.
        let mut one = vec![0u8; span];
        let started = Instant::now();
        for &(src, len) in &pieces {
            rpm(unsafe { title.add(src) }, unsafe { one.as_mut_ptr().add(src) }, len);
        }
        let one_ms = started.elapsed().as_secs_f64() * 1e3;

        // `g`: the pieces of a run read in one call (their union), cut up in
        // host memory here; the run rule is the layer's (a site, a page wide,
        // 64 pieces).
        let mut together = vec![0u8; span];
        let mut scratch = vec![0u8; 64 << 10];
        let started = Instant::now();
        let (mut runs, mut calls, mut served) = (0, 0, 0);
        let mut i = 0;
        while i < pieces.len() {
            let (mut lo, mut hi) = pieces[i];
            hi += lo; // (src, len) -> lo, hi
            let mut end = i + 1;
            while end < pieces.len() && end - i < 64 {
                let (src, len) = pieces[end];
                let (nlo, nhi) = (lo.min(src), hi.max(src + len));
                if nhi - nlo > 64 << 10 {
                    break;
                }
                (lo, hi) = (nlo, nhi);
                end += 1;
            }
            calls += 1;
            runs += 1;
            served += end - i;
            let span = hi - lo;
            if rpm_ok(unsafe { title.add(lo) }, scratch.as_mut_ptr(), span) {
                for &(src, len) in &pieces[i..end] {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            scratch.as_ptr().add(src - lo),
                            together.as_mut_ptr().add(src),
                            len,
                        )
                    };
                }
            } else {
                for &(src, len) in &pieces[i..end] {
                    rpm(unsafe { title.add(src) }, unsafe { together.as_mut_ptr().add(src) }, len);
                }
            }
            i = end;
        }
        let together_ms = started.elapsed().as_secs_f64() * 1e3;
        assert!(
            one.iter().zip(&together).all(|(&a, &b)| a == b),
            "the coalesced read gave other bytes"
        );
        println!(
            "f rpm pieces : {n} calls, {mb:.2} MB, {one_ms:.2} ms, {:.2} us/call",
            one_ms * 1e3 / n as f64
        );
        println!(
            "g coalesced  : {calls} calls for {served} pieces in {runs} runs ({:.1} a run), {mb:.2} MB, \
             {together_ms:.2} ms, {:.2} us/piece -> x{:.1}",
            served as f64 / runs as f64,
            together_ms * 1e3 / n as f64,
            one_ms / together_ms
        );
    }

    stop.store(true, Ordering::Relaxed);
    for t in churn {
        let _ = t.join();
    }
}
