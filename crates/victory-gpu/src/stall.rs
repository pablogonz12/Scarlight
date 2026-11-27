//! Naming a stall: the HLE call that did not come back, and the lock it waited on.
//!
//! A build that stops loading shows nothing in the frame line: the last real
//! work is a frame of 16.6 s whose split says all of it was "outside
//! submissions", and then no frame arrives at all, so no line is ever said
//! again. A mean over frames cannot name a call that never returns, and a
//! release build has no diagnostics counters to ask.
//!
//! So two always-on instruments, both bounded:
//!
//! - **Per-site HLE call timing** ([`Site::enter`]). Every call the title makes
//!   into [`crate::stall`]'s users is timed; a call past [`SLOW`] says its
//!   name, its arguments and its duration **once per call site** (a latch, so
//!   a stall loop cannot flood the log), and counts into the frame line's
//!   `calls over 2 s: N (site, ms)`. A call that has not returned at all is
//!   said by the watchdog thread every 250 ms it is looked at, once per site:
//!   that is the line a hung run prints, because nothing else will.
//! - **Lock waits** ([`guard`], [`read`], [`write`]). The five locks a frame
//!   takes most (the guards' shards, the watch journal, the shadow map, the
//!   queue, the descriptor heap) record their longest wait; the frame line
//!   prints all five, so the lock a stall is in is named in one run.
//!
//! Cost: two `Instant::now()` per call and per lock acquisition (tens of
//! nanoseconds), a relaxed store into a per-thread slot at entry, and nothing
//! at all on the fast path of a lock (no allocation, no lock of ours: the
//! slot table is per-slot CAS, the arguments are formatted into a fixed
//! stack buffer). `VICTORY_STALL_INSTR=0` turns both off.
//!
//! The beacon: our process's watchdog writes the call it is watching to
//! `$VICTORY_CTL_DIR/stall.txt` once a second, so a sampler outside the
//! process (`xcl-bin/notes/loading-stall-threads.sh`) can join our view of
//! the stall with `/proc`'s view of the threads.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// A call longer than this is a stall, not slow work: the frame budget is
/// 33 ms and the worst honest frame is a few hundred; two seconds is where a
/// title's own thread has stopped making progress.
pub const SLOW: Duration = Duration::from_secs(2);

/// How often the watchdog looks at the calls in flight.
const WATCH_EVERY: Duration = Duration::from_millis(250);

/// How often the beacon file is written (when a control folder is set).
const BEACON_EVERY: Duration = Duration::from_secs(1);

/// Whether the instruments are on. `VICTORY_STALL_INSTR=0` turns them off;
/// read once, so a run cannot change its mind half way.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_STALL_INSTR").is_none_or(|v| v != "0"))
}

/// Bytes of a call's arguments kept: enough for the fields that tell two
/// calls of one function apart, and small enough to format on the stack.
const ARGS: usize = 96;
/// The arguments as words, for the in-flight table (atomics only).
const WORDS: usize = ARGS.div_ceil(8);
/// Calls in flight at once: a thread has one, and a title has threads in the
/// tens. Past this, a call is timed but not published.
const MAX_CALLS: usize = 64;

/// One HLE call site, as a `static`: its name, its latch, and what it saw.
pub struct Site {
    name: &'static str,
    /// Set when the site's slow call has been said: once per site, ever.
    said: AtomicBool,
    /// Calls at this site past [`SLOW`].
    slow: AtomicU64,
    /// The site's longest call, in microseconds.
    worst_us: AtomicU64,
}

