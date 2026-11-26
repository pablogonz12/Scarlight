//! Whether the render targets and depth a draw sets are the ones the host
//! list has already.
//!
//! A title sets the same targets again and again — once per draw of a pass,
//! and again after a dispatch that changed nothing — and every call costs a
//! host view per target and one call of the driver. The state's last set is
//! kept as a key of what each view is made from (`State::set_targets`), and
//! a call with the same key is not made.
//!
//! `VICTORY_BIND_CACHE=0` sets them every time, as before.

#[cfg(any(windows, test))]
use std::sync::OnceLock;

/// Whether the same targets are left alone, from
/// `VICTORY_BIND_CACHE` (`"0"`: they are set every time).
#[cfg(any(windows, test))]
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on_from(std::env::var("VICTORY_BIND_CACHE").ok().as_deref()))
}

/// [`on`] of the value read.
#[cfg(any(windows, test))]
pub(crate) fn on_from(value: Option<&str>) -> bool {
    value != Some("0")
}

/// Whether `now` is the key of the set already on the list, `last` (and the
/// tool is on): nothing is to be set.
#[cfg(any(windows, test))]
pub(crate) fn same_with<K: PartialEq>(on: bool, last: Option<&K>, now: &K) -> bool {
    on && last == Some(now)
}

/// [`same_with`], with the switch as it is.
#[cfg(any(windows, test))]
pub(crate) fn same<K: PartialEq>(last: Option<&K>, now: &K) -> bool {
    same_with(on(), last, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VICTORY_BIND_CACHE` is off for nothing but "0".
    #[test]
    fn the_switch_is_off_for_zero_alone() {
        assert!(on_from(None), "unset: on");
        assert!(on_from(Some("1")));
        assert!(on_from(Some("")));
        assert!(!on_from(Some("0")), "VICTORY_BIND_CACHE=0: every call is made");
    }

    /// A key of four render targets and a depth view, as the state keeps it:
    /// per view the host texture and the description's words.
    type Key = (Vec<(usize, [u32; 7])>, Option<(usize, [u32; 7])>);

    fn key(textures: [usize; 4], depth: usize) -> Key {
        (
            textures.iter().map(|&t| (t << 16, [1, t as u32, 0, 0, 0, 0, 2])).collect(),
            Some((depth << 16, [3, depth as u32, 0, 0, 0, 0, 4])),
        )
    }

    /// Sets of targets a title asks for over `draws` draws, one set per draw:
    /// the same four targets, but every 500th draw brings a new one.
    fn asked(draws: usize) -> impl Iterator<Item = Key> {
        (0..draws).map(|draw| {
            let new = draw / 500;
            key([0, 1, 2, 3 + new], 9)
        })
    }

    /// Of 10,000 draws that set four targets each, those whose set is not
    /// made: with the tool off, none.
    #[test]
    fn the_same_targets_are_not_set_again() {
        let mut last: Option<Key> = None;
        let (mut made, mut saved) = (0, 0);
        for now in asked(10_000) {
            if same_with(true, last.as_ref(), &now) {
                saved += 1;
                continue;
            }
            made += 1;
            last = Some(now);
        }
        assert_eq!(made, 20, "a new target every 500 draws");
        assert_eq!(saved, 9_980);

        // `VICTORY_BIND_CACHE=0`: every draw sets them.
        let mut last: Option<Key> = None;
        let mut made = 0;
        for now in asked(10_000) {
            if same_with(false, last.as_ref(), &now) {
                continue;
            }
            made += 1;
            last = Some(now);
        }
        assert_eq!(made, 10_000);
    }

    /// A title that writes another view behind the same texture: the
    /// description's words tell them apart, the texture alone does not.
    #[test]
    fn another_description_of_one_texture_is_another_set() {
        let mut last = key([1, 2, 3, 4], 5);
        let mut other = last.clone();
        other.0[1].1[1] = 7; // another slice or mip of the same texture
        assert!(!same_with(true, Some(&last), &other), "different view");
        assert!(same_with(true, Some(&last), &last));
        last.0[2].0 = 0; // a null view in a slot
        assert!(!same_with(true, Some(&key([1, 2, 3, 4], 5)), &last));
    }

    /// What a comparison of a set costs: eight words a view and a vector's
    /// equality, against a host view made and a driver's call.
    #[test]
    fn a_comparison_is_cheap() {
        let key = key([1, 2, 3, 4], 5);
        let draws = 200_000;
        let began = std::time::Instant::now();
        let mut hits = 0u64;
        for _ in 0..draws {
            if same_with(true, Some(&key), &key) {
                hits += 1;
            }
        }
        let each = began.elapsed().as_nanos() as f64 / draws as f64;
        // Only the count is gated: a runner that pauses mid-loop must not
        // fail the suite over a measurement (CodeRabbit, PR #226).
        assert_eq!(hits, draws);
        println!("a set of four targets compared: {each:.1} ns (one box, no device)");
    }
}
