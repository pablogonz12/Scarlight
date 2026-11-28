//! Names the faulting code when title code crashes.
//!
//! A vectored handler sees every exception first, so it only prints the
//! kinds that end a process (access violations and the like, and
//! breakpoints: titles assert with `int3`, fatal without a debugger) as
//! first-chance, since a handler may still take them (ntdll's loader does
//! with its own probes). What nobody handles, of any kind (an uncaught C++
//! exception too), is reported from the unhandled-exception filter
//! (`report_unhandled`, which the runtime calls ahead of the title's own)
//! and appended to the crash log, with the title's last debug output, which
//! often states the reason. Panics of ours go to the crash log too.

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_BREAKPOINT, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, HMODULE,
};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS, IMAGE_RUNTIME_FUNCTION_ENTRY, M128A,
    ReadProcessMemory, RtlLookupFunctionEntry, RtlVirtualUnwind, SetUnhandledExceptionFilter,
    UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
    VirtualQuery,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadStackLimits};

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// Frames listed per report.
pub(crate) const MAX_FRAMES: usize = 40;

/// Memory saved to files when a crash is reported: (address, length, file).
#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

/// Debugging aid: on a reported crash, also write each `[address, address
/// + len)` (readable title memory, e.g. an engine table) to its file.
#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

/// The file crash reports are appended to, besides being printed.
static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Also appends every unhandled-exception report and panic to `file`, so a
/// crash can be read after the console is gone.
pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "VIctory: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

/// Appends a failure that is not an exception (the host GPU's) to the crash
/// log, with the title's last debug output, and prints it.
pub fn log_crash(text: &str) {
    let text = format!("{text}
{}", title_output());
    eprint!("{text}");
    append_to_log(&text);
}

fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new().create(true).append(true).open(file);
    if let Ok(mut f) = opened {
        let _ = write!(f, "unix time {now}, process {}\n{text}\n", std::process::id());
    }
}

/// The title's last lines of debug output, kept for crash reports.
static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

/// Keeps a line of the title's debug output for the next crash report.
pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

/// The kept lines, one per report line; nothing if another thread holds
/// them (it may be the one that faulted).
fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter().map(|l| format!("  title said: {l}\n")).collect()
}

/// Installs the handlers; call once before running title code.
pub fn report_crashes() {
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        unsafe { report_unhandled(info) };
        EXCEPTION_CONTINUE_SEARCH
    }
    // Read the switch now, not first inside a handler.
    unwind_checks_enabled();
    // SAFETY: registers handlers that only read the exception they are
    // given; the title's own filter, once it sets one, takes the place of
    // ours and calls `report_unhandled` first.
    unsafe {
        AddVectoredExceptionHandler(0, Some(handler));
        SetUnhandledExceptionFilter(Some(filter));
    }
}

/// What the vectored handler reports as it happens.
const FATAL: [i32; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_BREAKPOINT,
];

/// Console CPUs are Zen 2: titles use AMD-only instructions Intel CPUs
/// lack. Does their job instead: `clzero` (`0f 01 fc`) zeroes the 64-byte
/// line at `rax`; `monitorx`/`mwaitx` (`0f 01 fa`/`fb`) are wait hints, so
/// a spin loop around them works with mwaitx as a yield.
unsafe fn emulate_amd_only(info: *mut EXCEPTION_POINTERS) -> bool {
    // SAFETY: the system passes valid exception pointers; the faulting
    // instruction's bytes are readable, as the CPU just fetched them.
    unsafe {
        let record = &*(*info).ExceptionRecord;
        let context = &mut *(*info).ContextRecord;
        if record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION {
            return false;
        }
        let (bytes, n) = code_at(context.Rip as usize);
        let bytes = &bytes[..n];
        match bytes.get(..3) {
            Some([0x0f, 0x01, 0xfc]) => ((context.Rax & !63) as *mut u8).write_bytes(0, 64),
            Some([0x0f, 0x01, 0xfa]) => {}
            Some([0x0f, 0x01, 0xfb]) => std::thread::yield_now(),
            _ => {
                if let Some(pass) = clwb_loop(bytes) {
                    // A whole write-back loop in one exception (see `clwb_loop`),
                    // unless its last pass would leave the address space: then
                    // only the `clwb` is skipped and the loop runs at the
                    // CPU's own pace, so `base` never wraps.
                    let limit = *gpr(context, pass.limit);
                    let base = gpr(context, pass.base);
                    if let Some(end) = loop_exit(*base, limit) {
                        *base = end;
                        let flags = cmp_flags(*base, limit);
                        context.EFlags = context.EFlags & !CMP_FLAGS | flags;
                        context.Rip += pass.len as u64;
                        return true;
                    }
                }
                if let Some(len) = clwb_len(bytes) {
                    // A write-back hint: the PC's memory is coherent, so it has nothing to do.
                    context.Rip += len as u64;
                    return true;
                }
                let Some(len) = sse4a(bytes, &mut context.Anonymous.FltSave.XmmRegisters) else {
                    return false;
                };
                context.Rip += len as u64;
                return true;
            }
        }
        context.Rip += 3;
    }
    true
}

/// Up to 32 code bytes from `rip` and how many there are: those left in its
/// page, and the next page's when that page can be read. The faulting
/// instruction's own bytes are always there (the CPU fetched them), but
/// reading past a short instruction at the end of a page into an unmapped
/// one would fault inside this handler.
///
/// # Safety
///
/// `rip`'s page is readable.
unsafe fn code_at(rip: usize) -> ([u8; 32], usize) {
    let mut bytes = [0u8; 32];
    let room = 0x1000 - (rip & 0xfff);
    let n = if room < bytes.len() && readable(rip + room) {
        bytes.len()
    } else {
        room.min(bytes.len())
    };
    // SAFETY: `n` bytes from `rip` are readable, as above.
    unsafe { std::ptr::copy_nonoverlapping(rip as *const u8, bytes.as_mut_ptr(), n) };
    (bytes, n)
}

/// Whether the page at `address` is committed and can be read.
fn readable(address: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_GUARD, PAGE_NOACCESS,
        VirtualQuery,
    };
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not `address` itself.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == MEM_COMMIT
        && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0
        && info.Protect & 0xff != PAGE_EXECUTE
}

/// `clwb m8` (`66 [rex] 0f ae /6`, a memory operand), which Intel CPUs before
/// Ice Lake lack: its length, or `None` for any other instruction, or one
/// `code` does not hold whole.
fn clwb_len(code: &[u8]) -> Option<usize> {
    let at = if *code.get(1)? & 0xf0 == 0x40 { 2 } else { 1 };
    if *code.first()? != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let (mode, rm) = (modrm >> 6, modrm & 7);
    if mode == 3 || (modrm >> 3) & 7 != 6 {
        return None;
    }
    let mut len = at + 3;
    let mut base = rm;
    if rm == 4 {
        base = *code.get(len)? & 7;
        len += 1;
    }
    len += match mode {
        0 if rm == 5 || (rm == 4 && base == 5) => 4,
        1 => 1,
        2 => 4,
        _ => 0,
    };
    (len <= code.len()).then_some(len)
}