impl Site {
    /// A site named `name`. `const`, so callers keep one per call site:
    ///
    /// ```ignore
    /// static SITE: Site = Site::new("ID3D12CommandQueue::ExecuteCommandLists");
    /// let _call = SITE.enter(format_args!("lists={count}"));
    /// ```
    pub const fn new(name: &'static str) -> Site {
        Site {
            name,
            said: AtomicBool::new(false),
            slow: AtomicU64::new(0),
            worst_us: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Calls at this site that took longer than [`SLOW`].
    pub fn slow_calls(&self) -> u64 {
        self.slow.load(Ordering::Relaxed)
    }

    /// The site's longest call.
    pub fn worst(&self) -> Duration {
        Duration::from_micros(self.worst_us.load(Ordering::Relaxed))
    }

    /// Times a call until the returned [`Call`] is dropped. `args` is
    /// formatted only into a fixed stack buffer, so a fast call pays two
    /// clock reads and a copy of at most [`ARGS`] bytes.
    pub fn enter(&'static self, args: fmt::Arguments<'_>) -> Call {
        Call::new(self, args)
    }
}

/// A site per function, in one line: `stall::site!("Name").enter(...)`.
#[macro_export]
macro_rules! stall_site {
    ($name:expr) => {{
        static SITE: $crate::stall::Site = $crate::stall::Site::new($name);
        &SITE
    }};
}

/// A call in flight; what it took is recorded when it drops.
pub struct Call {
    site: &'static Site,
    at: Instant,
    /// Index in the in-flight table, or `usize::MAX` when not published.
    slot: usize,
    args: Args,
}

impl Call {
    fn new(site: &'static Site, args: fmt::Arguments<'_>) -> Call {
        let at = Instant::now();
        let args = Args::of(args);
        let mut slot = usize::MAX;
        if enabled() {
            slot = publish(site, at, &args);
            watchdog();
        }
        Call { site, at, slot, args }
    }

    /// How long this call has been running.
    pub fn elapsed(&self) -> Duration {
        self.at.elapsed()
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if self.slot != usize::MAX {
            release(self.slot);
        }
        let took = self.at.elapsed();
        let us = took.as_micros().min(u64::MAX as u128) as u64;
        self.site.worst_us.fetch_max(us, Ordering::Relaxed);
        if took < SLOW {
            return;
        }
        self.site.slow.fetch_add(1, Ordering::Relaxed);
        totals().record(self.site, took);
        let first = !self.site.said.swap(true, Ordering::Relaxed);
        say(format_args!(
            "HLE call {} took {:.1} ms ({}); {first}",
            self.site.name,
            took.as_secs_f64() * 1000.0,
            self.args,
            first = if first { "said once for this site" } else { "said already" },
        ));
    }
}

/// The arguments of one call, in a fixed buffer: no allocation on the fast
/// path, and the bytes outlive the call in the in-flight table.
struct Args {
    bytes: [u8; ARGS],
    len: usize,
}

impl Args {
    fn of(args: fmt::Arguments<'_>) -> Args {
        struct Fill<'a> {
            bytes: &'a mut [u8],
            len: usize,
            cut: bool,
        }
        impl fmt::Write for Fill<'_> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                let room = self.bytes.len() - self.len;
                let take = s.len().min(room);
                self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                if take < s.len() {
                    self.cut = true;
                }
                Ok(())
            }
        }
        let mut bytes = [0_u8; ARGS];
        let mut fill = Fill {
            bytes: &mut bytes,
            len: 0,
            cut: false,
        };
        let _ = fmt::write(&mut fill, args);
        let len = fill.len;
        if fill.cut && len >= 3 {
            bytes[len - 3..len].copy_from_slice(b"...");
        }
        Args { bytes, len }
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl fmt::Display for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.text();
        f.write_str(if text.is_empty() { "-" } else { text })
    }
}

/// One call in flight, per slot: reads from the watchdog are racy by design
/// (a call can end as it is looked at), so every field is atomic and the
/// owner CASes the slot before writing it.
#[repr(align(64))]
struct Slot {
    /// `0` free, `1` a call in flight.
    live: AtomicU32,
    /// Set when the watchdog has said this call is still running.
    said: AtomicBool,
    site: AtomicUsize,
    /// When the call started, in nanoseconds since [`EPOCH`].
    at: AtomicU64,
    tid: AtomicU32,
    len: AtomicU32,
    args: [AtomicU64; WORDS],
}

static SLOTS: [Slot; MAX_CALLS] = [const {
    Slot {
        live: AtomicU32::new(0),
        said: AtomicBool::new(false),
        site: AtomicUsize::new(0),
        at: AtomicU64::new(0),
        tid: AtomicU32::new(0),
        len: AtomicU32::new(0),
        args: [const { AtomicU64::new(0) }; WORDS],
    }
}; MAX_CALLS];

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// The slot this thread used last: a thread that keeps calling keeps its
/// slot, so claiming one is once per thread.
static MINE: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

