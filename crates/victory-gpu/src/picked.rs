//! Of a declared range, only the 64 KiB pages the shaders wrote are
//! compared (`VICTORY_PICKED_WRITES=1`; off by default).
//!
//! A submission declares the ranges its unordered-access views cover, and
//! every byte of them is compared with the copy's bytes from before it
//! (`crate::shadow::find_all`): a title buffer's views are declared whole,
//! so a shader that sets a few words of a 24 MB buffer costs a compare of
//! 24 MB. Most of a declared range was not written. Which pages were is
//! learned from the compares themselves: a page that differed is looked at
//! again every time, on the ground that a page written once is written
//! again; of the others, one page in [`SLICE`] is looked at each time, in
//! turn, so that every page of a range is looked at once in `SLICE` (64)
//! submissions and nothing stays missed. Ranges of at most [`SMALL`] pages
//! are compared whole: the rotation would find a change there late for no
//! saving.
//!
//! What is found late is not landed late, and nothing is lost: the compare
//! reads the copy against its snapshot, and a page left out keeps both as
//! they were, so the difference is still there when its turn comes and is
//! then put back into title memory as any other (`Shadow::copy_back`). That
//! needs the snapshot to outlive the submission: with
//! `VICTORY_KEEP_SNAPSHOTS=1` it does, and only then is anything left out
//! (`Shadow::baseline_kept`); a copy whose snapshot is taken anew for each
//! submission is compared whole, as before.
//!
//! The idea, the rotation and the numbers are
//! avatarmanz/VIctory-7900XTX's (`VICTORY_PICKED_WRITES`, 69 -> 6 ms of
//! page compares a frame); this is the same scheme over our compares.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// The pages of title memory a compare cuts a range into: the 64 KiB
/// pieces the copy-back works in (`crate::shadow`).
pub(crate) const PAGE: usize = 64 << 10;

/// Of the pages not learned, one in this many is compared at each look, in
/// turn (`VICTORY_PICKED_SLICE`).
fn slice() -> usize {
    static SLICE: OnceLock<usize> = OnceLock::new();
    *SLICE.get_or_init(|| {
        std::env::var("VICTORY_PICKED_SLICE").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|&s| s > 0).unwrap_or(64)
    })
}

/// A range of at most this many pages is compared whole.
fn small() -> usize {
    static SMALL: OnceLock<usize> = OnceLock::new();
    *SMALL.get_or_init(|| slice().max(1))
}

/// Whether picked writes are on. Off unless `VICTORY_PICKED_WRITES=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var_os("VICTORY_PICKED_WRITES").is_some_and(|v| v == "1") {
            return false;
        }
        if !std::env::var_os("VICTORY_KEEP_SNAPSHOTS").is_some_and(|v| v == "1") {
            tracing::warn!(
                "VICTORY_PICKED_WRITES=1 needs VICTORY_KEEP_SNAPSHOTS=1 (the snapshot must outlive the submission, or a page \
                 left out is lost); every declared range is compared whole"
            );
        }
        true
    })
}

/// Since the last frame-rate line: pages compared and pages left out (zero
/// when nothing is picked).
static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// What the picked compares did since the last line, for the frame line.
pub(crate) fn log(frames: f64) {
    let c: Vec<f64> = COUNTS.iter().map(|c| c.swap(0, Relaxed) as f64 / frames).collect();
    if c.iter().all(|&v| v == 0.0) {
        return;
    }
    tracing::info!("pages compared per frame: {:.1} of {} looked at (picked writes)", c[0], c[1]);
}

/// The pages a copy's compares look at, learned as they find: over the
/// pages of one copy, counted from the copy's first page.
#[derive(Default)]
struct Sweep {
    /// Pages a compare found written: looked at every time.
    learned: BTreeSet<usize>,
    /// How many looks it has had: the page examined this time is the one
    /// whose number is the turn's, modulo [`slice`].
    turn: u64,
}

