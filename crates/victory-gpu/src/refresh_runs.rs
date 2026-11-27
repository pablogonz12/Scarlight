//! The ranges a submission's copies took, as the ranges themselves.
//!
//! A submission records the same big buffer many times, in small pieces: the
//! heap's ready list alone adds some 4,100 copies. Merging them into one
//! `start..end` per copy (what this module replaces) made every refresh ask
//! the write watch, and read the title, over the gaps between the pieces: a
//! 4 MiB window asked for a few KiB of vertices walked its whole 4 MiB in
//! the kernel, and pages between the pieces the copy had never taken were
//! copied whole. The bytes the title asked for, and the bytes that reach the
//! copy, are the same either way; only the span walked changes.

use std::sync::OnceLock;

/// Whether refreshes are the ranges a copy took rather than one span over
/// them (`VICTORY_REFRESH_RUNS`, on by default; `=0` is the old shape).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REFRESH_RUNS").is_none_or(|v| v != "0"))
}

/// The most runs one copy keeps: past this the pieces are merged into the
/// span over them (the title asks for a buffer in thousands of pieces in a
/// bad frame, and a run each would cost more than the span).
pub(crate) const MOST_RUNS: usize = 64;

/// Adds `[from, to)` to a copy's runs, merging it with the runs it touches.
pub(crate) fn add(runs: &mut Vec<(usize, usize)>, from: usize, to: usize) {
    let mut from = from;
    let mut to = to;
    let mut i = 0;
    while i < runs.len() {
        let (a, b) = runs[i];
        if a <= to && from <= b {
            from = from.min(a);
            to = to.max(b);
            runs.remove(i);
        } else {
            i += 1;
        }
    }
    let at = runs.partition_point(|&(a, _)| a < from);
    runs.insert(at, (from, to));
    if runs.len() > MOST_RUNS {
        let (a, b) = runs.iter().fold((usize::MAX, 0), |(a, b), &(x, y)| (a.min(x), b.max(y)));
        runs.clear();
        runs.push((a, b));
    }
}

#[cfg(test)]
mod tests {
    use super::{add, MOST_RUNS};

    #[test]
    fn pieces_of_one_copy_are_runs_not_the_span_between_them() {
        // A 4 MiB window asked for 8 KiB at each end: the old shape made one
        // 4 MiB refresh (it walked the gap in the kernel and copied it).
        let mut runs = Vec::new();
        add(&mut runs, 0, 0x2000);
        add(&mut runs, 0x3ff000, 0x400000);
        assert_eq!(runs, vec![(0, 0x2000), (0x3ff000, 0x400000)]);
        // The shape this replaces: `Batch::add` kept one span per copy.
        assert_ne!(runs, vec![(0, 0x400000)]);
        // Adjacent and overlapping pieces are one run.
        add(&mut runs, 0x2000, 0x3000);
        assert_eq!(runs, vec![(0, 0x3000), (0x3ff000, 0x400000)]);
        add(&mut runs, 0x2fff, 0x4000);
        assert_eq!(runs, vec![(0, 0x4000), (0x3ff000, 0x400000)]);
        add(&mut runs, 0x3f000, 0x3ff100);
        assert_eq!(runs, vec![(0, 0x4000), (0x3f000, 0x400000)]);
    }

    #[test]
    fn a_copy_with_too_many_pieces_falls_back_to_the_span_over_them() {
        let mut runs = Vec::new();
        for i in 0..=MOST_RUNS {
            add(&mut runs, i * 0x10_0000, i * 0x10_0000 + 0x1000);
        }
        assert_eq!(runs, vec![(0, MOST_RUNS * 0x10_0000 + 0x1000)]);
    }
}
