//! `watch`'s journal of written pages: page number → the asking that last
//! found it written, the answer a copy gets without asking the kernel.
//!
//! One lock for all pages made every title write the runtime declares
//! (`watch`'s [`wrote`](crate::watch), thousands a frame) and every copy's
//! question queue on each other, though they touch different pages. The pages
//! are sharded here, as `guards` shards the marked blocks (same mechanism,
//! #258): each shard owns a contiguous 1 MiB run of pages and its own
//! cache-line-aligned `RwLock`, so a record in one does not stop a question
//! about another. A range takes one shard at a time, in page order, so two
//! ranges can never wait on each other. How many shards we use is a switch,
//! `VICTORY_JOURNAL_SHARDS` (`1`: the single lock of before).
//!
//! The asking numbers stay one sequence ([`crate::watch`]'s `ASKED`): a page
//! is compared with the asking a copy took, so a number from one shard has to
//! mean the same as one from another. Two pages of one range may be inserted
//! a moment apart (a copy sees one and not yet the other); each page's own
//! number is what the callers use, never a whole range's.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The unit the journal tracks: `watch`'s page.
const PAGE: usize = crate::watch::PAGE;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

/// Pages of one shard in a row: 1 MiB, so a range meets few shards.
const STRIPE: usize = 16;

/// Shards the table holds: [`shards`] chooses how many are used.
const MAX_SHARDS: usize = 128;

/// Shards in use: `VICTORY_JOURNAL_SHARDS`, rounded up to a power of two, 16
/// unless given (`1`: one lock for every page, as before).
pub(crate) fn shards() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        let n = std::env::var("VICTORY_JOURNAL_SHARDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(16);
        n.clamp(1, MAX_SHARDS).next_power_of_two()
    })
}

#[inline]
fn shard(page: usize, n: usize) -> usize {
    (page / STRIPE) & (n - 1)
}

/// `[first, last)` as its maximal runs of pages of one shard: `(shard, from,
/// to)` in page order.
struct Runs {
    cur: usize,
    last: usize,
    n: usize,
}

impl Runs {
    fn new(first: usize, last: usize) -> Self {
        Self {
            cur: first,
            last,
            n: shards(),
        }
    }
}

impl Iterator for Runs {
    type Item = (usize, usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.last {
            return None;
        }
        let to = ((self.cur / STRIPE + 1) * STRIPE).min(self.last);
        let run = (shard(self.cur, self.n), self.cur, to);
        self.cur = to;
        Some(run)
    }
}

/// One shard's pages.
struct Pages {
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
}

/// One shard, on its own cache line: no false sharing with its neighbours.
#[repr(align(64))]
struct Slot(RwLock<Pages>);

static SLOTS: [Slot; MAX_SHARDS] = [const {
    Slot(RwLock::new(Pages {
        written: BTreeMap::new(),
    }))
}; MAX_SHARDS];

fn writing(s: usize) -> RwLockWriteGuard<'static, Pages> {
    SLOTS[s].0.write().unwrap_or_else(|e| e.into_inner())
}

fn reading(s: usize) -> RwLockReadGuard<'static, Pages> {
    SLOTS[s].0.read().unwrap_or_else(|e| e.into_inner())
}