impl Sweep {
    fn examines(&self, page: usize, turn: u64) -> bool {
        self.learned.contains(&page) || page % slice() == (turn as usize) % slice()
    }
}

/// Copies with a set, and their bounds.
static SETS: Mutex<BTreeMap<u64, Sweep>> = Mutex::new(BTreeMap::new());

/// At most this many copies keep a set (the world has a few hundred live).
const KEPT: usize = 4096;

/// The offsets of a copy of title memory from `start` (`id` is the copy's,
/// [`crate::shadow::Shadow::id`]) that its compare examines this time: the
/// pages the shaders wrote so far and this turn's page in [`slice`], of
/// `[from, to)`.
///
/// `[from, to)` whole where nothing is picked, where the range is small,
/// and where the caller says the copy's snapshot does not outlive the
/// submission (`keep`: [`crate::shadow::Shadow::baseline_kept`]).
pub(crate) fn spans(id: u64, start: usize, from: usize, to: usize, keep: bool) -> Vec<(usize, usize)> {
    spans_on(on(), id, start, from, to, keep)
}

/// [`spans`], with the switch given: what the tests of the rule use, so
/// that they run whatever the environment says.
fn spans_on(picked: bool, id: u64, start: usize, from: usize, to: usize, keep: bool) -> Vec<(usize, usize)> {
    if from >= to {
        return Vec::new();
    }
    let base = start / PAGE;
    let first = (start + from) / PAGE - base;
    let last = (start + to - 1) / PAGE - base;
    if !picked || !keep || last - first < small() {
        COUNTS[0].fetch_add((to - from) as u64, Relaxed);
        return vec![(from, to)];
    }
    let turn = {
        let mut sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
        if sets.len() >= KEPT && !sets.contains_key(&id) {
            sets.clear();
        }
        let sweep = sets.entry(id).or_default();
        let turn = sweep.turn;
        sweep.turn = turn.wrapping_add(1);
        turn
    };
    spans_turn(turn, id, base, start, from, to)
}

/// [`spans_on`] of a turn given rather than taken: the looks of a copy in
/// the order a sweep runs them (the tests and the benchmark take them so).
fn spans_turn(turn: u64, id: u64, base: usize, start: usize, from: usize, to: usize) -> Vec<(usize, usize)> {
    let first = (start + from) / PAGE - base;
    let last = (start + to - 1) / PAGE - base;
    let exams: Vec<bool> = {
        let sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
        // (A copy no compare has had yet, or whose set went with the
        // table's clear: nothing learned, its turn's page only.)
        let exams = |page: usize| match sets.get(&id) {
            Some(sweep) => sweep.examines(page, turn),
            None => page % slice() == (turn as usize) % slice(),
        };
        (first..=last).map(exams).collect()
    };
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    for (page, &examined) in (first..=last).zip(&exams) {
        let lo = (((base + page) * PAGE).saturating_sub(start)).max(from);
        let hi = ((base + page + 1) * PAGE).saturating_sub(start).min(to);
        if hi <= lo {
            continue;
        }
        if examined {
            run = Some(match run {
                Some((a, _)) => (a, hi),
                None => (lo, hi),
            });
        } else if let Some((a, e)) = run.take() {
            out.push((a, e));
        }
    }
    if let Some((a, e)) = run {
        out.push((a, e));
    }
    let examined: usize = out.iter().map(|&(a, e)| e - a).sum();
    COUNTS[0].fetch_add(examined as u64, Relaxed);
    COUNTS[1].fetch_add((to - from - examined) as u64, Relaxed);
    out
}

/// The pages of the copy from `start` that the pieces `[at, at + n)` of
/// `found` hold are learned: they are compared every time from now on.
pub(crate) fn found(id: u64, start: usize, found: &[(usize, usize)]) {
    found_on(on(), id, start, found);
}