/// A `clwb` write-back loop as the title's compilers build it, one cache line a
/// pass (`base` and `limit` any registers):
///
/// ```text
/// top: clwb [base]
///      add  base, 0x40
///      cmp  base, limit
///      jbe  top
/// ```
///
/// An exception costs about a microsecond and the title writes back whole
/// buffers this way while it loads, so the loop is done in one: `clwb` has
/// nothing to do on the PC, `base` ends past `limit` as the last pass leaves it.
struct ClwbLoop {
    base: u8,
    limit: u8,
    /// Bytes from `clwb` to the instruction after `jbe`.
    len: usize,
}

fn clwb_loop(code: &[u8]) -> Option<ClwbLoop> {
    // clwb [base]: 66 [40|41] 0f ae, ModRM mod 00, reg 6, no SIB or displacement.
    let (rex, at) = match code.get(1)? {
        0x40 | 0x41 => (code[1], 2),
        _ => (0, 1),
    };
    if *code.first()? != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let rm = modrm & 7;
    if modrm >> 6 != 0 || (modrm >> 3) & 7 != 6 || rm == 4 || rm == 5 {
        return None;
    }
    let base = rm | (rex & 1) << 3;
    // add base, 0x40: REX.W(+B) 83 /0 ib.
    let add = at + 3;
    if *code.get(add..add + 4)? != [0x48 | base >> 3, 0x83, 0xc0 | (base & 7), 0x40] {
        return None;
    }
    // cmp base, limit: REX.W 39 /r with base in r/m.
    let cmp = add + 4;
    let &[cmp_rex @ 0x48..=0x4f, 0x39, cmp_modrm] = code.get(cmp..cmp + 3)? else {
        return None;
    };
    if cmp_modrm >> 6 != 3 || (cmp_modrm & 7 | (cmp_rex & 1) << 3) != base {
        return None;
    }
    let limit = (cmp_modrm >> 3) & 7 | (cmp_rex & 4) << 1;
    // jbe top.
    let jump = cmp + 3;
    let len = jump + 2;
    if *code.get(jump)? != 0x76 || i64::from(*code.get(jump + 1)? as i8) != -(len as i64) {
        return None;
    }
    (limit != base).then_some(ClwbLoop { base, limit, len })
}

/// Where an emulated write-back loop's last pass leaves `base`: past `limit`
/// by one line. `None` when that would leave the address space (a `base` or
/// `limit` within a line of `u64::MAX`), so the loop is left to the CPU
/// rather than finished with a wrapped `base`.
fn loop_exit(base: u64, limit: u64) -> Option<u64> {
    let passes = if base <= limit {
        (limit - base) / 64 + 1
    } else {
        1
    };
    base.checked_add(passes.checked_mul(64)?)
}

/// The general register numbered `n` as instructions encode it.
fn gpr(context: &mut CONTEXT, n: u8) -> &mut u64 {
    match n & 15 {
        0 => &mut context.Rax,
        1 => &mut context.Rcx,
        2 => &mut context.Rdx,
        3 => &mut context.Rbx,
        4 => &mut context.Rsp,
        5 => &mut context.Rbp,
        6 => &mut context.Rsi,
        7 => &mut context.Rdi,
        8 => &mut context.R8,
        9 => &mut context.R9,
        10 => &mut context.R10,
        11 => &mut context.R11,
        12 => &mut context.R12,
        13 => &mut context.R13,
        14 => &mut context.R14,
        _ => &mut context.R15,
    }
}

/// The status flags `cmp a, b` leaves (CF, PF, AF, ZF, SF, OF) as `EFLAGS` bits.
fn cmp_flags(a: u64, b: u64) -> u32 {
    let r = a.wrapping_sub(b);
    u32::from(a < b)
        | u32::from((r as u8).count_ones().is_multiple_of(2)) << 2
        | (((a ^ b ^ r) >> 4) as u32 & 1) << 4
        | u32::from(r == 0) << 6
        | ((r >> 63) as u32) << 7
        | ((((a ^ b) & (a ^ r)) >> 63) as u32) << 11
}
const CMP_FLAGS: u32 = 0x8d5;

