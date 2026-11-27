//! Fences: a 64-bit value and the events waiting for it. Nothing runs on a
//! GPU yet, so only signals (the title's, later our queues') move one.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{self, E_NOTIMPL, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 4] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Fence {
    object: Object,
    /// Titles also read it straight from memory, at the fence's GPU
    /// address.
    value: AtomicU64,
    flags: u32,
    /// Events to set once the value reaches theirs (handles as integers).
    waiters: Mutex<Vec<(u64, usize)>>,
}

fn fence<'a>(this: This) -> &'a Fence {
    // SAFETY: called from fence methods only.
    unsafe { &*this.cast::<Fence>() }
}

pub(crate) extern "system" fn create(
    _: This,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::debug!(initial, flags, "fence");
    let fence = Box::new(Fence {
        object: Object::new(&FUNCTIONS),
        value: AtomicU64::new(initial),
        flags,
        waiters: Mutex::new(Vec::new()),
    });
    object::hand_out(FENCE, fence, iid, &FENCE_IIDS, out)
}

fn set_event(event: usize) {
    #[cfg(windows)]
    // SAFETY: the title passed an event handle.
    unsafe {
        windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void)
    };
    #[cfg(not(windows))]
    let _ = event;
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        FENCE,
        iid,
        object::is(iid, &FENCE_IIDS).then_some(this),
        out,
    )
}
pub(crate) extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Fence>(this)
}
/// The value as it is, read by our own paths: no taking in of the line
/// (`behind`), which may be waiting for what waits for the caller.
pub(crate) fn value(this: This) -> u64 {
    fence(this).value.load(Ordering::Acquire)
}
pub(crate) extern "system" fn completed(this: This) -> u64 {
    // With submissions in the line of a thread of ours (`VICTORY_ASYNC_SUBMIT`),
    // a value not set yet is behind work the reading thread may be waiting
    // for: the read takes the line in, as a wait does.
    #[cfg(windows)]
    crate::queue::before_sync();
    fence(this).value.load(Ordering::Acquire)
}
extern "system" fn set_event_on_completion(this: This, value: u64, event: *mut c_void) -> HRESULT {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    if fence.value.load(Ordering::Acquire) >= value {
        if !event.is_null() {
            set_event(event as usize);
        }
        return S_OK;
    }
    if event.is_null() {
        // With submissions sent to the card and not waited for
        // (`crate::host::late`) the signal may be behind them: they are
        // settled, which sets it.
        #[cfg(windows)]
        {
            drop(waiters);
            if crate::host::late() {
                crate::host::settle_for_fence();
                if fence.value.load(Ordering::Acquire) >= value {
                    return S_OK;
                }
            }
            // A null event means "block until reached", and the card does
            // reach it: wait on the value itself in short slices. Bounded,
            // because a submission can be refused and then nothing would ever
            // wake this guest thread.
            if block_ms() > 0 {
                // Let what a queue holds run once, before sleeping: it may reach
                // the value, and a release can execute a submission on this
                // thread (`INFINITE` host-event wait), which inside the loop
                // would overshoot the deadline.
                crate::queue::release_held(false);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(block_ms());
                loop {
                    let current = fence.value.load(Ordering::Acquire);
                    if current >= value {
                        return S_OK;
                    }
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let ms = left.as_millis().min(10).max(1) as u32;
                    // SAFETY: a pointer to this fence's value, which lives as
                    // long as the fence, and to a copy of what it holds now:
                    // the thread sleeps while they still agree.
                    unsafe {
                        windows_sys::Win32::System::Threading::WaitOnAddress(
                            (&raw const fence.value).cast(),
                            (&raw const current).cast(),
                            size_of::<u64>(),
                            ms,
                        );
                    }
                }
                GAVE_UP.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(value, "a blocking fence wait gave up");
                return E_NOTIMPL;
            }
        }
        // shortcut: no blocking wait asked for (`VICTORY_FENCE_BLOCK_MS=0`):
        // fail instead of hanging.
        tracing::warn!(value, "blocking fence wait not implemented");
        return E_NOTIMPL;
    }
    waiters.push((value, event as usize));
    S_OK
}
/// Blocking waits on a fence's value that ran out of time (`tracing::warn`
/// at the time). One the signal wakes is not counted.
pub(crate) static GAVE_UP: AtomicU64 = AtomicU64::new(0);

/// How long a null-event wait blocks before it fails instead of hanging
/// (`VICTORY_FENCE_BLOCK_MS`, 0: fail at once, as before).
fn block_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("VICTORY_FENCE_BLOCK_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(250)
    })
}

/// Moves the fence to `value` and sets the events that waited for it.
pub(crate) fn set(this: This, value: u64) {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    fence.value.store(value, Ordering::Release);
    waiters.retain(|&(wanted, event)| {
        let reached = wanted <= value;
        if reached {
            set_event(event);
        }
        !reached
    });
    drop(waiters);
    // A guest thread waiting on the value itself (no event) sleeps on this
    // address: it is woken here, and reads the new value.
    #[cfg(windows)]
    // SAFETY: the fence's value, which lives as long as the fence.
    unsafe {
        windows_sys::Win32::System::Threading::WakeByAddressAll((&raw const fence.value).cast());
    }
}
pub(crate) extern "system" fn signal(this: This, value: u64) -> HRESULT {
    set(this, value);
    // A queue may have waited for it: what it held runs now.
    #[cfg(windows)]
    crate::queue::release_held(false);
    S_OK
}

extern "system" fn creation_flags(this: This) -> u32 {
    fence(this).flags
}
/// Where the value lives (GPU addresses are CPU addresses).
extern "system" fn gpu_address(this: This) -> u64 {
    (&raw const fence(this).value) as u64
}

/// IUnknown 3, object 4, device child 1, fence 3, fence 1 (2, a console
/// one among them).
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<FENCE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = completed as *const (); // 0x40
    f[9] = set_event_on_completion as *const (); // 0x48
    f[10] = signal as *const (); // 0x50
    f[11] = creation_flags as *const (); // 0x58
    f[12] = gpu_address as *const (); // 0x60
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f), S_OK);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        // SAFETY: plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let wait = slot::<extern "system" fn(This, u64, *mut c_void) -> HRESULT>(f, 0x48);
        let signal = slot::<extern "system" fn(This, u64) -> HRESULT>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        // SAFETY: a valid event.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(signal(f, 5), S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 9, std::ptr::null_mut()), E_NOTIMPL);
        // The title reads the value at the fence's address.
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f);
        // SAFETY: the fence's value.
        assert_eq!(unsafe { *(at as *const u64) }, 5);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_blocking_null_event_wait_is_woken_by_the_signal() {
        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f), S_OK);
        let signal = slot::<extern "system" fn(This, u64) -> HRESULT>(f, 0x50);
        let (tx, rx) = std::sync::mpsc::channel();
        let at = f as usize;
        let thread = std::thread::spawn(move || {
            // SAFETY: a live fence; the test holds it until the join.
            let wait =
                slot::<extern "system" fn(This, u64, *mut c_void) -> HRESULT>(at as This, 0x48);
            let _ = tx.send(wait(at as This, 5, std::ptr::null_mut()));
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(signal(f, 5), S_OK);
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(S_OK),
            "the signal woke the blocking wait, it did not run out of time"
        );
        thread.join().unwrap();
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }
}