fn thread_id() -> u32 {
    #[cfg(windows)]
    {
        // SAFETY: no preconditions, always valid for the calling thread.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn since_epoch(at: Instant) -> u64 {
    at.saturating_duration_since(*EPOCH).as_nanos().min(u64::MAX as u128) as u64
}

fn publish(site: &'static Site, at: Instant, args: &Args) -> usize {
    let tid = thread_id();
    let mine = MINE.lock().unwrap_or_else(|e| e.into_inner());
    let cached = mine.iter().position(|&(t, _)| t == tid);
    let index = match cached {
        Some(i) => mine[i].1,
        None => usize::MAX,
    };
    drop(mine);
    let index = if index < MAX_CALLS && SLOTS[index].live.load(Ordering::Relaxed) == 0 {
        index
    } else {
        let claim = SLOTS
            .iter()
            .position(|s| {
                s.live
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            });
        let Some(index) = claim else {
            return usize::MAX;
        };
        let mut mine = MINE.lock().unwrap_or_else(|e| e.into_inner());
        match mine.iter_mut().find(|(t, _)| *t == tid) {
            Some(entry) => entry.1 = index,
            None => mine.push((tid, index)),
        }
        SLOTS[index].live.store(1, Ordering::Release);
        index
    };
    let slot = &SLOTS[index];
    slot.said.store(false, Ordering::Relaxed);
    slot.site.store(site as *const Site as usize, Ordering::Relaxed);
    slot.at.store(since_epoch(at), Ordering::Relaxed);
    slot.tid.store(tid, Ordering::Relaxed);
    slot.len.store(args.len as u32, Ordering::Relaxed);
    for (i, word) in slot.args.iter().enumerate() {
        let from = i * 8;
        let mut bytes = [0_u8; 8];
        let take = args.len.saturating_sub(from).min(8);
        bytes[..take].copy_from_slice(&args.bytes[from..from + take]);
        word.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
    }
    index
}

fn release(index: usize) {
    SLOTS[index].live.store(0, Ordering::Release);
}

/// A call in flight, as the watchdog sees it.
struct Running {
    index: usize,
    site: &'static Site,
    args: String,
    tid: u32,
    for_: Duration,
}

fn running() -> Vec<Running> {
    let now = Instant::now();
    let mut out = Vec::new();
    for (index, slot) in SLOTS.iter().enumerate() {
        if slot.live.load(Ordering::Acquire) != 1 {
            continue;
        }
        let site = slot.site.load(Ordering::Relaxed);
        if site == 0 {
            continue;
        }
        // SAFETY: a site is a `&'static Site` published by the call's owner
        // and lives for the process; a slot's site is only replaced after
        // the slot is free again.
        let site = unsafe { &*(site as *const Site) };
        let at = slot.at.load(Ordering::Relaxed);
        let for_ = now.saturating_duration_since(*EPOCH + Duration::from_nanos(at));
        let len = (slot.len.load(Ordering::Relaxed) as usize).min(ARGS);
        let mut bytes = [0_u8; ARGS];
        for (i, word) in slot.args.iter().enumerate() {
            let from = i * 8;
            let take = len.saturating_sub(from).min(8);
            bytes[from..from + take].copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes()[..take]);
        }
        let args = String::from_utf8_lossy(&bytes[..len]).into_owned();
        out.push(Running {
            index,
            site,
            args,
            tid: slot.tid.load(Ordering::Relaxed),
            for_,
        });
    }
    out
}

/// The last line this module said, for tests and for the beacon.
static LAST: Mutex<String> = Mutex::new(String::new());
static SAID: AtomicU64 = AtomicU64::new(0);

/// How many lines the module has said (the per-site latch is what keeps a
/// stall loop from flooding: this counts what actually went out).
pub fn said_lines() -> u64 {
    SAID.load(Ordering::Relaxed)
}

/// The last line said.
pub fn last_line() -> String {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn say(args: fmt::Arguments<'_>) {
    let text = fmt::format(args);
    SAID.fetch_add(1, Ordering::Relaxed);
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = text.clone();
    tracing::warn!("{text}");
}

/// Starts the watchdog once. It says a call that has not returned, and
/// writes the beacon file, until the process ends.
fn watchdog() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("victory-stall".into())
            .spawn(|| {
                let mut wrote = Instant::now();
                loop {
                    std::thread::sleep(WATCH_EVERY);
                    for run in running() {
                        if run.for_ < SLOW {
                            continue;
                        }
                        let slot = &SLOTS[run.index];
                        if slot.said.swap(true, Ordering::Relaxed) {
                            continue;
                        }
                        say(format_args!(
                            "HLE call {} has not returned: running {:.1} ms in thread {}, {:?}",
                            run.site.name,
                            run.for_.as_secs_f64() * 1000.0,
                            run.tid,
                            run.args,
                        ));
                    }
                    if wrote.elapsed() < BEACON_EVERY {
                        continue;
                    }
                    wrote = Instant::now();
                    beacon();
                }
            });
        if let Err(e) = spawned {
            say(format_args!("stall watchdog could not start: {e}"));
        }
    });
}

