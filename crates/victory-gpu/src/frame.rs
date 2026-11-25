//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! Events are scheduled at an offset from the tick, which waits honour;
//! the clock is still free-running rather than tied to presentation.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    tracing::info!(?interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Per event kind: the offset from the tick, in nanoseconds, last scheduled.
static OFFSETS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Device method: events are scheduled at every tick, `offset` microseconds
/// into it. The list is the event's wait list, which our clock does not use.
pub(crate) extern "system" fn schedule(
    _: This,
    kind: u32,
    offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    OFFSETS[(kind as usize).min(OFFSETS.len() - 1)]
        .store(u64::from(offset) * 1_000, Ordering::Relaxed);
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next event after `elapsed` nanoseconds into the clock, and how long
/// until it. An event happens once per tick, after `offset` nanoseconds:
/// a wait woken a little early must not let the next one end on the same
/// tick. Offsets of a whole tick or more move the event into a later tick.
fn event_after(
    elapsed: u128,
    interval: u128,
    first: u64,
    last: u64,
    offset: u128,
) -> (u64, u128) {
    let interval = interval.max(1);
    let extra = (offset / interval) as u64;
    let remainder = offset % interval;
    // The current tick's event may still lie ahead of `elapsed` when the
    // offset is inside it. Only step to the next tick once that instant has
    // passed (review note on #321).
    let mut tick = first + (elapsed / interval) as u64 + extra;
    if u128::from(tick - first) * interval + remainder <= elapsed {
        tick += 1;
    }
    tick = tick.max(last + 1);
    let at = u128::from(tick - first) * interval + remainder;
    (tick, at.saturating_sub(elapsed))
}

/// The next tick after now and after the last one a wait for `kind` ended
/// on, and how long until its scheduled offset within that tick.
fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let last_tick = last(kind).load(Ordering::Relaxed);
    // The tick most recently passed. A flip (or vblank) wait that begins
    // after a tick nobody has taken yet is late: the frame took longer than
    // an interval. It ends at once on that tick instead of waiting for the
    // following one, so a frame a little over budget costs that much more,
    // not a whole extra interval (30 fps falling straight to 15).
    let passed = c.first + (elapsed / interval) as u64;
    if kind != ORIGIN && passed > last_tick && passed > c.first {
        return (passed, Duration::ZERO);
    }
    // #321's offset within the tick, with the late-tick rule above.
    let (tick, delay) = event_after(
        elapsed,
        interval,
        c.first,
        last_tick,
        u128::from(OFFSETS[(kind as usize).min(OFFSETS.len() - 1)].load(Ordering::Relaxed)),
    );
    (tick, Duration::from_nanos(delay.min(u64::MAX as u128) as u64))
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let (mut tick, mut wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // A blocking wait for an event that has happened since the last wait
    // for it ended (a frame took longer than a tick) ends at once, on the
    // tick that passed: sleeping on to the next one cost 17 ms of every
    // 730 ms frame (run 58: "2.0 frame waits sleeping 16.8 ms"). Polls (a
    // time-out shorter than a tick) answer as before.
    // Not the origin wait: it hands out the frame's token, and ending two
    // of them on one passed tick starts two frames together, whose work
    // (the intro's movie uploads among it) then interleaves -- jman's
    // `fe1e6caf` ends a late wait on the tick it missed for the flip and
    // the vblank only, and the frame waits sleep to the next tick as
    // before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !crate::switches::b(
        "VICTORY_LATE_WAITS",
        *SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1")),
    ) && !origin
        && limit >= wait
        && tick > last(kind).load(Ordering::Relaxed) + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    #[cfg(windows)]
    {
        let us = wait.min(limit).as_micros() as u64;
        crate::framelog::add(crate::framelog::T::PaceUs, us);
        crate::framelog::here(crate::framelog::H::PaceUs, us);
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_happen_at_their_offset_within_the_tick() {
        const MS: u128 = 1_000_000;
        // A quarter into the first millisecond after now.
        assert_eq!(event_after(250_000, MS, 0, 0, 0), (1, 750_000));
        assert_eq!(event_after(250_000, MS, 0, 0, 300_000), (1, 1_050_000));
        // An offset past a tick moves the event into the next one; that
        // event is still ahead and must not be skipped.
        assert_eq!(event_after(250_000, MS, 0, 0, MS + 200_000), (1, 950_000));
        // Review note on #321: between a tick and its offset, that tick's
        // event is still pending.
        assert_eq!(event_after(1_100_000, MS, 0, 0, 300_000), (1, 200_000));
        // A kind already woken on a tick cannot wake on it again, offset or
        // no offset.
        assert_eq!(event_after(250_000, MS, 0, 1, 0), (2, 1_750_000));
        assert_eq!(event_after(250_000, MS, 0, 1, 300_000), (2, 2_050_000));
        // The clock's first tick is carried through interval changes.
        assert_eq!(event_after(250_000, MS, 100, 100, 0), (101, 750_000));
        // An offset behind now never makes the wait negative.
        assert_eq!(event_after(900_000, MS, 0, 0, 0).1, 100_000);
    }

    #[test]
    fn scheduling_stores_the_offset_for_a_kind() {
        let null = std::ptr::null();
        let list = std::ptr::null::<u8>();
        assert_eq!(schedule(null, 2, 700, list, 0), S_OK);
        assert_eq!(OFFSETS[2].load(Ordering::Relaxed), 700_000);
        // An unknown kind shares the last slot but still accepts the call.
        assert_eq!(schedule(null, 99, 12, list, 0), S_OK);
        assert_eq!(OFFSETS[2].load(Ordering::Relaxed), 12_000);
        schedule(null, 2, 0, list, 0);
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        assert!(before.elapsed() <= Duration::from_millis(50));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let mut token = 0;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                LAST[2].load(Ordering::Relaxed)
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP);
        assert!(tick > flips[3]);

        // A frame over budget: its flip wait begins after a tick nobody
        // took and ends at once on it, not on the tick after.
        assert_eq!(set_interval(null, null, 20_000, 1, 0), S_OK);
        assert_eq!(
            wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
            S_OK
        );
        let flipped = LAST[2].load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(25));
        let late = Instant::now();
        assert_eq!(
            wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
            S_OK
        );
        assert!(late.elapsed() < Duration::from_millis(10));
        assert_eq!(LAST[2].load(Ordering::Relaxed), flipped + 1);
    }
}
