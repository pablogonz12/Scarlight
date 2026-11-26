//! Scratch memory a list filled with title memory, kept for the list's
//! other draws.
//!
//! A root constant buffer whose title address is not 256-byte aligned cannot
//! be named by the host (D3D12 wants 256 bytes; the console's shaders want
//! 16), so it is copied into scratch memory of ours and the draw names that.
//! Every draw of a list that binds the same root constant buffer asked for
//! scratch memory and a copy of the same bytes again: up to 64 KiB of fill a
//! repeat. The fill is remembered by (source, bytes) until the list next
//! writes title memory, as a refresh is.
//!
//! `VICTORY_SCRATCH_ONCE=0` asks and fills every time, as before.

#[cfg(any(windows, test))]
use std::collections::HashMap;
#[cfg(any(windows, test))]
use std::sync::OnceLock;

/// Whether a root constant buffer's scratch memory is filled once per list
/// and root, from `VICTORY_SCRATCH_ONCE` (`"0"`: every time).
#[cfg(any(windows, test))]
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on_from(std::env::var("VICTORY_SCRATCH_ONCE").ok().as_deref()))
}

/// [`on`] of the value read.
#[cfg(any(windows, test))]
pub(crate) fn on_from(value: Option<&str>) -> bool {
    value != Some("0")
}

/// What a list has filled into scratch memory since it last wrote title
/// memory: (source address, bytes) to the host address of the scratch.
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct Filled(HashMap<(usize, usize), u64>);

#[cfg(any(windows, test))]
impl Filled {
    /// The scratch memory `key` was filled to, if it still holds what the
    /// title memory at `key`'s source has.
    pub(crate) fn known(&self, key: (usize, usize)) -> Option<u64> {
        self.0.get(&key).copied()
    }

    /// `key` was filled to `gpu`: the list's next asks for it are answered
    /// with that address.
    pub(crate) fn note(&mut self, key: (usize, usize), gpu: u64) {
        self.0.insert(key, gpu);
    }

    /// The list wrote title memory: what was filled before that may not be
    /// what it holds now.
    pub(crate) fn forget(&mut self) {
        self.0.clear();
    }

    /// How many fills the list remembers.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VICTORY_SCRATCH_ONCE` is off for nothing but "0".
    #[test]
    fn the_switch_is_off_for_zero_alone() {
        assert!(on_from(None));
        assert!(on_from(Some("1")));
        assert!(!on_from(Some("0")));
    }

    /// A pass draws 10,000 times with three root constant buffers: three
    /// fills, each of 256 bytes, instead of 10,000.
    #[test]
    fn fills_of_a_list_of_draws() {
        const RANGES: [(usize, usize); 3] = [(0x1_0000_0010, 256), (0x1_0000_0110, 256), (0x2_0000_0008, 512)];
        let mut filled = Filled::default();
        let mut scratch = 0x7_f000_0000u64;
        let (mut fills, mut saved) = (0, 0);
        let mut bytes = 0;
        for draw in 0..10_000 {
            let key = RANGES[draw % RANGES.len()];
            if filled.known(key).is_some() {
                saved += 1;
                continue;
            }
            scratch += 0x10000;
            filled.note(key, scratch);
            fills += 1;
            bytes += key.1;
        }
        assert_eq!(fills, RANGES.len(), "one fill a root constant buffer");
        assert_eq!(saved, 10_000 - RANGES.len());
        assert_eq!(bytes, 1_024, "the bytes filled, not 10,000 times as many");
        assert_eq!(filled.len(), RANGES.len());
    }

    /// A copy of title memory (the list wrote it) forgets them: the next
    /// draw fills again, or it would name bytes the title has changed.
    #[test]
    fn a_title_copy_forgets_them() {
        let mut filled = Filled::default();
        filled.note((0x1000, 256), 0x7_f000_0000);
        assert_eq!(filled.known((0x1000, 256)), Some(0x7_f000_0000));
        filled.forget();
        assert_eq!(filled.known((0x1000, 256)), None);
        assert_eq!(filled.len(), 0);
    }

    /// What a fill that is not made costs to make: the bytes copied, at the
    /// two sizes a root constant buffer comes in.
    #[test]
    fn a_fill_is_a_copy() {
        let (src, mut dst) = (vec![7u8; 64 << 10], vec![0u8; 64 << 10]);
        for bytes in [256usize, 64 << 10] {
            let fills = 20_000;
            let began = std::time::Instant::now();
            for _ in 0..fills {
                dst[..bytes].copy_from_slice(&src[..bytes]);
            }
            let each = began.elapsed().as_nanos() as f64 / fills as f64;
            println!("a {bytes}-byte fill copied: {each:.1} ns (one box)");
        }
        assert_eq!(dst[..256], src[..256]);
    }

    /// The same bytes of another buffer are another fill.
    #[test]
    fn another_source_is_another_fill() {
        let mut filled = Filled::default();
        filled.note((0x1000, 256), 1);
        assert_eq!(filled.known((0x1000, 256)), Some(1));
        assert_eq!(filled.known((0x1000, 512)), None, "other bytes");
        assert_eq!(filled.known((0x2000, 256)), None, "other title memory");
    }
}
