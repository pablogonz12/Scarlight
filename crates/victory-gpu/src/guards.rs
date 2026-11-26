//! `watch`'s guards for view memory: the blocks we made read-only, the ones
//! still clean, the ones known uncommitted and the ones known there.
//!
//! One lock for all of them made every copy of view memory and every write
//! fault of the title's threads (the vectored handler) queue on each other,
//! though they touch different blocks: the blocks are sharded here, so a copy
//! holding one shard does not stop a fault in another. Mechanism from
//! fever308's victory-corgi (`f4c1175a1`): the blocks of a shard are
//! contiguous and shards are taken in ascending order, so two ranges can
//! never wait on each other. How many we use is a switch,
//! `VICTORY_GUARD_SHARDS` (`1`: the single lock of before).
//!
//! Lock order: `watch`'s `JOURNAL`, then a shard, then `watch`'s
//! `DESTINATIONS` and `BACK` (taken by [`protect`] and [`open`] under it).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The unit guarded and copied: 64 KiB, `watch`'s page.
const BLOCK: usize = crate::watch::PAGE;
// A written block is reported as its page.
const _: () = assert!(BLOCK == crate::watch::PAGE);

/// Pages of one block (a block is reported as its pages).
const PAGES: usize = BLOCK / crate::watch::PAGE;

/// Blocks of one shard in a row: 1 MiB, so a range meets few shards.
const STRIPE: usize = 16;

/// Shards the table holds: [`shards`] chooses how many are used, so the lock
/// the fault handler takes is a static one (nothing is allocated in a fault).
const MAX_SHARDS: usize = 128;

/// Shards in use: `VICTORY_GUARD_SHARDS`, rounded up to a power of two, 16
/// unless given (`1`: one lock for every block, as before).
fn shards() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        let n = std::env::var("VICTORY_GUARD_SHARDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(16);
        n.clamp(1, MAX_SHARDS).next_power_of_two()
    })
}

#[inline]
fn shard(block: usize, n: usize) -> usize {
    (block / STRIPE) & (n - 1)
}

/// `[first, last)` as its maximal runs of blocks of one shard: `(shard, from,
/// to)` in block order.
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

/// The shards of a range held for writing, in ascending order: a change to a
/// range's marks is one step, so no query sees half of it.
struct Writing(Vec<(usize, RwLockWriteGuard<'static, Guards>)>);

impl Writing {
    fn of_range(first: usize, last: usize) -> Self {
        let mut mask = Mask(0);
        for (s, _, _) in Runs::new(first, last) {
            mask.0 |= 1u128 << s;
        }
        let mut v = Vec::with_capacity(mask.0.count_ones() as usize);
        mask.for_each(|s| v.push((s, slot(s))));
        Self(v)
    }

    fn at(&mut self, s: usize) -> &mut Guards {
        let i = self.0.binary_search_by_key(&s, |&(s, _)| s).expect("held");
        &mut self.0[i].1
    }
}

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: CleanBlocks,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: BTreeSet<usize>,
    /// Blocks of title memory a copy has taken whole (or a guard has made
    /// read-only, which only committed memory allows): memory that is there,
    /// so a copy may go straight at its bytes ([`all_mapped`]).
    mapped: BTreeSet<usize>,
}

/// One shard, on its own cache line: no false sharing with its neighbours.
#[repr(align(64))]
struct Slot(RwLock<Guards>);

static SLOTS: [Slot; MAX_SHARDS] = [const {
    Slot(RwLock::new(Guards {
        protected: BTreeSet::new(),
        clean: CleanBlocks(BTreeMap::new()),
        absent: BTreeSet::new(),
        mapped: BTreeSet::new(),
    }))
}; MAX_SHARDS];

fn slot(s: usize) -> RwLockWriteGuard<'static, Guards> {
    SLOTS[s].0.write().unwrap_or_else(|e| e.into_inner())
}

fn reading(s: usize) -> RwLockReadGuard<'static, Guards> {
    SLOTS[s].0.read().unwrap_or_else(|e| e.into_inner())
}

/// Shards `0..` as a bitset, set bits taken in ascending order (fever308's
/// `ShardMask`): two ranges then always take their shards the same way round.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Mask(u128);

impl Mask {
    fn for_each(self, mut f: impl FnMut(usize)) {
        let mut rem = self.0;
        while rem != 0 {
            f(rem.trailing_zeros() as usize);
            rem &= rem - 1;
        }
    }
}

