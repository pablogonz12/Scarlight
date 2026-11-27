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
//! again; of the others, one page in [`slice`] is looked at each time, in
//! turn, so that every page of a range is looked at once in 64 submissions
//! and nothing stays missed. The turn is the pass's, not a range's
//! (`next_turn`): every page of every span of a submission is looked at on
//! the same rule, so a copy with two spans in one submission covers both.
//! Ranges of at most [`slice`] pages are compared whole: the rotation would
//! find a change there late for no saving.
//!
//! What is found late is not landed late, and nothing is lost, as long as
//! the page's bytes and their reference stay as they are between looks:
//! the compare reads the copy against its snapshot, and a page left out
//! keeps both, so the difference is still there when its turn comes and is
//! then put back into title memory as any other (`Shadow::copy_back`). That
//! is why only pages whose snapshot piece is *kept* and current are left
//! out (`Shadow::baseline_pages`: `VICTORY_KEEP_SNAPSHOTS=1`, a piece
//! taken whole and not taken again since), and why unwatched memory, which
//! a refresh copies whole, is never left out.
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
    slice().max(1)
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
                "VICTORY_PICKED_WRITES=1 needs VICTORY_KEEP_SNAPSHOTS=1 (a page left out is only found again if \
                 its snapshot piece is kept and current); every declared range is compared whole"
            );
        }
        true
    })
}

/// Since the last frame-rate line: bytes compared and bytes left out.
static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// What the picked compares did since the last line, for the frame line.
pub(crate) fn log(frames: f64) {
    let c: Vec<f64> = COUNTS.iter().map(|c| c.swap(0, Relaxed) as f64 / frames / (1 << 20) as f64).collect();
    if c.iter().all(|&v| v == 0.0) {
        return;
    }
    tracing::info!("compared per frame with picked writes: {:.1} MB of {:.1} MB declared", c[0], c[0] + c[1]);
}

/// The copy's turn: its own count, one per compare of a submission's ranges
/// and the same for every span of it (see [`spans`]). Per copy, so that a
/// copy compares on every other submission of two still walks every one of
/// its pages (`turn % slice` would otherwise only ever be the parity of its
/// own turns, and half its pages never look).
pub(crate) fn next_turn(id: u64) -> Option<u64> {
    let mut turns = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    next_turn_in(&mut turns, id)
}

fn next_turn_in(turns: &mut BTreeMap<u64, u64>, id: u64) -> Option<u64> {
    if !turns.contains_key(&id) && turns.len() >= KEPT {
        return None;
    }
    let t = turns.entry(id).or_insert(0);
    let now = *t;
    *t = t.wrapping_add(1);
    Some(now)
}

/// Each copy's count of compares so far, its turn ([`next_turn`]).
static TURNS: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());

/// The pages a copy's compares have found written: looked at every time
/// from then on, counted from the copy's first page.
#[derive(Default)]
struct Learned {
    learned: BTreeSet<usize>,
}

/// Copies with a set.
static SETS: Mutex<BTreeMap<u64, Learned>> = Mutex::new(BTreeMap::new());

/// At most this many copies keep a set (the world has a few hundred live).
const KEPT: usize = 4096;

