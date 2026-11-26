//! What one host heap keeps between passes over its buffer views: the slot
//! table, the frame whose lists already made it, and how long ago each
//! unwatched host copy was refreshed.
//!
//! The pass that refreshes the host copies behind buffer views
//! ([`crate::draw::State::heap_buffers`]) once kept this in one process-wide
//! map keyed by the heap's address, and its once-a-frame gate in one static:
//! with two heaps, only the first to be drawn each frame was refreshed, and a
//! new heap that happened to land on a freed heap's address took over that
//! heap's slot table (its slots then read copies of buffers it does not hold).
//! Kept on the heap itself, each heap refreshes its own slots once a frame and
//! takes nothing over from a heap that is gone.
//!
//! Ported from codec-d/cool_fox "Scope bindless refreshes to their heap"
//! (18bdf7932).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Passes between the ones that look at every slot: a view of a released
/// resource goes stale without a write, and a slot written before its memory
/// had a host copy is only made then.
const HEAP_ROUND: u64 = 16;

/// Unwatched copies refreshed, skipped as covered by the last refresh, or
/// refreshed because they now cover more than it did.
pub(crate) static REFRESHED: AtomicU64 = AtomicU64::new(0);
pub(crate) static SKIPPED: AtomicU64 = AtomicU64::new(0);
pub(crate) static WIDENED: AtomicU64 = AtomicU64::new(0);

/// The counters above, and starts them over.
pub(crate) fn take() -> [u64; 3] {
    [
        REFRESHED.swap(0, Ordering::Relaxed),
        SKIPPED.swap(0, Ordering::Relaxed),
        WIDENED.swap(0, Ordering::Relaxed),
    ]
}

pub(crate) struct Sweep {
    passes: u64,
    /// The last frame whose lists refreshed this heap.
    frame: AtomicU64,
    /// Slot: the host copy and the first and last byte its buffer view reads.
    pub(crate) ranges: HashMap<u32, (Arc<crate::shadow::Shadow>, u64, u64)>,
    /// Host copy: the frame it was last refreshed, and the span it covered
    /// then. Copies no write watch covers are copied whole, so the span is
    /// what decides whether the last refresh is still the newest state.
    unwatched: HashMap<u64, (u64, u64, u64)>,
    /// The same, for the copies this pass looks at (swapped in at its end).
    next: HashMap<u64, (u64, u64, u64)>,
}

impl Default for Sweep {
    fn default() -> Self {
        Self {
            passes: 0,
            frame: AtomicU64::new(u64::MAX),
            ranges: HashMap::new(),
            unwatched: HashMap::new(),
            next: HashMap::new(),
        }
    }
}

impl Sweep {
    /// Whether this heap's pass may run in `frame`, and notes that it did.
    pub(crate) fn frame_due(&self, frame: u64) -> bool {
        self.frame.swap(frame, Ordering::Relaxed) != frame
    }

    /// The next pass number, and whether it looks at every slot.
    pub(crate) fn pass(&mut self) -> (u64, bool) {
        let passes = self.passes;
        self.passes += 1;
        (passes, passes % HEAP_ROUND == 0)
    }

    /// Whether the unwatched copy `id`, whose views now read `from..to`, is
    /// refreshed in `frame`. It waits out `every` frames only while the span
    /// it reads is inside the one the last refresh covered: a wider span
    /// names bytes that refresh did not copy.
    pub(crate) fn due(&mut self, id: u64, frame: u64, every: u64, from: u64, to: u64) -> bool {
        if let Some(&(last, low, high)) = self.unwatched.get(&id) {
            if frame >= last && frame - last < every && low <= from && to <= high {
                SKIPPED.fetch_add(1, Ordering::Relaxed);
                self.next.insert(id, (last, low, high));
                return false;
            }
            if low > from || high < to {
                WIDENED.fetch_add(1, Ordering::Relaxed);
            }
        }
        REFRESHED.fetch_add(1, Ordering::Relaxed);
        self.next.insert(id, (frame, from, to));
        true
    }

    /// The end of the pass: copies it did not look at are forgotten, so a
    /// copy that comes back is refreshed rather than trusted.
    pub(crate) fn end_pass(&mut self) {
        self.unwatched = std::mem::take(&mut self.next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_heap_refreshes_once_a_frame_of_its_own() {
        // Two heaps drawn in one frame: the second must not be taken for the
        // first (the static this replaced let one heap through a frame).
        let (a, b) = (Sweep::default(), Sweep::default());
        assert!(a.frame_due(7) && b.frame_due(7));
        assert!(!a.frame_due(7) && !b.frame_due(7));
        assert!(a.frame_due(8) && b.frame_due(8));
    }

    #[test]
    fn a_heap_new_at_a_freed_address_starts_with_no_slots() {
        // `ranges` is per heap now: nothing is left to inherit.
        let mut old = Sweep::default();
        old.ranges.reserve(1);
        old.pass();
        let new = Sweep::default();
        assert!(new.ranges.is_empty());
        assert_eq!(new.pass().0, 0, "the first pass looks at every slot");
    }

    #[test]
    fn an_unwatched_copy_waits_only_while_it_reads_less_than_it_did() {
        let mut s = Sweep::default();
        assert!(s.due(1, 10, 4, 100, 200));
        s.end_pass();
        // Inside the span the refresh at frame 10 copied: waits.
        assert!(!s.due(1, 11, 4, 120, 180));
        s.end_pass();
        // One byte below it was never copied: refreshed.
        assert!(s.due(1, 12, 4, 99, 180));
        s.end_pass();
        // One byte above.
        assert!(s.due(1, 13, 4, 120, 201));
        s.end_pass();
        // The interval is over.
        assert!(s.due(1, 17, 4, 150, 160));
        s.end_pass();
        // Every frame asks.
        assert!(s.due(2, 17, 1, 0, 8) && s.due(2, 17, 1, 0, 8));
    }

    #[test]
    fn a_copy_the_pass_did_not_look_at_is_refreshed_when_it_returns() {
        let mut s = Sweep::default();
        assert!(s.due(1, 10, 8, 0, 64));
        s.end_pass();
        s.end_pass(); // a pass that saw no views of it
        assert!(s.due(1, 11, 8, 0, 64));
    }
}