/// Disjoint, non-touching half-open runs of clean blocks.
struct CleanBlocks(BTreeMap<usize, usize>);

impl CleanBlocks {
    fn insert(&mut self, mut from: usize, mut to: usize) {
        if from >= to {
            return;
        }
        if let Some((&start, &end)) = self.0.range(..=from).next_back()
            && end >= from
        {
            if end >= to {
                return;
            }
            from = start;
        }
        while let Some((&start, &end)) = self.0.range(from..=to).next() {
            self.0.remove(&start);
            to = to.max(end);
        }
        self.0.insert(from, to);
    }

    fn remove(&mut self, block: usize) {
        let Some((&from, &to)) = self.0.range(..=block).next_back() else {
            return;
        };
        if block >= to {
            return;
        }
        if from < block {
            self.0.insert(from, block);
        } else {
            self.0.remove(&from);
        }
        if block + 1 < to {
            self.0.insert(block + 1, to);
        }
    }

    fn uncovered(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut next = self
            .0
            .range(..=from)
            .next_back()
            .map_or(from, |(_, &end)| from.max(end));
        let mut missing = Vec::new();
        if next >= to {
            return missing;
        }
        for (&start, &end) in self.0.range(from + 1..to) {
            if next < start {
                missing.push((next, start));
            }
            next = end;
        }
        if next < to {
            missing.push((next, to));
        }
        missing
    }
}

/// Whether a view mapped beside others (`opened_beside`) has its guards
/// taken off: `VICTORY_GUARD_ALIASES=0` leaves them (a start-up switch).
pub(crate) fn aliases() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_ALIASES").is_none_or(|v| v != "0"))
}

/// Whether the guards hold the block of `at` (read-only by our hand, or known
/// not committed): the range needs no view check.
pub(crate) fn block_known(at: usize) -> bool {
    let block = at / BLOCK;
    let g = reading(shard(block, shards()));
    g.protected.contains(&block) || g.absent.contains(&block)
}

/// What the fault handler says of an address it cannot resolve: whether the
/// guards hold the block, and whether they know it uncommitted.
pub(crate) fn holds(at: usize) -> (bool, bool) {
    let block = at / BLOCK;
    let g = reading(shard(block, shards()));
    (g.protected.contains(&block), g.absent.contains(&block))
}

/// The shards of the range read-locked together, in ascending order: a range
/// that spans several sees one state, never half of it.
fn shared(first: usize, last: usize) -> Vec<(usize, RwLockReadGuard<'static, Guards>)> {
    let mut mask = Mask(0);
    for (s, _, _) in Runs::new(first, last) {
        mask.0 |= 1u128 << s;
    }
    let mut held = Vec::with_capacity(mask.0.count_ones() as usize);
    mask.for_each(|s| held.push((s, reading(s))));
    held
}

/// One stripe's blocks under its one read lock, without building anything.
fn one_stripe(first: usize, last: usize) -> Option<RwLockReadGuard<'static, Guards>> {
    (first / STRIPE == (last - 1) / STRIPE).then(|| reading(shard(first, shards())))
}

/// Whether any block of `[first, last)` is read-only by our hand.
pub(crate) fn any_protected(first: usize, last: usize) -> bool {
    if first >= last {
        return false;
    }
    if let Some(g) = one_stripe(first, last) {
        return g.protected.range(first..last).next().is_some();
    }
    let held = shared(first, last);
    Runs::new(first, last).any(|(s, lo, hi)| {
        let i = held.binary_search_by_key(&s, |&(s, _)| s).expect("held");
        held[i].1.protected.range(lo..hi).next().is_some()
    })
}

/// Whether every block of `[first, last)` is known there: asked under one
/// hold of their shards, so a range being unmapped meanwhile is never taken
/// for one wholly there.
pub(crate) fn all_mapped(first: usize, last: usize) -> bool {
    if first >= last {
        return false;
    }
    if let Some(g) = one_stripe(first, last) {
        return (first..last).all(|block| g.mapped.contains(&block));
    }
    let held = shared(first, last);
    Runs::new(first, last).all(|(s, lo, hi)| {
        let i = held.binary_search_by_key(&s, |&(s, _)| s).expect("held");
        (lo..hi).all(|block| held[i].1.mapped.contains(&block))
    })
}

