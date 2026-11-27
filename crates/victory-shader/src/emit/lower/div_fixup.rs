//! Lane bit extraction, kept out of `lower.rs` for the file-size limit
//! (fxceless/victory-fork #1040). `v_div_fixup`'s special cases stay in
//! `Lower::div_fixup` in `lower.rs`.

use super::*;

impl<'a> Lower<'a> {
    pub(super) fn lane_bit_at(&mut self, at: usize, lane: V, lo: V, hi: Option<V>) -> V {
    let (k31, k32, one, z) = (
        self.s.m.i32(31),
        self.s.m.i32(32),
        self.s.m.i32(1),
        self.s.m.i32(0),
    );
    let word = match hi {
        Some(hi) => {
            let low = self.s.m.cmp(at, pred::IULT, lane, k32);
            self.s.m.select(at, low, lo, hi)
        }
        None => lo,
    };
    let sh = self.s.m.bin(at, Bin::And, lane, k31);
    let t = self.s.m.bin(at, Bin::LShr, word, sh);
    let t = self.s.m.bin(at, Bin::And, t, one);
    self.s.m.cmp(at, pred::NE, t, z)
}
}