/// Notes `pages` (in whatever order) as written by asking `now`, one shard
/// held at a time, so a record never waits on another record's shard.
fn insert(pages: impl Iterator<Item = usize>, now: u64) {
    let n = shards();
    let mut held: Option<(usize, RwLockWriteGuard<'static, Pages>)> = None;
    for page in pages {
        let s = shard(page, n);
        if held.as_ref().map(|h| h.0) != Some(s) {
            held = Some((s, writing(s)));
        }
        if let Some((_, p)) = held.as_mut() {
            p.written.insert(page, now);
        }
    }
}

/// Notes pages `[first, last)` as written by asking `now`.
pub(crate) fn record(first: usize, last: usize, now: u64) {
    insert(first..last, now);
}

/// The journal's pages of `[first, last)` as last written: `(page, asking)`
/// in page order. Empty for pages that were never found written.
pub(crate) fn pages(first: usize, last: usize) -> Vec<(usize, u64)> {
    let mut v = Vec::new();
    for (s, from, to) in Runs::new(first, last) {
        let p = reading(s);
        v.extend(p.written.range(from..to).map(|(&page, &w)| (page, w)));
    }
    v
}

/// Whether a page of `[first, last)` was written after asking `since`.
pub(crate) fn any_since(first: usize, last: usize, since: u64) -> bool {
    for (s, from, to) in Runs::new(first, last) {
        let p = reading(s);
        if p.written.range(from..to).any(|(_, &w)| w > since) {
            return true;
        }
    }
    false
}

/// The kernel's answer to [`ask`]: its room is held until this is dropped,
/// so an asking never allocates and the one buffer serves every thread.
pub(crate) struct Answer {
    room: std::sync::MutexGuard<'static, Vec<usize>>,
    count: usize,
}

impl Answer {
    /// The pages the kernel found written, as the addresses it gives.
    pub(crate) fn pages(&self) -> &[usize] {
        &self.room[..self.count]
    }

    /// Notes the answer's pages as written by asking `now`.
    pub(crate) fn record(&self, now: u64) {
        insert(self.pages().iter().map(|&at| at / PAGE), now);
    }
}

/// Asks the kernel for the pages of `[first, last)` written since it last
/// reset them: `Some` answer, or `None` if the memory is not write-watched
/// (the caller then looks at the guards).
///
/// This is the one place a kernel call is made, and it costs 30-70
/// microseconds whatever the range: askings were serialised by the journal's
/// lock before and are by the answer's room now, never by a page shard (a
/// copy reads pages meanwhile).
pub(crate) fn ask(first: usize, last: usize) -> Option<Answer> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    static ROOM: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    let mut guard = ROOM.lock().unwrap_or_else(|e| e.into_inner());
    let room = &mut *guard;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    room.resize(count.max(room.len()), 0);
    let mut granularity = 0;
    #[cfg(test)]
    note_kernel_ask();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            room.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return None;
    }
    Some(Answer { room: guard, count })
}