/// SSE4a's `extrq`/`insertq` (register forms, the only ones there are) on
/// the saved XMM registers; returns the instruction's length, or `None` for
/// any other instruction, or one `code` does not hold whole.
fn sse4a(code: &[u8], xmm: &mut [M128A; 16]) -> Option<usize> {
    // Past `code`, a byte no prefix or opcode checked here matches; a
    // length past it is refused below, before any register changes.
    let b = |i: usize| code.get(i).copied().unwrap_or(0);
    let prefix = b(0);
    let (rex, at) = if b(1) & 0xf0 == 0x40 {
        (b(1), 2)
    } else {
        (0, 1)
    };
    if !matches!(prefix, 0x66 | 0xf2) || b(at) != 0x0f || !matches!(b(at + 1), 0x78 | 0x79) {
        return None;
    }
    let modrm = b(at + 2);
    if modrm >> 6 != 3 {
        return None;
    }
    let dst = usize::from((modrm >> 3) & 7 | (rex & 4) << 1);
    let src = usize::from(modrm & 7 | (rex & 1) << 3);
    let immediate = b(at + 1) == 0x78;
    let (fields, len) = if immediate {
        ((b(at + 3), b(at + 4)), at + 5)
    } else {
        // extrq reads its fields from src's low half, insertq from its high.
        let f = if prefix == 0x66 { xmm[src].Low } else { xmm[src].High as u64 };
        ((f as u8, (f >> 8) as u8), at + 3)
    };
    if len > code.len() {
        return None;
    }
    let (n, at_bit) = (u32::from(fields.0 & 63), u32::from(fields.1 & 63));
    let bits = if n == 0 { 64 } else { n };
    let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    if prefix == 0x66 {
        // extrq: register form takes its fields from src, works on dst.
        if immediate && (modrm >> 3) & 7 != 0 {
            return None;
        }
        let target = if immediate { src } else { dst };
        xmm[target].Low = (xmm[target].Low >> at_bit) & mask;
    } else {
        let value = (xmm[src].Low & mask) << at_bit;
        xmm[dst].Low = xmm[dst].Low & !(mask << at_bit) | value;
    }
    Some(len)
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers.
    if unsafe { emulate_amd_only(info) } {
        return -1; // EXCEPTION_CONTINUE_EXECUTION
    }
    // SAFETY: as above.
    if FATAL.contains(&unsafe { (*(*info).ExceptionRecord).ExceptionCode }) {
        // SAFETY: as above.
        unsafe { report("first-chance exception", info, false) };
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// For an unhandled-exception filter: reports the exception that is about
/// to end the process, and appends it to the crash log.
///
/// # Safety
/// `info` comes from the system, for an exception being dispatched.
pub unsafe fn report_unhandled(info: *const EXCEPTION_POINTERS) {
    // SAFETY: forwarded from the caller.
    unsafe { report("unhandled exception", info, true) };
}

/// The bytes at `at` as hex, as many as the longest x86 instruction (15),
/// but none past the end of `at`'s page: the next page may not be mapped.
/// For an instruction the CPU refused, they name it (`0f 01 fd` is
/// `rdpru`), which the address alone does not.
///
/// # Safety
/// `at`'s page is readable.
unsafe fn instruction_bytes(at: usize) -> String {
    let n = (0x1000 - (at & 0xfff)).min(15);
    // SAFETY: `n` bytes from `at` stay inside its page, which the caller
    // says is readable.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, n) };
    let hex: Vec<_> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(" ")
}

/// MSVC's code for a C++ `throw`, and its magic first parameter.
const CPP_EXCEPTION: i32 = 0xE06D_7363_u32 as i32;
const CPP_MAGIC: usize = 0x1993_0520;

/// What a C++ throw threw: the decorated name of its type, then its value
/// where that names the cause (an integer, e.g. an HRESULT; a standard
/// exception's message).
///
/// # Safety
/// `p` are the parameters of a live MSVC throw (`p[0]` the magic).
unsafe fn thrown(p: &[usize]) -> String {
    // SAFETY: the throw's ThrowInfo, its RVAs relative to `p[3]`: catchable
    // type array at +12 (count, then type RVAs), each type's descriptor at
    // +4, whose decorated name starts at +16; the object is at `p[1]`.
    unsafe {
        let rva = |at: usize| (at as *const u32).read_unaligned() as usize;
        let types = p[3] + rva(p[2] + 12);
        let name = |i: usize| {
            let descriptor = p[3] + rva(p[3] + rva(types + 4 + 4 * i) + 4);
            std::ffi::CStr::from_ptr((descriptor + 16) as *const std::ffi::c_char)
                .to_string_lossy()
        };
        let first = name(0);
        let value = match &*first {
            // int, unsigned, long (HRESULT), unsigned long
            ".H" | ".I" | ".J" | ".K" if p[1] != 0 => {
                format!(" = {:#010x}", (p[1] as *const u32).read_unaligned())
            }
            // std::exception keeps its message after the table pointer.
            _ if (0..rva(types)).any(|i| name(i) == ".?AVexception@std@@") => {
                let message = (p[1] as *const *const std::ffi::c_char).add(1).read();
                if message.is_null() {
                    String::new()
                } else {
                    format!(": {}", std::ffi::CStr::from_ptr(message).to_string_lossy())
                }
            }
            _ => String::new(),
        };
        format!("{first}{value}")
    }
}

/// # Safety
/// See [`report_unhandled`].
unsafe fn report(what: &str, info: *const EXCEPTION_POINTERS, to_file: bool) {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    let code = record.ExceptionCode;
    let at = record.ExceptionAddress as usize;
    let p = record.ExceptionInformation;
    let detail = if code == EXCEPTION_ACCESS_VIOLATION {
        let kind = match p[0] {
            0 => "read",
            1 => "write",
            _ => "execute",
        };
        format!(" ({kind} {:#x}: {})", p[1], memory_at(p[1]))
    } else if code == CPP_EXCEPTION && record.NumberParameters >= 4 && p[0] == CPP_MAGIC {
        // SAFETY: a live MSVC throw.
        format!(" (C++ {} thrown)", unsafe { thrown(&p) })
    } else if code == EXCEPTION_ILLEGAL_INSTRUCTION || code == EXCEPTION_PRIV_INSTRUCTION {
        // SAFETY: the CPU refused the instruction at `at`, so it fetched it.
        format!(" (code {})", unsafe { instruction_bytes(at) })
    } else {
        String::new()
    };
    // Printed line by line (a fault while unwinding must not lose the
    // lines before it), and kept for the file.
    let mut text = String::new();
    let mut line = |l: String| {
        eprintln!("{l}");
        text.push_str(&l);
        text.push('\n');
    };
    line(format!(
        "VIctory: {what} {:#010x} at {}{detail}",
        code as u32,
        locate(at)
    ));
    line(format!(
        "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rsp {:#x} rbp {:#x}",
        context.Rax,
        context.Rbx,
        context.Rcx,
        context.Rdx,
        context.Rsi,
        context.Rdi,
        context.Rsp,
        context.Rbp
    ));
    line(format!(
        "  r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
        context.R8,
        context.R9,
        context.R10,
        context.R11,
        context.R12,
        context.R13,
        context.R14,
        context.R15
    ));
    for (i, pc) in unwind(*context).into_iter().enumerate() {
        line(format!("  #{i:<2} {}", locate(pc as usize)));
    }
    // Where every other thread was when the fault happened: an intermittent
    // fault (the title's own fail-fast at +0x2215c35, say) is usually one
    // thread deciding another has made no progress, and only the other
    // threads' stacks say what that one was waiting for.
    #[cfg(all(windows, feature = "diagnostics"))]
    if to_file {
        crate::stacks::dump_stacks();
    }
    if to_file {
        let said = title_output();
        eprint!("{said}");
        text.push_str(&said);
        append_to_log(&text);
    }
    #[cfg(feature = "diagnostics")]
    for (address, len, file) in SAVE.get().into_iter().flatten() {
        // SAFETY: the caller of `save_on_crash` named readable memory.
        let bytes = unsafe { std::slice::from_raw_parts(*address as *const u8, *len) };
        let saved = std::fs::write(file, bytes);
        eprintln!(
            "  saved {address:#x}+{len:#x} to {} ({saved:?})",
            file.display()
        );
    }
}

/// Return addresses of the faulting thread's frames, from the faulting
/// instruction up, by the unwind data of each image (the Windows loader
/// registered it for title images too). Code without unwind data (our
/// thunks, leaf functions) is taken to have its return address at `rsp`;
/// with `VICTORY_UNWIND_CHECKS` on (the default), only at the first frame.
fn unwind(context: CONTEXT) -> Vec<u64> {
    let (mut low, mut high) = (0, 0);
    // SAFETY: writes the current thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    unwind_into(context, low, high, &mut frames, unwind_checks_enabled());
    frames
}

/// A `CONTEXT` at the 16-byte alignment the unwind and capture functions need.
#[repr(C, align(16))]
pub(crate) struct Aligned16(pub(crate) CONTEXT);

/// Whether the stack walk checks each code address, unwind record and stack
/// range before it reads them. Read once: [`report_crashes`] reads it before
/// title code runs, so the exception handler and a walk of a suspended
/// thread never take the environment's lock.
pub(crate) fn unwind_checks_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // VICTORY_UNWIND_CHECKS=0 selects the original stack walk for A/B runs.
    *ON.get_or_init(|| std::env::var_os("VICTORY_UNWIND_CHECKS").is_none_or(|value| value != "0"))
}

/// [`unwind`] for any thread of the process, its stack being `low..high`;
/// pushes no more than `MAX_FRAMES`, so it does not allocate when `frames`
/// has that capacity (another thread may be suspended holding the heap).
pub(crate) fn unwind_into(
    context: CONTEXT,
    low: usize,
    high: usize,
    frames: &mut Vec<u64>,
    checks: bool,
) {
    // `RtlVirtualUnwind` writes the context with 16-byte aligned stores, and
    // windows-sys declares `CONTEXT` without that alignment: a by-value copy on
    // the stack can land 8 bytes off and fault (seen in a debug build).
    let mut aligned = Aligned16(context);
    let context = &mut aligned.0;
    frames.push(context.Rip);
    while frames.len() < MAX_FRAMES {
        // Stop where reading the stack would fault inside this handler.
        let rsp = context.Rsp as usize;
        if rsp < low || rsp.checked_add(8).is_none_or(|end| end > high) {
            break;
        }
        // With checks, code must be executable. Code outside an image (our
        // thunks and stubs, made with `VirtualAlloc`) has no unwind data and
        // is only taken at the first frame, by the leaf rule below.
        let code_image = if checks {
            let Some(region) = readable_region(context.Rip as usize) else {
                break;
            };
            let in_image = region.Type == MEM_IMAGE;
            if !executable(&region) || (!in_image && frames.len() != 1) {
                break;
            }
            in_image.then_some(region.AllocationBase as usize)
        } else {
            None
        };
        let mut image = 0;
        let entry = if checks && code_image.is_none() {
            null_mut()
        } else {
            // SAFETY: looks up the function table entry covering `Rip`, if any.
            unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, null_mut()) }
        };
        if entry.is_null() {
            context.Rip = if checks {
                let mut address = [0; 8];
                if frames.len() != 1
                    || readable_range(rsp, rsp + 8, None).is_none()
                    || !read_memory(rsp, &mut address)
                {
                    break;
                }
                u64::from_le_bytes(address)
            } else {
                // SAFETY: `rsp` is inside this thread's stack.
                unsafe { (rsp as *const u64).read_unaligned() }
            };
            context.Rsp += 8;
        } else {
            if checks
                && (code_image != Some(image as usize)
                    || stack_reads_end(context, image as usize, entry, low, high).is_none())
            {
                break;
            }
            let (mut data, mut frame) = (null_mut(), 0);
            // SAFETY: `entry` belongs to `image` and the context is our aligned copy.
            // The checked path bounds the stack reads and checks the image ranges above.
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    entry,
                    context,
                    &mut data,
                    &mut frame,
                    null_mut(),
                )
            };
        }
        if context.Rip == 0 || (checks && (context.Rsp <= rsp as u64 || context.Rsp >= high as u64))
        {
            break;
        }
        frames.push(context.Rip);
    }
}

