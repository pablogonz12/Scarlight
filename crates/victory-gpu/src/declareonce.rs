//! Ranges a list's shaders are declared to write, declared once.
//!
//! A dispatch declares the ranges its shaders write; the list copies each of
//! them back into title memory once it has run. Dispatches declare the same
//! few ranges again and again — thousands of steps a frame, each a flush of
//! the copy's pending refreshes and two locks on the thread that submits —
//! and declaring a range twice changes nothing until the list writes title
//! memory in between. The ranges and their order stay what they were.
//!
//! `VICTORY_DECLARE_ONCE=0` declares them every time, as before.

#[cfg(any(windows, test))]
use std::collections::HashSet;
#[cfg(any(windows, test))]
use std::sync::OnceLock;

/// Whether a range is declared once until the list next writes title
/// memory, from `VICTORY_DECLARE_ONCE` (`"0"`: every time).
#[cfg(any(windows, test))]
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on_from(std::env::var("VICTORY_DECLARE_ONCE").ok().as_deref()))
}

/// [`on`] of the value read.
#[cfg(any(windows, test))]
pub(crate) fn on_from(value: Option<&str>) -> bool {
    value != Some("0")
}

/// The ranges a list declared written by its shaders since it last wrote
/// title memory: (host copy, address, bytes).
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct Written(HashSet<(usize, usize, usize)>);

#[cfg(any(windows, test))]
impl Written {
    /// Whether `key` is new to the list, and it is declared from here on.
    pub(crate) fn first(&mut self, key: (usize, usize, usize)) -> bool {
        self.0.insert(key)
    }

    /// The list wrote title memory: what its shaders' declaration said about
    /// a range may not hold any more.
    pub(crate) fn forget(&mut self) {
        self.0.clear();
    }

    /// How many ranges the list is holding.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VICTORY_DECLARE_ONCE` is off for nothing but "0".
    #[test]
    fn the_switch_is_off_for_zero_alone() {
        assert!(on_from(None));
        assert!(on_from(Some("1")));
        assert!(!on_from(Some("0")));
    }

    /// 5,000 dispatches that declare the same three ranges leave three
    /// steps, not 15,000 — upstream counted 4,003 to 5,550 steps a frame
    /// of one fork.
    #[test]
    fn a_range_declared_again_is_one_step() {
        const RANGES: [(usize, usize, usize); 3] =
            [(0x1, 0x10_0000, 64), (0x1, 0x10_0040, 4096), (0x2, 0x20_0000, 256)];
        let mut written = Written::default();
        let mut steps = 0;
        for dispatch in 0..5_000 {
            let key = RANGES[dispatch % RANGES.len()];
            if written.first(key) {
                steps += 1;
            }
        }
        assert_eq!(steps, RANGES.len(), "one step a range");
        assert_eq!(written.len(), RANGES.len());

        // `VICTORY_DECLARE_ONCE=0`: a step a declaration.
        let mut written = Written::default();
        let mut steps = 0;
        for dispatch in 0..5_000 {
            let key = RANGES[dispatch % RANGES.len()];
            let once = on_from(Some("0"));
            if !once || written.first(key) {
                steps += 1;
            }
        }
        assert_eq!(steps, 5_000);
    }

    /// A list that writes title memory (a copy of the title's) declares them
    /// again: what its shaders wrote before may be gone.
    #[test]
    fn a_title_copy_declares_them_again() {
        let mut written = Written::default();
        assert!(written.first((1, 0x1000, 64)));
        assert!(!written.first((1, 0x1000, 64)));
        written.forget();
        assert!(written.first((1, 0x1000, 64)), "declared again after the copy");
    }

    /// Ranges that meet but are not the same are three declarations: the
    /// steps are not joined, and nothing may be left out.
    #[test]
    fn ranges_that_meet_are_told_apart() {
        let mut written = Written::default();
        assert!(written.first((1, 0x1000, 64)));
        assert!(written.first((1, 0x1040, 64)));
        assert!(written.first((1, 0x1000, 128)));
        assert!(written.first((2, 0x1000, 64)));
        assert_eq!(written.len(), 4);
    }
}