/// The longest call in flight and the last line said, for a sampler outside
/// the process: `$VICTORY_CTL_DIR/stall.txt`.
fn beacon() {
    let Some(dir) = std::env::var_os("VICTORY_CTL_DIR") else {
        return;
    };
    let mut text = format!("t={}\n", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()));
    let mut longest: Option<Running> = None;
    for run in running() {
        if longest.as_ref().is_none_or(|l| run.for_ > l.for_) {
            longest = Some(run);
        }
    }
    match longest {
        Some(run) => text.push_str(&format!(
            "running tid={} hle={} for={:.1}ms {}\n",
            run.tid,
            run.site.name,
            run.for_.as_secs_f64() * 1000.0,
            run.args,
        )),
        None => text.push_str("running none\n"),
    }
    text.push_str(&format!("last {}\n", last_line()));
    let path = std::path::Path::new(&dir).join("stall.txt");
    let _ = std::fs::write(path.with_extension("tmp"), &text)
        .and_then(|()| std::fs::rename(path.with_extension("tmp"), &path));
}

/// The five locks a frame takes most, as the frame line names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    /// The guard pools' shards (`guards.rs`): held around every copy piece.
    Guards,
    /// The watch journal (`watch.rs`): the asks one submission answered.
    Journal,
    /// The shadow map's copies and their twins (`shadow.rs`).
    Shadow,
    /// The queue's held work and waiting list (`queue.rs`).
    Queue,
    /// The descriptor heap's views (`descriptor.rs`).
    Heap,
}

impl Lock {
    /// All of them, in the order the frame line prints them.
    pub const ALL: [Lock; 5] = [
        Lock::Guards,
        Lock::Journal,
        Lock::Shadow,
        Lock::Queue,
        Lock::Heap,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Lock::Guards => "guards",
            Lock::Journal => "journal",
            Lock::Shadow => "shadow",
            Lock::Queue => "queue",
            Lock::Heap => "heap",
        }
    }

    fn index(self) -> usize {
        Lock::ALL.iter().position(|&l| l == self).unwrap_or(0)
    }

    /// The longest wait for this lock since the last frame line.
    pub fn longest(self) -> Duration {
        Duration::from_nanos(WAITS[self.index()].swap(0, Ordering::Relaxed))
    }
}

static WAITS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