struct UnwindInfo {
    stack_bytes: usize,
    save_end: usize,
    saves_xmm: bool,
    frame: u8,
    chained_rva: Option<usize>,
}

/// Bounds the stack accesses described by one x64 UNWIND_INFO record.
fn parse_unwind_info(bytes: &[u8]) -> Option<UnwindInfo> {
    let &[version_flags, prolog, count, frame] = bytes.get(..4)? else {
        return None;
    };
    let flags = version_flags >> 3;
    let chained = flags == 4;
    let register = frame & 15;
    if !matches!(version_flags & 7, 1 | 2)
        || (!chained && flags > 3)
        || (register == 0 && frame != 0)
        || (register != 0 && !nonvolatile(register))
    {
        return None;
    }
    let count = usize::from(count);
    let tail = 4 + count.next_multiple_of(2) * 2;
    let chained_rva = if chained {
        Some(u32::from_le_bytes(bytes.get(tail + 8..tail + 12)?.try_into().ok()?) as usize)
    } else {
        // The handler RVA, when present, follows the padded unwind codes.
        bytes.get(..tail + usize::from(flags != 0) * 4)?;
        None
    };
    let mut codes = bytes.get(4..4 + count * 2)?;
    let mut stack_bytes: usize = 0;
    let mut save_end = 0;
    let mut saves_xmm = false;
    let mut sets_frame = false;
    let mut previous_offset = prolog;
    while !codes.is_empty() {
        let offset = codes[0];
        if offset > previous_offset {
            return None;
        }
        previous_offset = offset;
        let (op, info) = (codes[1] & 15, codes[1] >> 4);
        codes = &codes[2..];
        let (extra, growth) = match op {
            // Chained records can add saves but cannot move RSP.
            0 if !chained && nonvolatile(info) && (info != register || sets_frame) => (0, 8),
            1 if !chained && info == 0 => (2, unwind_operand(codes, 2)? * 8),
            1 if !chained && info == 1 => (4, unwind_operand(codes, 4)?),
            2 if !chained => (0, (usize::from(info) + 1) * 8),
            3 if !chained && info == 0 && register != 0 && !sets_frame => {
                sets_frame = true;
                (0, 0)
            }
            4 | 5 | 8 | 9 => {
                let xmm = op >= 8;
                let valid_register = if xmm {
                    info >= 6
                } else {
                    nonvolatile(info) && (info != register || sets_frame)
                };
                if !valid_register {
                    return None;
                }
                let width = if xmm { 16 } else { 8 };
                let extra = if op & 1 == 0 { 2 } else { 4 };
                let mut offset = unwind_operand(codes, extra)?;
                if extra == 2 {
                    offset *= width;
                }
                if !offset.is_multiple_of(width) {
                    return None;
                }
                save_end = save_end.max(offset.checked_add(width)?);
                saves_xmm |= xmm;
                (extra, 0)
            }
            // The machine frame holds the saved RIP and RSP, with an optional error code.
            10 if !chained && info <= 1 && codes.is_empty() => (0, 40 + usize::from(info) * 8),
            _ => return None,
        };
        if !growth.is_multiple_of(8) {
            return None;
        }
        stack_bytes = stack_bytes.checked_add(growth)?;
        codes = codes.get(extra..)?;
    }
    if !chained && sets_frame != (register != 0) {
        return None;
    }
    Some(UnwindInfo {
        stack_bytes,
        save_end,
        saves_xmm,
        frame,
        chained_rva,
    })
}

fn unwind_operand(bytes: &[u8], width: usize) -> Option<usize> {
    let mut value = [0; 4];
    value[..width].copy_from_slice(bytes.get(..width)?);
    Some(u32::from_le_bytes(value) as usize)
}

fn nonvolatile(register: u8) -> bool {
    matches!(register, 3 | 5..=7 | 12..=15)
}

