//! Under Wine, a host driver that crashes while compiling a pipeline ends
//! the process, and no handler of ours runs first: the fault happens
//! inside a Unix call (`vkCreateComputePipelines`, say), Wine turns it
//! into that call's status, and winevulkan's thunk logs
//! `Exception 0xc0000005 in Unix call.` and calls `ExitProcess(3)`. One
//! shader the driver's compiler does not survive then ends every run at
//! the same place (a RADV/ACO or dxil-spirv fault on a few of the world's
//! programs).
//!
//! winevulkan's import of `ExitProcess` is pointed at [`exit`] here. A
//! pipeline worker that arrives there while it makes a pipeline is parked
//! for good and its pipeline counts as lost (its draws and dispatches are
//! dropped like those of a refused one); another worker takes its place.
//! Any other caller exits as before.
//!
//! shortcut: the parked thread keeps what the driver and vkd3d-proton held
//! when the compiler crashed; a later compile that needs the same lock
//! would wait for it. Not seen in runs that lost up to 8 workers.
//!
//! `VICTORY_WINE_EXIT=1` leaves Wine's exit alone.
//!
//! Ported from Marvisak/VIctory #2401 (Mumogg).

use std::sync::Once;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::ExitProcess;

/// Points winevulkan's `ExitProcess` at [`exit`], once; nothing where
/// winevulkan is not loaded (Windows).
pub(super) fn hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("VICTORY_WINE_EXIT").is_some_and(|v| v != "0") {
            return;
        }
        // SAFETY: names of modules; null where one is not loaded.
        let (vulkan, kernel) = unsafe {
            (GetModuleHandleA(c"winevulkan.dll".as_ptr().cast()), GetModuleHandleA(c"kernel32.dll".as_ptr().cast()))
        };
        if vulkan.is_null() || kernel.is_null() {
            return;
        }
        // SAFETY: a loaded module and a name.
        let Some(real) = (unsafe { GetProcAddress(kernel, c"ExitProcess".as_ptr().cast()) }) else {
            return;
        };
        // SAFETY: a loaded image; its import address table is rewritten
        // while only this thread knows of the hook.
        let redirected = unsafe { redirect(vulkan, real as usize, exit as *const () as usize) };
        if redirected == 0 {
            tracing::warn!("winevulkan does not import ExitProcess: a driver compiler crash ends the run");
        } else {
            tracing::info!("Wine: a driver compiler crash costs its pipeline, not the run");
        }
    });
}

/// Replaces `from` by `to` in the import address tables of the PE32+ image
/// at `image`; returns how many slots it changed.
///
/// # Safety
/// `image` is a loaded PE32+ image whose import tables no thread is being
/// bound through.
unsafe fn redirect(image: HMODULE, from: usize, to: usize) -> usize {
    let base = image.cast::<u8>();
    // SAFETY: the caller's image: its headers and import directory are
    // mapped, and each slot is made writable for the write.
    unsafe {
        let dword = |p: *const u8| p.cast::<u32>().read_unaligned() as usize;
        let nt = base.add(dword(base.add(0x3c)));
        // The optional header follows the signature and the file header;
        // data directory 1 (imports) is 0x78 into a PE32+ one.
        let mut descriptor = base.add(dword(nt.add(0x18 + 0x78)));
        let mut changed = 0;
        // Descriptors of 20 bytes, the last all zeros: the DLL's name at
        // 12, its address table at 16.
        while dword(descriptor.add(12)) != 0 {
            let mut slot = base.add(dword(descriptor.add(16))).cast::<usize>();
            while slot.read_unaligned() != 0 {
                if slot.read_unaligned() == from {
                    let mut old = 0;
                    if VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old) != 0 {
                        slot.write_unaligned(to);
                        VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut old);
                        changed += 1;
                    }
                }
                slot = slot.add(1);
            }
            descriptor = descriptor.add(20);
        }
        changed
    }
}

/// What winevulkan calls in place of `ExitProcess`.
unsafe extern "system" fn exit(code: u32) -> ! {
    // Parking keeps what the driver and vkd3d-proton held, but the parked
    // thread never unwinds: with `VICTORY_LIBRARY_LOCK=1` and the library's
    // one-call lock held, that would keep the lock for the rest of the run,
    // so it exits; likewise while it holds the library's store guard, which
    // every later save waits on. A crash outside both (a creation) parks as
    // before; the name it held is let go by `library::abandon`.
    // The guards first: `worker_lost` drains the queue and saves, and a save
    // asks for the library's write lock, which a thread holding the store
    // read guard would wait on for good (a lost worker does not unwind it).
    if super::library::calling() || super::library::storing_now() || !super::make::worker_lost() {
        // SAFETY: what the caller asked for.
        unsafe { ExitProcess(code) }
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PE32+ image of our own with one import descriptor of three slots.
    #[test]
    fn an_import_slot_is_redirected() {
        let mut image = vec![0u64; 64];
        let base = image.as_mut_ptr().cast::<u8>();
        // SAFETY: every offset is inside the 512 bytes of `image`, which
        // nothing else reads or writes.
        unsafe {
            let put = |at: usize, v: u32| base.add(at).cast::<u32>().write_unaligned(v);
            put(0x3c, 0x40); // the NT headers
            put(0x40 + 0x18 + 0x78, 0x100); // the import directory
            put(0x100 + 12, 0x180); // a descriptor: its name,
            put(0x100 + 16, 0x140); // its address table; the next is zeros
            let slots = base.add(0x140).cast::<usize>();
            for (i, v) in [0x1111usize, 0x2222, 0x1111, 0].into_iter().enumerate() {
                slots.add(i).write_unaligned(v);
            }
            assert_eq!(redirect(base.cast(), 0x1111, 0x3333), 2);
            let now: Vec<usize> = (0..4).map(|i| slots.add(i).read_unaligned()).collect();
            assert_eq!(now, [0x3333, 0x2222, 0x3333, 0]);
            assert_eq!(redirect(base.cast(), 0x1111, 0x3333), 0);
        }
    }
}