/// A copy took all of `[first, last)`: every block of it is there.
pub(crate) fn mark_mapped(first: usize, last: usize) {
    if first >= last {
        return;
    }
    // The runs first: nothing proportional to the range is walked with the
    // shards locked.
    let runs: Vec<(usize, usize, usize)> = Runs::new(first, last).collect();
    let mut w = Writing::of_range(first, last);
    for (s, lo, hi) in runs {
        w.at(s).mapped.extend(lo..hi);
    }
}

/// The runtime remapped `[at, at + len)`: nothing there is guarded, known
/// uncommitted or known there any more.
pub(crate) fn remapped(at: usize, len: usize) {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    if first >= last {
        return;
    }
    let mut w = Writing::of_range(first, last);
    // Each shard's set holds only its own blocks: the range is searched, not
    // walked a stripe at a time (a 16 GiB remap is 16,384 stripes).
    for (_, g) in &mut w.0 {
        let g: &mut Guards = g;
        let absent: Vec<usize> = g.absent.range(first..last).copied().collect();
        for block in absent {
            g.absent.remove(&block);
        }
        let was: Vec<usize> = g.mapped.range(first..last).copied().collect();
        for block in was {
            g.mapped.remove(&block);
        }
        let blocks: Vec<usize> = g.protected.range(first..last).copied().collect();
        for block in blocks {
            g.protected.remove(&block);
            g.clean.remove(block);
        }
    }
}

/// Forgets the guard notes of `blocks` (opened for a kernel write that did
/// not fault into [`open`]).
pub(crate) fn opened(blocks: &[usize]) {
    for &block in blocks {
        let mut g = slot(shard(block, shards()));
        g.protected.remove(&block);
        g.clean.remove(block);
    }
}

/// Forgets the clean notes of the blocks holding `addresses`, written through
/// another view of the same physical pages (xmem's `write_physical`, a
/// DirectStorage page destination): what guards those blocks stays in place,
/// but a host copy of the memory is stale again.
pub(crate) fn written_through(addresses: impl Iterator<Item = usize>) {
    for at in addresses {
        let block = at / BLOCK;
        let mut g = slot(shard(block, shards()));
        if g.protected.contains(&block) {
            g.clean.remove(block);
        }
    }
}

/// Whether the block holding `at` is read-only by our hand.
pub(crate) fn guarded(at: usize) -> bool {
    let block = at / BLOCK;
    reading(shard(block, shards())).protected.contains(&block)
}