fn frame_register(context: &CONTEXT, register: u8) -> Option<usize> {
    Some(match register {
        3 => context.Rbx,
        5 => context.Rbp,
        6 => context.Rsi,
        7 => context.Rdi,
        12 => context.R12,
        13 => context.R13,
        14 => context.R14,
        15 => context.R15,
        _ => return None,
    } as usize)
}

fn readable_region(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut region = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: queries metadata for an address and writes only our local buffer.
    let read = unsafe { VirtualQuery(address as *const _, &mut region, size_of_val(&region)) };
    let readable = matches!(
        region.Protect & 0xff,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    );
    (read != 0 && region.State == MEM_COMMIT && region.Protect & PAGE_GUARD == 0 && readable)
        .then_some(region)
}

fn executable(region: &MEMORY_BASIC_INFORMATION) -> bool {
    matches!(
        region.Protect & 0xff,
        PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    )
}

fn readable_range(mut start: usize, end: usize, image: Option<usize>) -> Option<()> {
    while start < end {
        let region = readable_region(start)?;
        if image
            .is_some_and(|base| region.Type != MEM_IMAGE || region.AllocationBase as usize != base)
        {
            return None;
        }
        start = (region.BaseAddress as usize).checked_add(region.RegionSize)?;
    }
    Some(())
}

fn read_memory(address: usize, bytes: &mut [u8]) -> bool {
    // SAFETY: Windows copies into our slice and reports failure for unreadable memory.
    unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const _,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            null_mut(),
        ) != 0
    }
}

fn read_image(image: usize, address: usize, bytes: &mut [u8]) -> Option<()> {
    readable_range(address, address.checked_add(bytes.len())?, Some(image))?;
    read_memory(address, bytes).then_some(())
}

fn stack_reads_end(
    context: &CONTEXT,
    image: usize,
    entry: *const IMAGE_RUNTIME_FUNCTION_ENTRY,
    low: usize,
    high: usize,
) -> Option<usize> {
    let mut function = [0; 12];
    read_image(image, entry as usize, &mut function)?;
    let pc = context.Rip as usize;
    let rva = pc.checked_sub(image)?;
    let begin = unwind_operand(&function, 4)?;
    let end = unwind_operand(&function[4..], 4)?;
    if !(begin..end).contains(&rva) {
        return None;
    }

    let mut instruction = [0; 8];
    read_image(image, pc, &mut instruction)?;
    // Version 1 can simulate an epilogue directly from code. These prefixes can
    // move RSP by amounts that the unwind record alone does not establish.
    let opcode = if matches!(instruction[0], 0x40..=0x4f) {
        instruction[1]
    } else {
        instruction[0]
    };
    if matches!(opcode, 0x58..=0x5f)
        || (matches!(instruction[0], 0x48..=0x4f)
            && (opcode == 0x8d && instruction[2] & 0x38 == 0x20
                || matches!(opcode, 0x81 | 0x83) && instruction[2] == 0xc4))
    {
        return None;
    }

    let mut unwind_rva = unwind_operand(&function[8..], 4)?;
    let rsp = context.Rsp as usize;
    let mut frame = None;
    let mut save_end = 0;
    // A limit also stops cycles in damaged chained records.
    for _ in 0..32 {
        if unwind_rva == 0 || !unwind_rva.is_multiple_of(4) {
            return None;
        }
        let address = image.checked_add(unwind_rva)?;
        let mut header = [0; 4];
        read_image(image, address, &mut header)?;
        let flags = header[0] >> 3;
        let size = 4
            + usize::from(header[2]).next_multiple_of(2) * 2
            + match flags {
                0 => 0,
                1..=3 => 4,
                4 => 12,
                _ => return None,
            };
        let mut bytes = [0; 4 + 256 * 2 + 12];
        let bytes = &mut bytes[..size];
        read_image(image, address, bytes)?;
        let info = parse_unwind_info(bytes)?;
        if *frame.get_or_insert(info.frame) != info.frame {
            return None;
        }
        // Windows subtracts the scaled offset from the register to recover
        // the fixed frame base. In a partial prologue it may use RSP instead.
        let base = if info.frame == 0 {
            rsp
        } else {
            let register = frame_register(context, info.frame & 15)?;
            if !(low..high).contains(&register) {
                return None;
            }
            register.checked_sub(usize::from(info.frame >> 4) * 16)?
        };
        if !(low..high).contains(&base)
            || (info.saves_xmm && (!base.is_multiple_of(16) || !rsp.is_multiple_of(16)))
        {
            return None;
        }
        save_end = save_end.max(info.save_end);
        if let Some(next) = info.chained_rva {
            unwind_rva = next;
            continue;
        }
        let span = info.stack_bytes.checked_add(8)?.max(save_end);
        let end = rsp.max(base).checked_add(span)?;
        if end > high {
            return None;
        }
        readable_range(rsp.min(base), end, None)?;
        return Some(end);
    }
    None
}

/// `module+rva`, or the bare address.
pub fn locate(address: usize) -> String {
    module_of(address).unwrap_or_else(|| format!("{address:#x}"))
}

fn module_of(address: usize) -> Option<String> {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: looks up the module containing `address`; no reference taken.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 {
        return None;
    }
    let mut path = [0u16; 260];
    // SAFETY: `module` is a loaded module; the buffer length is passed.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) };
    let path = String::from_utf16_lossy(&path[..len as usize]);
    let name = path.rsplit('\\').next().unwrap_or(&path);
    Some(format!("{name}+{:#x}", address - module as usize))
}

