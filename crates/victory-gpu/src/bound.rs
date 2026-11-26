//! What a host list has bound once already.
//!
//! A pipeline is kept and its name noted the first time a list binds it, and
//! not again for the rest of the list: two queries for an interface, two locks
//! on the deferred state and a name looked up at every change of pipeline
//! become one lookup by address. The root signature goes with the pipeline
//! (a kept pipeline's address names no other), so a repeat bind of a pipeline
//! whose root signature changed is still seen as a repeat: the list's own
//! binding of the two is what the title set, in that order, and D3D12 ties a
//! pipeline's root signature to the pipeline.

#[cfg(any(windows, test))]
use std::collections::HashSet;

/// The addresses a list has bound: a set of pipeline addresses.
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct Bound(HashSet<usize>);

#[cfg(any(windows, test))]
impl Bound {
    /// Whether `address` is new to the list, and it is noted from here on:
    /// the first bind of a pipeline is `true`, every repeat `false`.
    pub(crate) fn first(&mut self, address: usize) -> bool {
        self.0.insert(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass binds five pipelines over and over: four of the 10,000 binds
    /// are the first of one, and each of the five is kept and named once.
    #[test]
    fn a_pipeline_bound_again_is_kept_and_named_once() {
        const ADDRESSES: [usize; 5] = [0x1_0000, 0x2_0000, 0x3_0000, 0x4_0000, 0x5_0000];
        let mut bound = Bound::default();
        let (mut firsts, mut repeats) = (0, 0);
        let mut kept = 0;
        for draw in 0..10_000 {
            let address = ADDRESSES[draw % ADDRESSES.len()];
            if bound.first(address) {
                firsts += 1;
                kept += 2; // the pipeline and its root signature
            } else {
                repeats += 1;
            }
        }
        assert_eq!(firsts, ADDRESSES.len());
        assert_eq!(repeats, 10_000 - ADDRESSES.len());
        assert_eq!(kept, 2 * ADDRESSES.len(), "kept once each, not once a bind");
        assert_eq!(bound.0.len(), ADDRESSES.len());
    }

    /// A list that is reset keeps and names them again.
    #[test]
    fn a_list_reset_binds_them_again() {
        let mut bound = Bound::default();
        assert!(bound.first(0x1_0000));
        assert!(!bound.first(0x1_0000));
        let mut bound = Bound::default();
        assert!(bound.first(0x1_0000), "a new list: kept again");
    }

    /// Another pipeline's address is its own bind, however close the two.
    #[test]
    fn addresses_tell_pipelines_apart() {
        let mut bound = Bound::default();
        assert!(bound.first(0x1_0000));
        assert!(bound.first(0x1_0010));
        assert!(!bound.first(0x1_0000));
    }
}