/// [`found`], with the switch given (see [`spans_on`]).
fn found_on(picked: bool, id: u64, start: usize, found: &[(usize, usize)]) {
    if !picked || found.is_empty() {
        return;
    }
    let base = start / PAGE;
    let mut sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sweep) = sets.get_mut(&id) else {
        return;
    };
    for &(at, n) in found {
        if n == 0 {
            continue;
        }
        for page in (start + at) / PAGE..=(start + at + n - 1) / PAGE {
            sweep.learned.insert(page - base);
        }
    }
}

/// Forgets a copy's set: its copy is gone, and a new one with the same
/// number does not inherit its pages.
pub(crate) fn forget(id: u64) {
    SETS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

#[cfg(test)]
mod tests {
    use super::{PAGE, found_on, slice, spans_on};
    use std::collections::BTreeSet;

    /// A deterministic stand-in for a random number generator (no crate of
    /// its own, no clock: the same walk on every machine).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// One look of a copy of title memory from `start`: `changed` are the
    /// numbers (`address / PAGE`) of the pages that differ from the
    /// snapshot. Returns the changed pages this look found, and learns them
    /// as the compare does (`found` is told the pieces that differ).
    fn look(id: u64, start: usize, from: usize, to: usize, keep: bool, changed: &BTreeSet<usize>) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for (a, e) in spans_on(true, id, start, from, to, keep) {
            let pieces: Vec<(usize, usize)> = changed
                .iter()
                .copied()
                .filter(|&page| page * PAGE < start + e && (page + 1) * PAGE > start + a)
                .map(|page| (page * PAGE - start, PAGE))
                .collect();
            out.extend(pieces.iter().map(|&(at, _)| (start + at) / PAGE));
            found_on(true, id, start, &pieces);
        }
        out
    }

    /// Over the looks a sweep takes, the picked compares find the pages a
    /// whole scan finds and nothing else, on ranges of every size: the
    /// picked path against the path that examines the whole range.
    #[test]
    fn the_picked_pages_found_are_the_ones_a_whole_scan_finds() {
        let mut rng = Rng(0x5eed_1234_9abc_def1);
        for case in 0..96 {
            let pages = 1 + rng.below(600);
            let start = 0x1_0000 + rng.below(4) * PAGE * 7;
            let (from, to) = (0, pages * PAGE);
            let mut changed = BTreeSet::new();
            for _ in 0..rng.below(48) {
                changed.insert(start / PAGE + rng.below(pages));
            }
            // The whole scan: one look, every changed page.
            let whole = look(2000 + case, start, from, to, false, &changed);
            assert_eq!(whole, changed, "{case}: a whole scan finds every changed page");
            // The picked path: look after look.
            let id = 3000 + case;
            let mut picked = BTreeSet::new();
            for _ in 0..slice() + 1 {
                picked.extend(look(id, start, from, to, true, &changed));
            }
            assert_eq!(picked, whole, "{case}: {pages} pages");
        }
    }

    /// A page the compare leaves out is not lost: its turn comes within a
    /// sweep, however big the range, and the change is found then.
    #[test]
    fn a_page_left_out_is_found_on_its_turn() {
        let start = 0x2_0000;
        let pages = slice() * 4;
        let mut changed = BTreeSet::new();
        changed.insert(start / PAGE + pages - 1);
        let mut seen = BTreeSet::new();
        for turn in 0..slice() {
            let found = look(7, start, 0, pages * PAGE, true, &changed);
            assert!(found.is_empty() || found == changed, "{turn}: nothing but the page changed");
            seen.extend(found);
        }
        assert_eq!(seen, changed, "the last page is found within a sweep");
    }

    /// Where the snapshot does not outlive the submission, every byte of
    /// the range is compared, as without the switch; and a small range is
    /// whole whatever the baseline.
    #[test]
    fn without_a_kept_snapshot_the_range_is_compared_whole() {
        let start = 0x3_0000;
        let to = slice() * 8 * PAGE;
        assert_eq!(spans_on(true, 11, start, 0, to, false), vec![(0, to)]);
        assert_eq!(spans_on(true, 12, start, 0, slice() * PAGE, true), vec![(0, slice() * PAGE)]);
    }

    /// The spans are of the range, in order, never overlapping; over a
    /// sweep they cover it.
    #[test]
    fn the_spans_are_of_the_range_and_cover_it_over_a_sweep() {
        let start = 0x10_0000;
        let to = slice() * 3 * PAGE;
        let mut covered = vec![false; to];
        for _ in 0..slice() {
            let looked = spans_on(true, 13, start, PAGE, to, true);
            assert!(!looked.is_empty());
            let mut at = PAGE;
            for (a, e) in looked {
                assert!(a >= at && e > a && e <= to, "{a} {e}");
                at = e;
                for i in a..e {
                    covered[i] = true;
                }
            }
        }
        assert!(covered[PAGE..].iter().all(|&c| c), "every byte gets its turn");
        assert_eq!(spans_on(true, 14, start, PAGE, PAGE, true), Vec::new(), "an empty range");
    }
}