fn waited(lock: Lock, took: Duration) {
    WAITS[lock.index()].fetch_max(took.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
}

/// [`Mutex::lock`], with the wait recorded against `lock`.
pub fn guard<T>(lock: Lock, mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    let at = Instant::now();
    let held = mutex.lock().unwrap_or_else(|e| e.into_inner());
    waited(lock, at.elapsed());
    held
}

/// [`RwLock::read`], with the wait recorded against `lock`.
pub fn read<T>(lock: Lock, rw: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    let at = Instant::now();
    let held = rw.read().unwrap_or_else(|e| e.into_inner());
    waited(lock, at.elapsed());
    held
}

/// [`RwLock::write`], with the wait recorded against `lock`.
pub fn write<T>(lock: Lock, rw: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    let at = Instant::now();
    let held = rw.write().unwrap_or_else(|e| e.into_inner());
    waited(lock, at.elapsed());
    held
}

/// What the frame line adds: `calls over 2 s: N (site, ms)`, then the
/// longest wait for each of our hot locks. Takes the counters: each frame
/// line says what happened since the one before it.
pub fn frame_summary() -> String {
    let (count, site, worst) = {
        let mut totals = totals();
        let count = totals.slow;
        let worst_call = totals.worst.take();
        totals.slow = 0;
        let (site, worst) = match worst_call {
            Some((site, took)) => (site.name(), took),
            None => ("-", Duration::ZERO),
        };
        (count, site, worst)
    };
    let calls = format!(
        "calls over 2 s: {count} ({site}, {:.1} ms)",
        worst.as_secs_f64() * 1000.0
    );
    let waits: Vec<String> = Lock::ALL
        .iter()
        .map(|&lock| {
            format!(
                "{} {:.1}",
                lock.name(),
                lock.longest().as_secs_f64() * 1000.0
            )
        })
        .collect();
    format!("{calls}; longest lock waits: {} ms", waits.join(", "))
}

/// What the last frame's calls came to, for the frame line.
#[derive(Default)]
struct Totals {
    slow: u64,
    /// The frame's longest slow call and its site.
    worst: Option<(&'static Site, Duration)>,
}

impl Totals {
    fn record(&mut self, site: &'static Site, took: Duration) {
        self.slow += 1;
        if self.worst.is_none_or(|(_, worst)| took > worst) {
            self.worst = Some((site, took));
        }
    }
}

/// Behind a mutex: only a call past [`SLOW`] reaches it, so the hot path
/// never touches it.
static TOTALS: Mutex<Totals> = Mutex::new(Totals {
    slow: 0,
    worst: None,
});

fn totals() -> MutexGuard<'static, Totals> {
    TOTALS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The latch is per site and the count is per call: a stall loop says one
    /// line, and the frame line still counts every slow call.
    #[test]
    fn a_call_that_sleeps_two_point_one_seconds_is_logged_once() {
        static SITE: Site = Site::new("Test::DeliberatelySlow");
        let said = said_lines();
        let slow = SITE.slow_calls();
        for _ in 0..2 {
            let _call = SITE.enter(format_args!("id={}", 7));
            std::thread::sleep(Duration::from_millis(2100));
        }
        let lines = said_lines() - said;
        // One line for the site ("said once"), plus the watchdog's
        // "has not returned" line for each call it caught: both latched per
        // call, never per second.
        assert!(
            (1..=3).contains(&lines),
            "two 2.1 s calls said {lines} lines, not one per site plus at most one per call"
        );
        assert_eq!(SITE.slow_calls(), slow + 2, "both calls counted");
        let last = last_line();
        assert!(
            last.contains("Test::DeliberatelySlow") || last.contains("has not returned"),
            "the last line names the call: {last}"
        );
        assert!(SITE.worst() >= Duration::from_millis(2100));
    }

    /// The arguments are cut to the buffer, not panicked on.
    #[test]
    fn long_arguments_are_cut() {
        let long = "x".repeat(400);
        let args = Args::of(format_args!("{long}"));
        assert_eq!(args.len, ARGS);
        assert!(args.text().ends_with("..."));
        assert!(args.text().len() <= ARGS);
    }

    /// The frame line names the lock that was actually held.
    #[test]
    fn the_longest_lock_line_reports_the_lock_actually_held() {
        static HELD: Mutex<u8> = Mutex::new(0);
        let _ = frame_summary(); // clear what other tests left
        let holder = std::thread::spawn(|| {
            let _held = guard(Lock::Journal, &HELD);
            std::thread::sleep(Duration::from_millis(120));
        });
        // Wait for the holder to take it, then queue up behind it.
        std::thread::sleep(Duration::from_millis(40));
        {
            let _mine = guard(Lock::Journal, &HELD);
        }
        holder.join().expect("holder");
        let line = frame_summary();
        let journal = Lock::ALL
            .iter()
            .map(|&lock| (lock.name(), wait_of(&line, lock.name())))
            .collect::<Vec<_>>();
        let (name, ms) = journal
            .iter()
            .copied()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("locks");
        assert_eq!(name, "journal", "the held lock is the longest: {line}");
        assert!(ms > 20.0, "the wait is the hold's, not zero: {line}");
        for (other, other_ms) in journal {
            if other != "journal" {
                assert!(other_ms < ms, "{other} must not outlast journal: {line}");
            }
        }
        assert!(line.contains("calls over 2 s: 0"), "{line}");
    }

    /// The milliseconds after `name` on the frame line.
    fn wait_of(line: &str, name: &str) -> f64 {
        let waits = line
            .split_once("longest lock waits: ")
            .map(|(_, waits)| waits)
            .unwrap_or(line);
        for entry in waits.split(',') {
            let mut it = entry.split_whitespace();
            let (Some(lock), Some(ms)) = (it.next(), it.next()) else {
                continue;
            };
            if lock == name {
                return ms.parse().unwrap_or_else(|e| panic!("{e}: {entry:?} of {line:?}"));
            }
        }
        panic!("{name} is not on the frame line: {line}");
    }

    /// The frame line says the count and the worst site.
    #[test]
    fn the_frame_line_names_the_longest_call_and_counts_it() {
        static SITE: Site = Site::new("Test::CountedSlow");
        let _ = frame_summary();
        totals().record(&SITE, Duration::from_millis(16_550));
        let line = frame_summary();
        assert!(line.contains("calls over 2 s: 1 (Test::CountedSlow, 16550.0 ms)"), "{line}");
        // Taken: the next line has nothing.
        let next = frame_summary();
        assert!(next.contains("calls over 2 s: 0 (-, 0.0 ms)"), "{next}");
    }

    #[test]
    fn every_lock_has_a_name_and_its_own_slot() {
        for (i, lock) in Lock::ALL.iter().enumerate() {
            assert_eq!(lock.index(), i);
            assert!(!lock.name().is_empty());
        }
    }
}