#[cfg(test)]
thread_local! {
    /// The kernel askings of this thread (each test runs on its own).
    static KERNEL_ASKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_kernel_ask() {
    KERNEL_ASKED.with(|n| n.set(n.get() + 1));
}

/// The kernel askings of this thread so far: a test asserts one was (not)
/// made.
#[cfg(test)]
pub(crate) fn kernel_asks() -> usize {
    KERNEL_ASKED.with(|n| n.get())
}

/// The pages recorded, all shards: what a test proves the bench fired with.
#[cfg(test)]
pub(crate) fn recorded() -> usize {
    (0..shards()).map(|s| reading(s).written.len()).sum()
}

/// Forgets every page in every shard.
#[cfg(test)]
pub(crate) fn clear() {
    for s in 0..shards() {
        writing(s).written.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Two page numbers of different shards from `base`, if there is more
    /// than one shard.
    fn two_shards(base: usize) -> Option<(usize, usize)> {
        (shards() > 1).then_some((base, base + STRIPE))
    }

    /// A page recorded in one shard is found from the range's question, and
    /// a range across shards gives every page in order.
    #[test]
    fn pages_of_every_shard_are_seen() {
        let Some((a, b)) = two_shards(4_000_000) else {
            panic!("sharding off (VICTORY_JOURNAL_SHARDS=1): every page waits for every other");
        };
        record(a, a + 1, 7);
        record(b, b + 1, 9);
        assert_eq!(pages(a, b + 1), vec![(a, 7), (b, 9)]);
        assert!(any_since(a, b + 1, 8));
        assert!(!any_since(a, a + 1, 8), "the first page was written before");
        clear();
    }

    /// Two threads journaling pages of different regions do not block each
    /// other; with one lock for all pages the second waits for the first,
    /// which is the stall sharding removes. Bounded, so a deadlock fails the
    /// test instead of hanging the suite.
    #[test]
    fn a_held_shard_does_not_stop_another() {
        let Some((a, b)) = two_shards(6_000_000) else {
            panic!("sharding off (VICTORY_JOURNAL_SHARDS=1): every page waits for every other");
        };
        let (holding, held) = mpsc::channel();
        let (released, go) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = writing(shard(a, shards()));
            holding.send(()).unwrap();
            go.recv().unwrap();
        });
        held.recv_timeout(Duration::from_secs(5))
            .expect("the holder takes its shard");

        let (finished, joined) = mpsc::channel();
        let other = std::thread::spawn(move || {
            record(b, b + STRIPE, 1);
            finished.send(()).unwrap();
        });
        joined
            .recv_timeout(Duration::from_secs(5))
            .expect("a record in another shard does not wait for a held shard");
        other.join().unwrap();

        // The held shard itself does wait: the lock is not a no-op.
        let (blocked, waited) = mpsc::channel();
        let same = std::thread::spawn(move || {
            record(a, a + 1, 1);
            blocked.send(()).unwrap();
        });
        assert!(
            waited.recv_timeout(Duration::from_millis(300)).is_err(),
            "a record took a shard held for writing"
        );
        released.send(()).unwrap();
        holder.join().unwrap();
        waited
            .recv_timeout(Duration::from_secs(5))
            .expect("released, the record of the held shard finishes");
        same.join().unwrap();
        clear();
    }

    /// Threads journaling pages of regions of their own all finish, and every
    /// page is found afterwards: no record waits on another's shard, and none
    /// waits forever. Bounded, so a deadlock fails instead of hanging.
    #[test]
    fn records_of_different_regions_all_finish() {
        let base = 8_000_000;
        let (tx, rx) = mpsc::channel();
        for t in 0..8 {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let first = base + t * 8 * STRIPE;
                for i in 0..500 {
                    record(first + i % (4 * STRIPE), first + i % (4 * STRIPE) + 1, 1);
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);
        for _ in 0..8 {
            rx.recv_timeout(Duration::from_secs(60))
                .expect("every thread of records finishes");
        }
        for t in 0..8 {
            let first = base + t * 8 * STRIPE;
            assert_eq!(pages(first, first + 4 * STRIPE).len(), 4 * STRIPE, "thread {t}'s pages");
        }
        clear();
    }

    /// What a record costs: [`record`] of one page, alone and on eight
    /// threads of pages of their own, and the same for the question a copy
    /// asks ([`any_since`] of one page). `VICTORY_JOURNAL_SHARDS=1` measures
    /// the single lock.
    #[test]
    fn record_cost_alone_and_on_eight_threads() {
        const OPS: usize = 100_000;
        const READS: usize = 4_000_000;
        const WRITES: usize = 6_000_000;
        let n = shards();
        let bump = |t: usize, i: usize| {
            let page = WRITES + t * (1 << 20) + i;
            record(page, page + 1, 1);
        };
        let question = |t: usize, i: usize| {
            let page = READS + t * (1 << 20) + i;
            let _ = any_since(page, page + 1, 0);
        };
        // Alone at a page base of its own, so the two runs' pages do not
        // overlap and each can be counted.
        const ALONE: usize = 64;
        let alone = |f: &dyn Fn(usize, usize)| {
            let started = Instant::now();
            for i in 0..OPS {
                f(ALONE, i);
            }
            started.elapsed().as_nanos() as f64 / OPS as f64
        };
        let eight = |f: &(dyn Fn(usize, usize) + Sync)| {
            let started = Instant::now();
            std::thread::scope(|s| {
                for t in 0..8 {
                    s.spawn(move || {
                        for i in 0..OPS / 8 {
                            f(t, i);
                        }
                    });
                }
            });
            started.elapsed().as_nanos() as f64 / OPS as f64
        };
        let (w1, w8, r1, r8) = (alone(&bump), eight(&bump), alone(&question), eight(&question));
        // Whether the bench fired at all: the pages it recorded are there,
        // one per op of each run.
        let seen = recorded();
        assert_eq!(pages(WRITES, WRITES + 8 * (1 << 20)).len(), OPS, "nothing was recorded");
        assert_eq!(
            pages(WRITES + ALONE * (1 << 20), WRITES + (ALONE + 1) * (1 << 20)).len(),
            OPS,
            "nothing was recorded alone"
        );
        eprintln!(
            "journal: {n} shards, {OPS} ops — record (one page): {w1:.1} ns alone, {w8:.1} ns on 8 \
             threads; question (any_since, one page): {r1:.1} ns alone, {r8:.1} ns on 8; {seen} \
             pages recorded"
        );
        assert!(
            r8 < 20_000.0 && w8 < 20_000.0,
            "acquisitions cost microseconds: {r8:.0}/{w8:.0} ns"
        );
        // A frame's records: a submission of 10,000 buffer copies, the same
        // pages every frame, ten frames deep.
        const FRAME: usize = 10_000;
        const FRAMES: usize = 10;
        clear();
        let started = Instant::now();
        for _ in 0..FRAMES {
            for i in 0..FRAME {
                record(FRAME + i, FRAME + i + 1, 1);
            }
        }
        let per = started.elapsed().as_micros() as f64 / FRAMES as f64;
        let fired = pages(FRAME, 2 * FRAME).len();
        assert_eq!(fired, FRAME, "the frame's records were not journalled");
        eprintln!(
            "journal: a frame of {FRAME} records — {per:.1} us a frame, {fired} pages in the \
             journal of it"
        );
        clear();
    }
}