/// Guards the view blocks of `[first, last)` again, returning the pages of
/// those written since they were last guarded (all of a block's pages).
/// `watch::guard` has checked that the range is view memory.
pub(crate) fn reguard(first: usize, last: usize) -> Vec<usize> {
    let alias = aliases();
    let mut written = Vec::new();
    // One answer covers a whole uncommitted region, across stripes.
    let mut uncommitted_until = first;
    for (s, lo, hi) in Runs::new(first, last) {
        let mut g = slot(s);
        let g = &mut *g;
        for (from, to) in g.clean.uncovered(lo, hi) {
            let mut run = None;
            for block in from..to {
                let mut open = true;
                if g.absent.contains(&block) {
                    // Ordinary commits need no runtime mapping notification. Reuse
                    // this asking's query for the following blocks in the same region.
                    if block >= uncommitted_until {
                        uncommitted_until = uncommitted_end(block);
                    }
                    open = block >= uncommitted_until;
                    if open {
                        g.absent.remove(&block);
                    }
                }
                if open && alias && crate::inplace::aliased(block * BLOCK) {
                    // Given back if an earlier pass guarded it; never guarded
                    // again, and never clean.
                    open_blocks(g, block, block + 1);
                    written.extend(block * PAGES..(block + 1) * PAGES);
                } else if open {
                    written.extend(block * PAGES..(block + 1) * PAGES);
                    if g.protected.contains(&block) {
                        // Written through the runtime, still read-only.
                        g.clean.insert(block, block + 1);
                    } else {
                        run.get_or_insert(block);
                        continue;
                    }
                }
                if let Some(begin) = run.take() {
                    protect(g, begin, block);
                }
            }
            if let Some(begin) = run {
                protect(g, begin, to);
            }
        }
    }
    written
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent. One shard's: `from` and
/// `to` lie in the same run.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.insert(from, to);
        // The kernel only made committed memory read-only: it is there.
        g.mapped.extend(from..to);
    };
    // Made read-only by our hand: not a plain copy's destination now.
    crate::watch::forget(from * BLOCK, (to - from) * BLOCK);
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else if block < uncommitted_end(block) {
            g.absent.insert(block);
            g.mapped.remove(&block);
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
pub(crate) fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and notes
/// them written; true if any was guarded. The destination answers of the
/// blocks opened go: they are writable again by our hand.
pub(crate) fn open(start: usize, end: usize) -> bool {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if first >= last {
        return false;
    }
    let mut w = Writing::of_range(first, last);
    let mut span: Option<(usize, usize)> = None;
    for (s, lo, hi) in Runs::new(first, last) {
        if let Some((from, to)) = open_in(w.at(s), lo, hi) {
            span = Some(match span {
                Some((f, t)) => (f.min(from), t.max(to)),
                None => (from, to),
            });
        }
    }
    let Some((from, to)) = span else {
        return false;
    };
    crate::watch::forget(from * BLOCK, (to - from) * BLOCK);
    true
}

/// The block of `at` opened as [`open`] does, or whether something else may
/// write it (`foreign`, asked under the same hold of its shard): the two
/// answers cannot disagree with a copy guarding the block meanwhile.
pub(crate) fn open_block(at: usize, foreign: impl FnOnce() -> bool) -> bool {
    let block = at / BLOCK;
    let mut g = slot(shard(block, shards()));
    if open_in(&mut g, block, block + 1).is_some() {
        crate::watch::forget(block * BLOCK, BLOCK);
        return true;
    }
    foreign()
}

/// [`open`] of one shard's run: the blocks opened, first and end.
fn open_in(g: &mut Guards, lo: usize, hi: usize) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g.protected.range(lo..hi).copied().collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(block);
        // It was read-only by our hand, so it is committed and mapped.
        g.mapped.insert(block);
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
    }
    blocks.first().zip(blocks.last()).map(|(a, b)| (*a, b + 1))
}