/// What is at `address`, for an access violation there: nothing, memory
/// reserved but never committed, a guard page, or the protection the access
/// broke (with the module, for an image's pages). Each points at a different
/// cause: a wild pointer, an allocation whose commit failed, a stack
/// overrun, a write to code or constants.
fn memory_at(address: usize) -> String {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_IMAGE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE,
        PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD,
        PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not `address` itself.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 {
        // Above the process's address space: Windows reports a
        // non-canonical address as all ones.
        return "outside the address space".into();
    }
    let protect = match info.State {
        MEM_FREE => return "nothing mapped there".into(),
        MEM_RESERVE => return "reserved, never committed".into(),
        _ if info.Protect & PAGE_GUARD != 0 => return "a guard page".into(),
        _ => match info.Protect & 0xff {
            PAGE_NOACCESS => "no access".into(),
            PAGE_READONLY => "read-only".into(),
            PAGE_READWRITE => "read-write".into(),
            PAGE_WRITECOPY => "copy-on-write".into(),
            PAGE_EXECUTE => "execute-only".into(),
            PAGE_EXECUTE_READ => "read-execute".into(),
            PAGE_EXECUTE_READWRITE => "read-write-execute".into(),
            PAGE_EXECUTE_WRITECOPY => "copy-on-write, executable".into(),
            _ => format!("protection {:#x}", info.Protect),
        },
    };
    match module_of(address) {
        Some(module) if info.Type == MEM_IMAGE => format!("{protect}, in {module}"),
        _ => protect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_RECORD, RtlCaptureContext, RtlCaptureStackBackTrace,
    };

    #[test]
    fn bounds_unwind_allocations_and_saves() {
        // SET_FPREG rbp,16; ALLOC_SMALL 32; PUSH_NONVOL rbp.
        let info = parse_unwind_info(&[1, 9, 3, 0x15, 9, 3, 5, 0x32, 1, 0x50, 0, 0]).unwrap();
        assert_eq!(info.frame, 0x15);
        assert_eq!(info.stack_bytes, 40);

        // SAVE_XMM128 xmm6 at 48; ALLOC_LARGE (scaled) 64.
        let info = parse_unwind_info(&[1, 9, 4, 0, 9, 0x68, 3, 0, 5, 1, 8, 0]).unwrap();
        assert_eq!(info.stack_bytes, 64);
        assert_eq!(info.save_end, 64);
        assert!(info.saves_xmm);

        // SAVE_NONVOL_FAR rbx at 256; ALLOC_LARGE (unscaled) 128.
        let info =
            parse_unwind_info(&[1, 9, 6, 0, 9, 0x35, 0, 1, 0, 0, 5, 0x11, 128, 0, 0, 0]).unwrap();
        assert_eq!(info.stack_bytes, 128);
        assert_eq!(info.save_end, 264);
    }

    #[test]
    fn bounds_machine_frames_and_reads_chained_rva_after_padding() {
        for (op, size) in [(0x0a, 40), (0x1a, 48)] {
            let info = parse_unwind_info(&[1, 0, 1, 0, 0, op, 0, 0]).unwrap();
            assert_eq!(info.stack_bytes, size);
        }
        // One save consumes two slots, and adding a second far save uses
        // three more slots, so the chained function entry starts after padding.
        let info = parse_unwind_info(&[
            0x21, 9, 5, 0, 9, 0x34, 4, 0, 5, 0x65, 40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x34,
            0x12, 0, 0,
        ])
        .unwrap();
        assert_eq!(info.save_end, 48);
        assert_eq!(info.chained_rva, Some(0x1234));
    }

    #[test]
    fn rejects_unwind_codes_without_known_stack_bounds() {
        for bytes in [
            &[3, 0, 0, 0][..],               // Unsupported version.
            &[1, 1, 1, 0, 1, 6, 0, 0],       // Unsupported opcode.
            &[1, 1, 1, 0, 1, 0x11, 0, 0],    // Allocation operand cut short.
            &[1, 1, 1, 0, 1, 0x34, 0, 0],    // Save operand cut short.
            &[1, 1, 1, 0x15, 1, 0x02, 0, 0], // Missing frame setup.
            &[0x21, 0, 0, 0, 0, 0, 0, 0],    // Chained entry cut short.
        ] {
            assert!(parse_unwind_info(bytes).is_none(), "{bytes:?}");
        }
    }

    #[test]
    fn stops_at_a_pc_outside_an_image() {
        let pc = unwinds_like_the_system_walk as *const () as u64;
        let stack = [pc, pc];
        let low = stack.as_ptr() as usize;
        // SAFETY: zeroed is a valid CONTEXT. Only RIP and RSP are used in this test.
        let mut context: CONTEXT = unsafe { std::mem::zeroed() };
        context.Rip = low as u64;
        context.Rsp = low as u64;
        let mut frames = Vec::with_capacity(MAX_FRAMES);
        unwind_into(context, low, low + size_of_val(&stack), &mut frames, true);
        assert_eq!(frames, [low as u64]);
    }

    #[test]
    fn takes_the_caller_of_a_first_frame_in_our_own_stub_memory() {
        use windows_sys::Win32::System::Memory::{
            MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree,
        };
        // Executable memory outside any image, as our thunks and stubs are made.
        // SAFETY: reserves and commits one fresh page; nothing runs in it.
        let stub = unsafe {
            VirtualAlloc(
                null_mut(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        assert!(!stub.is_null());
        let pc = unwinds_like_the_system_walk as *const () as u64;
        let stack = [pc, pc];
        let low = stack.as_ptr() as usize;
        // SAFETY: zeroed is a valid CONTEXT. Only RIP and RSP are used in this test.
        let mut context: CONTEXT = unsafe { std::mem::zeroed() };
        context.Rip = stub as u64;
        context.Rsp = low as u64;
        let mut frames = Vec::with_capacity(MAX_FRAMES);
        unwind_into(context, low, low + size_of_val(&stack), &mut frames, true);
        // SAFETY: frees the page reserved above.
        unsafe { VirtualFree(stub, 0, MEM_RELEASE) };
        // The caller comes from the leaf rule; the walk may stop after it,
        // since the rest of the caller's frame is not in `stack`.
        assert!(frames.starts_with(&[stub as u64, pc]), "{frames:x?}");
    }

    #[test]
    #[inline(never)]
    fn unwinds_like_the_system_walk() {
        // SAFETY: zeroed CONTEXT is valid; both calls write our buffers.
        let (frames, walked) = unsafe {
            // `RtlCaptureContext` wants it 16-byte aligned too.
            let mut context = Aligned16(std::mem::zeroed());
            RtlCaptureContext(&mut context.0);
            let mut walked = [null_mut(); 8];
            let n = RtlCaptureStackBackTrace(0, 8, walked.as_mut_ptr(), null_mut());
            (unwind(context.0), walked[..usize::from(n)].to_vec())
        };
        // Frame 0 differs (two call sites in this function); callers agree.
        assert!(walked.len() >= 4, "{walked:?}");
        let ours: Vec<_> = frames[1..walked.len()]
            .iter()
            .map(|&f| f as usize)
            .collect();
        let system: Vec<_> = walked[1..].iter().map(|&f| f as usize).collect();
        assert_eq!(ours, system);
    }

    #[test]
    fn memory_at_names_what_an_access_hit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY,
            PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: reserves four pages of ours and commits three of them;
        // nothing reads or writes them.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x4000, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!base.is_null());
            let page = |i: usize, protect| {
                let at = base.cast::<u8>().add(i * 0x1000).cast();
                assert!(!VirtualAlloc(at, 0x1000, MEM_COMMIT, protect).is_null());
            };
            page(1, PAGE_READONLY);
            page(2, PAGE_READWRITE | PAGE_GUARD);
            page(3, PAGE_NOACCESS);
            base as usize
        };
        assert_eq!(memory_at(base + 8), "reserved, never committed");
        assert_eq!(memory_at(base + 0x1008), "read-only");
        assert_eq!(memory_at(base + 0x2008), "a guard page");
        assert_eq!(memory_at(base + 0x3008), "no access");
        // SAFETY: releases the reservation made above.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
        assert_eq!(memory_at(base + 8), "nothing mapped there");
        assert_eq!(memory_at(0), "nothing mapped there");
        assert_eq!(memory_at(usize::MAX), "outside the address space");
        // This test's own code: an image page, named by its module.
        let code = memory_at(memory_at_names_what_an_access_hit as *const () as usize);
        let named = code.starts_with("read-execute, in victory_loader-");
        assert!(named, "{code}");
    }

    /// On an Intel CPU, the real instructions go through the vectored
    /// handler. On an AMD CPU they run natively, and the tests below test
    /// the emulation.
    #[test]
    fn clwb_loops() {
        // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top
        let a = clwb_loop(&[
            0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
        ])
        .expect("rcx loop");
        assert_eq!((a.base, a.limit, a.len), (1, 0, 13));
        // clwb [r9]; add r9, 0x40; cmp r9, rax; jbe top
        let b = clwb_loop(&[
            0x66, 0x41, 0x0f, 0xae, 0x31, 0x49, 0x83, 0xc1, 0x40, 0x49, 0x39, 0xc1, 0x76, 0xf2,
        ])
        .expect("r9 loop");
        assert_eq!((b.base, b.limit, b.len), (9, 0, 14));
        // A different stride, a jump elsewhere, or a cut-off loop is not one.
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x20, 0x48, 0x39, 0xc1, 0x76, 0xf3
            ])
            .is_none()
        );
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf0
            ])
            .is_none()
        );
        assert!(clwb_loop(&[0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40]).is_none());
        // The flags the last cmp leaves: base just past limit.
        assert_eq!(cmp_flags(0x1040, 0x1000) & 0x41, 0); // CF and ZF clear: the loop ends
        assert_eq!(cmp_flags(0x1000, 0x1000) & 0x40, 0x40);
    }

    /// A `clwb` loop whose last pass would run past `u64::MAX`: `base` is not
    /// moved, only the `clwb` is emulated, and the loop then runs one
    /// instruction at a time (#2555).
    #[test]
    fn a_write_back_loop_at_the_top_of_the_address_space_is_left_to_the_cpu() {
        assert_eq!(loop_exit(0x1000, 0x1000), Some(0x1040));
        assert_eq!(loop_exit(0x1040, 0x1000), Some(0x1080));
        // One pass or many, the end is past `u64::MAX`: no fast path.
        assert_eq!(loop_exit(u64::MAX - 0x3f, u64::MAX), None);
        assert_eq!(loop_exit(u64::MAX, 0), None);

        // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top, with `rax` at the
        // top of the address space.
        let code = [
            0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
        ];
        let mut bytes = [0xcc_u8; 32];
        bytes[..code.len()].copy_from_slice(&code);
        assert!(clwb_loop(&bytes).is_some());
        // SAFETY: a zeroed record and context are valid; the emulation reads
        // only `bytes` and writes only `context` (the loop's `clwb` touches
        // no memory).
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = bytes.as_ptr() as u64;
            context.Rax = u64::MAX;
            context.Rcx = 0x1000;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            assert!(emulate_amd_only(&mut pointers));
            // Past the `clwb` only, and `rcx` as it was.
            assert_eq!(context.Rip - bytes.as_ptr() as u64, 4);
            assert_eq!(context.Rcx, 0x1000);
        }
    }

    /// Gives the bytes at `rip` to the emulation as an illegal instruction
    /// there, as [`emulate_as`] does; the bytes are not copied, so what
    /// follows them in memory is what the emulation sees.
    fn emulate_in_place(rip: *const u8) -> Option<u64> {
        // SAFETY: as in `emulate_as`; `rip`'s page is readable.
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = rip as u64;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            emulated.then(|| context.Rip - rip as u64)
        }
    }

    /// Instructions at the end of a page, before one that cannot be read and
    /// before one that can (#924): `clwb` there is emulated, a short
    /// instruction there is read no further than it reaches, and a `clwb`
    /// running on into a readable page is emulated whole.
    #[test]
    fn instructions_at_a_page_end_are_read_only_as_far_as_memory_allows() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: a fresh reservation of two pages, the first committed;
        // freed at the end.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x2000, MEM_RESERVE, PAGE_READWRITE);
            assert!(!base.is_null());
            let page = VirtualAlloc(base, 0x1000, MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!page.is_null());
            let put = |at: usize, code: &[u8]| {
                std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(at), code.len());
                page.add(at).cast_const()
            };

            // The next page reserved only: any read of it would fault.
            let clwb = put(0xffc, &[0x66, 0x0f, 0xae, 0x30]); // clwb [rax]
            assert_eq!(emulate_in_place(clwb), Some(4));
            let ud2 = put(0xffe, &[0x0f, 0x0b]);
            assert_eq!(emulate_in_place(ud2), None);
            let last = put(0xfff, &[0x90]);
            assert_eq!(emulate_in_place(last), None);
            // A clwb [rax+disp32] cut off by the page end is not one.
            let cut = put(0xffc, &[0x66, 0x0f, 0xae, 0xb0]);
            assert_eq!(emulate_in_place(cut), None);

            // The next page readable: an instruction across the boundary is
            // read whole.
            let next = base.cast::<u8>().add(0x1000);
            let next = VirtualAlloc(next.cast(), 0x1000, MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!next.is_null());
            next.copy_from_nonoverlapping([0, 1, 0, 0].as_ptr(), 4);
            assert_eq!(emulate_in_place(cut), Some(8)); // clwb [rax+0x100]

            assert_ne!(VirtualFree(base, 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn clwb_lengths() {
        let pad = |b: &[u8]| {
            let mut c = [0u8; 15];
            c[..b.len()].copy_from_slice(b);
            c
        };
        assert_eq!(clwb_len(&pad(&[0x66, 0x41, 0x0f, 0xae, 0x31])), Some(5)); // clwb [r9]
        assert_eq!(clwb_len(&pad(&[0x66, 0x0f, 0xae, 0x30])), Some(4)); // clwb [rax]
        assert_eq!(clwb_len(&pad(&[0x66, 0x0f, 0xae, 0x70, 0x40])), Some(5)); // clwb [rax+0x40]
        assert_eq!(
            clwb_len(&pad(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0, 0])),
            Some(8)
        ); // [rax+disp32]
        assert_eq!(clwb_len(&pad(&[0x66, 0x0f, 0xae, 0x34, 0x24])), Some(5)); // clwb [rsp]
        assert_eq!(
            clwb_len(&pad(&[0x66, 0x0f, 0xae, 0x35, 0, 0, 0, 0])),
            Some(8)
        ); // [rip+disp32]
        assert_eq!(clwb_len(&pad(&[0x66, 0x0f, 0xae, 0x38])), None); // clflushopt, /7
        assert_eq!(clwb_len(&pad(&[0x0f, 0xae, 0x30])), None); // xsaveopt, no 66
        assert_eq!(clwb_len(&pad(&[0x66, 0x0f, 0xae, 0xf0])), None); // register form
    }

    #[test]
    fn clzero_zeroes_its_line_on_any_cpu() {
        report_crashes();
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5); // inside the first line
        // SAFETY: clzero writes only the 64-byte line holding `at`, ours.
        unsafe { std::arch::asm!(".byte 0x0f, 0x01, 0xfc", in("rax") at) };
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
        // insertq xmm0, xmm1, len 16, at 16; then extrq xmm0, len 8, at 20.
        let out: u64;
        // SAFETY: touches only the named registers.
        unsafe {
            std::arch::asm!(
                "movq xmm0, {a}", "movq xmm1, {b}",
                ".byte 0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10",
                "movq {a}, xmm0",
                ".byte 0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14",
                "movq {b}, xmm0",
                a = inout(reg) 0x1111_2222_3333_4444u64 => _, b = inout(reg) 0xabcdu64 => out,
                out("xmm0") _, out("xmm1") _)
        };
        assert_eq!(out, 0xbc); // 0x1111_2222_abcd_4444 >> 20, 8 bits
        // SAFETY: monitorx/mwaitx are wait hints that write no memory.
        unsafe {
            std::arch::asm!(".byte 0x0f, 0x01, 0xfa", ".byte 0x0f, 0x01, 0xfb",
                in("rax") at, in("ecx") 0, in("edx") 0)
        };
    }

    fn reg(low: u64, high: u64) -> M128A {
        M128A {
            Low: low,
            High: high as i64,
        }
    }

    /// Gives `code` to the emulation as if it faulted with `exception` at
    /// its first byte, with `rax` and `xmm` as the saved registers. The
    /// emulation does not execute `code`, so this works on any CPU. Returns
    /// the number of bytes that the emulation skipped, or `None` if it did
    /// not emulate the instruction. `xmm` gets the saved registers after.
    fn emulate_as(exception: i32, code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        // `int3` after the instruction, so that a decoder that reads too
        // far sees no valid bytes.
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        let start = bytes.as_ptr() as u64;
        // SAFETY: a zeroed record and context are valid; the emulation
        // reads only `bytes` and writes only `context` (and memory at
        // `rax` for clzero, which the caller owns).
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = exception;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = start;
            context.Rax = rax;
            context.Anonymous.FltSave.XmmRegisters = *xmm;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            *xmm = context.Anonymous.FltSave.XmmRegisters;
            emulated.then(|| context.Rip - start)
        }
    }

    fn emulate(code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        emulate_as(EXCEPTION_ILLEGAL_INSTRUCTION, code, rax, xmm)
    }

    fn low(xmm: &[M128A; 16], i: usize) -> u64 {
        xmm[i].Low
    }

    #[test]
    fn emulated_clzero_zeroes_its_line() {
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5) as u64; // inside the first line
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfc], at, &mut xmm), Some(3));
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn emulated_monitorx_and_mwaitx_are_skipped() {
        let mut xmm = [reg(1, 2); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfa], 0, &mut xmm), Some(3));
        assert_eq!(emulate(&[0x0f, 0x01, 0xfb], 0, &mut xmm), Some(3));
        assert!(xmm.iter().all(|r| r.Low == 1 && r.High == 2));
    }

    #[test]
    fn emulated_insertq_and_extrq_with_immediates() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0x5555);
        xmm[1] = reg(0xabcd, 0x6666);
        // insertq xmm0, xmm1, length 16, index 16
        let insertq = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 1), xmm[1].High), (0xabcd, 0x6666));
        // extrq xmm0, length 8, index 20
        let extrq = [0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_insertq_and_extrq_with_fields_in_a_register() {
        let mut xmm = [reg(0, 0); 16];
        // insertq xmm0, xmm1: length and index in xmm1's high half.
        xmm[0] = reg(0x1111_2222_3333_4444, 0);
        xmm[1] = reg(0xabcd, 0x10_10);
        assert_eq!(emulate(&[0xf2, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        // extrq xmm0, xmm1: length and index in xmm1's low half.
        xmm[1] = reg(0x14_08, 0);
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_sse4a_reaches_xmm8_to_xmm15() {
        let mut xmm = [reg(0, 0); 16];
        xmm[8] = reg(0x1111_2222_3333_4444, 0);
        xmm[9] = reg(0xabcd, 0);
        // insertq xmm8, xmm9, length 16, index 16 (REX.R and REX.B)
        let insertq = [0xf2, 0x45, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 8), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 0), low(&xmm, 1)), (0, 0));
        // extrq xmm9, length 8, index 4 (REX.B)
        let extrq = [0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 9), 0xbc);
        assert_eq!(low(&xmm, 1), 0);
    }

    #[test]
    fn emulated_sse4a_length_0_is_64_bits() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1122_3344_5566_7788, 0);
        // extrq xmm0, length 0 (64), index 0. A non-zero index with
        // length 64 goes past bit 63, and AMD leaves that result undefined.
        assert_eq!(
            emulate(&[0x66, 0x0f, 0x78, 0xc0, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0x1122_3344_5566_7788);
        // insertq xmm0, xmm1, length 0 (64), index 0
        xmm[1] = reg(0xdead_beef_cafe_f00d, 0);
        assert_eq!(
            emulate(&[0xf2, 0x0f, 0x78, 0xc1, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn other_instructions_and_exceptions_are_not_emulated() {
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x0b], 0, &mut xmm), None); // ud2
        // extrq with a memory operand, which does not exist
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0x00], 0, &mut xmm), None);
        // extrq with immediates needs ModRM.reg 0
        let bad_extrq = [0x66, 0x0f, 0x78, 0xc8, 0x08, 0x14];
        assert_eq!(emulate(&bad_extrq, 0, &mut xmm), None);
        // a clzero that did not cause an illegal instruction
        let clzero = [0x0f, 0x01, 0xfc];
        assert_eq!(
            emulate_as(EXCEPTION_ACCESS_VIOLATION, &clzero, 0, &mut xmm),
            None
        );
    }

    #[test]
    fn instruction_bytes_stop_at_the_end_of_their_page() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        // rdpru, then the next instruction (mov rcx, rax) and padding.
        let mut code = [0x90_u8; 16];
        code[..6].copy_from_slice(&[0x0f, 0x01, 0xfd, 0x48, 0x89, 0xc1]);
        // SAFETY: two fresh pages of ours, the second made unreadable, so a
        // read past the end of the first faults.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            let mut old = 0;
            let guard = page.add(0x1000).cast();
            assert_ne!(VirtualProtect(guard, 0x1000, PAGE_NOACCESS, &mut old), 0);
            page.copy_from_nonoverlapping(code.as_ptr(), code.len());
            let whole = "0f 01 fd 48 89 c1 90 90 90 90 90 90 90 90 90";
            assert_eq!(instruction_bytes(page as usize), whole);
            // The page's last three bytes: rdpru, and nothing read after it.
            let end = page.add(0x1000 - 3);
            end.copy_from_nonoverlapping(code.as_ptr(), 3);
            assert_eq!(instruction_bytes(end as usize), "0f 01 fd");
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
