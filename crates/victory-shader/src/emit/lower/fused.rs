//! The fused 16-bit multiply-add: one rounding, as the console rounds it.

use super::Lower;
use crate::emit::bitcode::{Bin, V, pred};

impl<'a> Lower<'a> {
    /// Both binary16 factors have an exact product in binary32, so the fused
    /// sum's error is only the final rounding: an error-free residual of the
    /// sum rounds the inexact binary32 result to odd (Boldo and Melquiond,
    /// 2008). The caller must convert it to binary16 with nearest-even rounding.
    pub(super) fn fmad_half(&mut self, a: V, b: V, addend: V) -> V {
        let c = self.cur;
        let product = self.s.m.bin(c, Bin::Mul, a, b);
        let sum = self.fmad(a, b, addend);
        let rounded_addend = self.s.m.bin(c, Bin::Sub, sum, product);
        let rounded_product = self.s.m.bin(c, Bin::Sub, sum, rounded_addend);
        let lost_product = self.s.m.bin(c, Bin::Sub, product, rounded_product);
        let lost_addend = self.s.m.bin(c, Bin::Sub, addend, rounded_addend);
        let residual = self.s.m.bin(c, Bin::Add, lost_product, lost_addend);
        // The sum is inexact exactly where the residual is a non-zero number:
        // of NaNs and infinities it is NaN, and an ordered compare is false.
        let zero_f = self.s.m.f32(0);
        let above = self.s.m.cmp(c, pred::OGT, residual, zero_f);
        let below = self.s.m.cmp(c, pred::OLT, residual, zero_f);
        let inexact = self.s.m.bin(c, Bin::Or, above, below);
        let bits = self.as_int(sum);
        let residual_bits = self.as_int(residual);
        let one = self.s.m.i32(1);
        let minus_one = self.s.m.i32(u32::MAX);
        let zero = self.s.m.i32(0);
        let low_bit = self.s.m.bin(c, Bin::And, bits, one);
        let even = self.s.m.cmp(c, pred::EQ, low_bit, zero);
        let adjust = self.s.m.bin(c, Bin::And, even, inexact);
        // Ties to even: odd bits with a residual of the sum's sign go away
        // from zero, the others toward it.
        let signs = self.s.m.bin(c, Bin::Xor, bits, residual_bits);
        let toward_zero = self.s.m.cmp(c, pred::SLT, signs, zero);
        let delta = self.s.m.select(c, toward_zero, minus_one, one);
        let odd = self.s.m.bin(c, Bin::Add, bits, delta);
        let rounded = self.s.m.select(c, adjust, odd, bits);
        self.as_float(rounded)
    }

}