/// [`open`] of blocks within one shard's run, forgetting the destination
/// answers of the span they cover (a copy's guard given back).
fn open_blocks(g: &mut Guards, lo: usize, hi: usize) {
    if let Some((from, to)) = open_in(g, lo, hi) {
        crate::watch::forget(from * BLOCK, (to - from) * BLOCK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Two block numbers of different shards from `base`, if there is more
    /// than one shard.
    fn two_shards(base: usize) -> Option<(usize, usize)> {
        (shards() > 1).then_some((base, base + STRIPE))
    }

    /// The read path a copy takes ([`all_mapped`]) and the write path a
    /// fault or a remap takes ([`remapped`]) as a range's blocks change.
    #[test]
    fn marks_of_one_shard_are_seen_in_another() {
        let Some((a, b)) = two_shards(4_000_000) else {
            panic!("sharding off (VICTORY_GUARD_SHARDS=1): every guard waits for every other");
        };
        mark_mapped(a, a + 1);
        mark_mapped(b, b + 1);
        assert!(all_mapped(a, a + 1));
        assert!(all_mapped(b, b + 1));
        assert!(!all_mapped(a, a + 2), "nothing held the block between");
        remapped(a * BLOCK, BLOCK);
        assert!(!all_mapped(a, a + 1));
        assert!(all_mapped(b, b + 1), "the other shard keeps its marks");
    }

    /// A copy holding one shard in its hands does not stop a remap of a block
    /// in another shard; with one lock for all blocks the second waits for
    /// the first, which is the stall sharding removes. Bounded, so a deadlock
    /// fails the test instead of hanging the suite.
    #[test]
    fn a_held_shard_does_not_stop_another() {
        let Some((a, b)) = two_shards(6_000_000) else {
            panic!("sharding off (VICTORY_GUARD_SHARDS=1): every guard waits for every other");
        };
        let (holding, held) = mpsc::channel();
        let (released, go) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = reading(shard(a, shards()));
            holding.send(()).unwrap();
            go.recv().unwrap();
        });
        held.recv_timeout(Duration::from_secs(5))
            .expect("the holder takes its shard");

        let (finished, joined) = mpsc::channel();
        let other = std::thread::spawn(move || {
            remapped(b * BLOCK, BLOCK);
            finished.send(()).unwrap();
        });
        joined
            .recv_timeout(Duration::from_secs(5))
            .expect("a remap in another shard does not wait for a held shard");
        other.join().unwrap();

        // The held shard itself does wait: the lock is not a no-op.
        let (blocked, waited) = mpsc::channel();
        let same = std::thread::spawn(move || {
            remapped(a * BLOCK, BLOCK);
            blocked.send(()).unwrap();
        });
        assert!(
            waited.recv_timeout(Duration::from_millis(300)).is_err(),
            "a remap took a shard held for reading"
        );
        released.send(()).unwrap();
        holder.join().unwrap();
        waited
            .recv_timeout(Duration::from_secs(5))
            .expect("released, the remap of the held shard finishes");
        same.join().unwrap();
    }

    /// Threads of copies, of remaps and of faults' questions on their own
    /// blocks all finish: no thread waits on another's shard, and none waits
    /// forever. Bounded, so a deadlock fails instead of hanging.
    #[test]
    fn copies_remaps_and_faults_of_different_blocks_all_finish() {
        let (tx, rx) = mpsc::channel();
        for t in 0..8 {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let base = 1_000_000 + t * 8 * STRIPE;
                for i in 0..2_000 {
                    let block = base + i % (4 * STRIPE);
                    let _ = all_mapped(block, block + 1);
                    let _ = block_known(block * BLOCK);
                    let _ = holds(block * BLOCK);
                    if i % 3 == 0 {
                        mark_mapped(block, block + 1);
                    }
                    if i % 5 == 0 {
                        remapped(block * BLOCK, BLOCK);
                    }
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);
        for _ in 0..8 {
            rx.recv_timeout(Duration::from_secs(60))
                .expect("every thread of marks finishes");
        }
    }

    #[test]
    fn clean_runs_match_per_block_tracking() {
        let mut runs = CleanBlocks(BTreeMap::new());
        let mut blocks = BTreeSet::new();
        let check = |runs: &CleanBlocks, blocks: &BTreeSet<usize>| {
            for from in 0..36 {
                for to in from..36 {
                    let actual: Vec<_> = runs.uncovered(from, to).into_iter().flat_map(|(a, b)| a..b).collect();
                    let expected: Vec<_> = (from..to).filter(|block| !blocks.contains(block)).collect();
                    assert_eq!(actual, expected, "{from}..{to}");
                }
            }
        };

        for (from, to) in [(5, 12), (1, 3), (3, 7), (23, 33), (17, 20), (9, 26), (2, 3)] {
            runs.insert(from, to);
            blocks.extend(from..to);
            check(&runs, &blocks);
        }
        for block in [0, 1, 11, 14, 15, 32, 13, 12] {
            runs.remove(block);
            blocks.remove(&block);
            check(&runs, &blocks);
        }
        runs.insert(11, 16);
        blocks.extend(11..16);
        check(&runs, &blocks);
    }

    /// What a guard costs: the read path a copy takes ([`all_mapped`], one
    /// block) and the write path a remap or a fault takes ([`remapped`], one
    /// block), alone and on eight threads of blocks of their own.
    /// `VICTORY_GUARD_SHARDS=1` measures the single lock.
    #[test]
    fn guard_acquisition_cost_alone_and_on_eight_threads() {
        const OPS: usize = 100_000;
        const READS: usize = 8_000_000;
        const WRITES: usize = 16_000_000;
        let n = shards();
        let read = |t: usize, i: usize| {
            let block = READS + t * (1 << 20) + i;
            let _ = all_mapped(block, block + 1);
        };
        let write = |t: usize, i: usize| remapped((WRITES + t * (1 << 20) + i) * BLOCK, BLOCK);
        let alone = |f: &dyn Fn(usize, usize)| {
            let started = Instant::now();
            for i in 0..OPS {
                f(0, i);
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
        let (r1, r8, w1, w8) = (alone(&read), eight(&read), alone(&write), eight(&write));
        eprintln!(
            "guards: {n} shards, {OPS} ops — read (all_mapped, 1 block): {r1:.1} ns alone, {r8:.1} ns \
             on 8 threads; write (remapped, 1 block): {w1:.1} ns alone, {w8:.1} ns on 8"
        );
        assert!(
            r8 < 20_000.0 && w8 < 20_000.0,
            "acquisitions cost microseconds: {r8:.0}/{w8:.0} ns"
        );
    }
}