/// What the compare of 4,096 pages (256 MB) costs whole and picked, with
/// one page in 64 of them changed (the frame the switch is for: a declared
/// range whose shaders set a few words). The picked path is the spans and
/// the compares they leave out, as `find_pieces` runs them.
///
/// `cargo test --release -p victory-gpu --lib compare_of_4k_pages -- --nocapture`
#[test]
fn compare_of_4k_pages_whole_and_picked() {
    use std::time::Instant;
    let pages = 4096;
    let len = pages * PAGE;
    // The copy and its snapshot, page 0 of the run's title memory.
    let start = 0x1_0000_0000usize;
    let mut data = vec![0xa5u8; len];
    let before = vec![0xa5u8; len];
    // 1 page in 64 changed, as the shaders of a frame leave it.
    let changed: Vec<usize> = (0..pages).filter(|page| page % 64 == 37).collect();
    for &page in &changed {
        for byte in &mut data[page * PAGE..(page + 1) * PAGE] {
            *byte ^= 0x01;
        }
    }
    // Whole: every page compared, as `find_pieces` does without the switch.
    let whole = |data: &[u8], before: &[u8]| -> Vec<(usize, usize)> {
        let mut pieces = Vec::new();
        for page in 0..pages {
            let (at, n) = (page * PAGE, PAGE);
            if data[at..at + n] != before[at..at + n] {
                pieces.push((at, n));
            }
        }
        pieces
    };
    // Picked: the spans of a look, and only those pages compared.
    let picked = |turn: u64, data: &[u8], before: &[u8]| -> Vec<(usize, usize)> {
        let mut pieces = Vec::new();
        // (The pages of the copy, counted from its first: 0..pages.)
        for (a, e) in spans_turn(turn, 1, start / PAGE, start, 0, len) {
            let mut at = a;
            while at < e {
                let n = (PAGE - (start + at) % PAGE).min(e - at);
                if data[at..at + n] != before[at..at + n] {
                    pieces.push((at, n));
                }
                at += n;
            }
        }
        pieces
    };
    let time = |mut f: Box<dyn FnMut()>| {
        let started = Instant::now();
        for _ in 0..4 {
            f();
        }
        started.elapsed() / 4
    };
    let mut whole_found = Vec::new();
    let whole_us = time(Box::new(|| whole_found = whole(&data, &before)));
    let mut picked_found = Vec::new();
    // The look whose turn's pages are the changed ones (page 37 in 64):
    // it compares them and nothing else.
    let picked_us = time(Box::new(|| picked_found = picked(37, &data, &before)));
    assert_eq!(whole_found.len(), changed.len(), "a whole scan finds them all");
    assert_eq!(picked_found.len(), changed.len(), "the look of their turn finds them all");
    eprintln!(
        "4,096 pages, {} changed: whole {whole_us:?}, picked {picked_us:?}",
        changed.len()
    );
}