/// The offsets of a copy of title memory from `start` (`id` is the copy's,
/// [`crate::shadow::Shadow::id`]) that its compare examines on `turn`: the
/// pages the shaders wrote so far and this turn's page in [`slice`], of
/// `[from, to)`.
///
/// `[from, to)` whole where `baseline` is `None` (nothing picked, a range
/// too small for the rotation), and where a page's snapshot piece is not
/// one the copy may leave out: `baseline` holds that per page of the range,
/// from its first, as [`crate::shadow::Shadow::baseline_pages`] gives it.
pub(crate) fn spans(
    turn: u64,
    id: u64,
    start: usize,
    from: usize,
    to: usize,
    baseline: Option<&[bool]>,
) -> Vec<(usize, usize)> {
    if from >= to {
        return Vec::new();
    }
    let base = start / PAGE;
    let first = (start + from) / PAGE - base;
    let last = (start + to - 1) / PAGE - base;
    let skip = match baseline {
        Some(baseline) if last - first >= small() && baseline.len() == last - first + 1 => baseline,
        _ => {
            COUNTS[0].fetch_add((to - from) as u64, Relaxed);
            return vec![(from, to)];
        }
    };
    let exams: Vec<bool> = (first..=last)
        .map(|page| {
            let learned = SETS.lock().unwrap_or_else(|e| e.into_inner()).get(&id).is_some_and(|s| s.learned.contains(&page));
            // (A page whose snapshot piece is not current is never left out.)
            !skip[page - first] || learned || page % slice() == (turn as usize) % slice()
        })
        .collect();
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

/// Without a retained turn, comparing the whole range prevents starvation.
pub(crate) fn spans_for_turn(
    turn: Option<u64>, id: u64, start: usize, from: usize, to: usize, baseline: Option<&[bool]>,
) -> Vec<(usize, usize)> {
    match turn {
        Some(turn) => spans(turn, id, start, from, to, baseline),
        None if from < to => vec![(from, to)],
        None => Vec::new(),
    }
}

/// The pages of the copy from `start` that the pieces `[at, at + n)` of
/// `found` hold are learned: they are compared every time from now on.
pub(crate) fn found(id: u64, start: usize, found: &[(usize, usize)]) {
    if found.is_empty() {
        return;
    }
    let base = start / PAGE;
    let mut sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
    if sets.len() >= KEPT && !sets.contains_key(&id) {
        sets.clear();
    }
    let sweep = sets.entry(id).or_default();
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
    TURNS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

#[cfg(test)]
mod tests {
    use super::{PAGE, found, slice, spans};
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

    /// One look of a copy of title memory from `start` on `turn`: `changed`
    /// are the numbers (`address / PAGE`) of the pages that differ from the
    /// snapshot, `baseline` which pages may be left out. Returns the changed
    /// pages found, and learns them as the compare does (`found` is told
    /// the pieces that differ).
    fn look(
        turn: u64,
        id: u64,
        start: usize,
        from: usize,
        to: usize,
        baseline: Option<&[bool]>,
        changed: &BTreeSet<usize>,
    ) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for (a, e) in spans(turn, id, start, from, to, baseline) {
            let pieces: Vec<(usize, usize)> = changed
                .iter()
                .copied()
                .filter(|&page| page * PAGE < start + e && (page + 1) * PAGE > start + a)
                .map(|page| (page * PAGE - start, PAGE))
                .collect();
            out.extend(pieces.iter().map(|&(at, _)| (start + at) / PAGE));
            found(id, start, &pieces);
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
            let whole = look(0, 2000 + case, start, from, to, None, &changed);
            assert_eq!(whole, changed, "{case}: a whole scan finds every changed page");
            // The picked path: look after look.
            let allowed = vec![true; pages];
            let id = 3000 + case;
            let mut picked = BTreeSet::new();
            for turn in 0..slice() + 1 {
                picked.extend(look(turn as u64, id, start, from, to, Some(&allowed), &changed));
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
        let allowed = vec![true; pages];
        let mut changed = BTreeSet::new();
        changed.insert(start / PAGE + pages - 1);
        let mut seen = BTreeSet::new();
        for turn in 0..slice() {
            let found = look(turn as u64, 7, start, 0, pages * PAGE, Some(&allowed), &changed);
            assert!(found.is_empty() || found == changed, "{turn}: nothing but the page changed");
            seen.extend(found);
        }
        assert_eq!(seen, changed, "the last page is found within a sweep");
    }

    /// Where nothing is picked, or the range is small, every byte of it is
    /// compared, as without the switch; and a page the copy may not leave
    /// out (its snapshot piece is not current) is looked at every time.
    #[test]
    fn a_page_that_may_not_be_left_out_is_looked_at_every_time() {
        let start = 0x3_0000;
        let pages = slice() * 8;
        let to = pages * PAGE;
        assert_eq!(spans(0, 11, start, 0, to, None), vec![(0, to)]);
        let allowed = vec![true; slice()];
        assert_eq!(spans(0, 12, start, 0, slice() * PAGE, Some(&allowed)), vec![(0, slice() * PAGE)]);
        // One page among many may not be left out: it is compared on every
        // turn, the others by rotation.
        let mut baseline = vec![true; pages];
        baseline[5] = false;
        for turn in 0..4u64 {
            let looked = spans(turn, 13, start, 0, to, Some(&baseline));
            let covers = |at: usize| looked.iter().any(|&(a, e)| a <= at && at < e);
            assert!(covers(5 * PAGE), "{turn}: page 5 is compared every time");
        }
    }

    /// A copy's sweep goes on when the map is full: its turn is not cleared
    /// back to 0 with everyone else's (the pages off its first then never
    /// look).
    #[test]
    fn a_full_map_does_not_restart_a_copys_sweep() {
        let id = 0x00fa_0000_0000_00d1u64;
        let mut turns = std::collections::BTreeMap::new();
        // A few of its own turns first, so a restart cannot hide behind 0.
        let mut first = None;
        for _ in 0..4 {
            first = super::next_turn_in(&mut turns, id);
        }
        assert_eq!(first, Some(3));
        // More copies than the map keeps, each asking once: a whole-range
        // scan of a big title does this, and so does a small one that learns.
        for i in 0..(super::KEPT as u64 * 2) {
            let other = 0x0100_0000_0000_0000u64 | i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            super::next_turn_in(&mut turns, other);
        }
        assert_eq!(super::next_turn_in(&mut turns, id), Some(4));
        assert_eq!(super::next_turn_in(&mut turns, u64::MAX), None);
        assert_eq!(super::spans_for_turn(None, u64::MAX, 0, 0, 8 * PAGE, Some(&[true; 8])), vec![(0, 8 * PAGE)]);
        assert!(super::spans_for_turn(None, u64::MAX, 0, 0, 0, None).is_empty());
        turns.remove(&id);
        assert_eq!(super::next_turn_in(&mut turns, u64::MAX), Some(0));
    }

    /// The spans are of the range, in order, never overlapping; over a
    /// sweep they cover it — also when a copy has two spans in one look,
    /// which share the pass's turn: each span covers its own pages over the
    /// same turns, so no page is left out for good.
    #[test]
    fn the_spans_are_of_the_range_and_cover_it_over_a_sweep() {
        let start = 0x10_0000;
        let to = slice() * 3 * PAGE;
        let mut covered = vec![false; to];
        for turn in 0..slice() {
            let looked = spans(turn as u64, 14, start, PAGE, to, Some(&vec![true; to / PAGE]));
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
        assert_eq!(spans(0, 15, start, PAGE, PAGE, Some(&[true])), Vec::new(), "an empty range");

        // Two spans of one copy in one submission (a `hand_back_now` work
        // item each), the same turn: every page of both is looked at over
        // the sweep.
        let pages = slice() * 4;
        let all = vec![true; pages * 2];
        let mut seen = vec![false; pages * 2];
        for turn in 0..slice() as u64 {
            for (from, to) in [(0usize, pages * PAGE), (pages * PAGE, pages * 2 * PAGE)] {
                for (a, e) in spans(turn, 16, start, from, to, Some(&all[from / PAGE..to / PAGE])) {
                    let first = a / PAGE;
                    let last = (e - 1) / PAGE;
                    for page in first..=last {
                        seen[page] = true;
                    }
                }
            }
        }
        assert!(seen.iter().all(|&s| s), "both spans are covered over a sweep");
    }

    /// What the compare of 4,096 pages (256 MB) costs whole and picked,
    /// with one page in 64 of them changed (the frame the switch is for).
    ///
    /// `cargo test --release -p victory-gpu --lib compare_of_4k_pages -- --nocapture`
    #[test]
    fn compare_of_4k_pages_whole_and_picked() {
        use std::time::Instant;
        let pages = 4096;
        let len = pages * PAGE;
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
        let picked = |turn: u64, data: &[u8], before: &[u8], baseline: &[bool]| -> Vec<(usize, usize)> {
            let mut pieces = Vec::new();
            for (a, e) in spans(turn, 1, start, 0, len, Some(baseline)) {
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
        let time = |f: &mut dyn FnMut()| {
            let started = Instant::now();
            f();
            started.elapsed()
        };
        let baseline = vec![true; pages];
        let mut whole_found = Vec::new();
        let whole_us = time(&mut || whole_found = whole(&data, &before));
        let mut picked_found = Vec::new();
        // The look whose turn's pages are the changed ones (page 37 in 64).
        let picked_us = time(&mut || picked_found = picked(37, &data, &before, &baseline));
        assert_eq!(whole_found.len(), changed.len(), "a whole scan finds them all");
        assert_eq!(picked_found.len(), changed.len(), "the look of their turn finds them all");
        eprintln!("4,096 pages, {} changed: whole {whole_us:?}, picked {picked_us:?}", changed.len());
    }
}
