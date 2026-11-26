//! `v_div_fixup`'s special cases, kept out of `lower.rs` for the file-size
//! limit. See `Lower::div_fixup` (fxceless/victory-fork #1040).

use super::*;

impl<'a> Lower<'a> {
/// `v_div_fixup`'s special cases on f32 values: a NaN operand, 0/0 and
/// inf/inf give NaN, x/0 and inf/x a signed infinity, 0/x and x/inf a signed
/// zero, an all-ones quotient exponent a signed infinity; else `|q|` with
/// the quotient's sign (the ISA's order, f16 bits being exact in f32).
pub(super) fn div_fixup(&mut self, q: V, den: V, num: V) -> V {
    let c = self.cur;
    let (qi, di, ni) = (self.as_int(q), self.as_int(den), self.as_int(num));
    let k = |l: &mut Self, x: u32| l.s.m.i32(x);
    let (ksign, kabs, kinf, kqnan, k0) = (
        k(self, 0x8000_0000),
        k(self, 0x7FFF_FFFF),
        k(self, 0x7F80_0000),
        k(self, 0x0040_0000),
        k(self, 0),
    );
    let x = self.s.m.bin(c, Bin::Xor, ni, di);
    let sign = self.s.m.bin(c, Bin::And, x, ksign);
    let q_abs = self.s.m.bin(c, Bin::And, qi, kabs);
    let d_abs = self.s.m.bin(c, Bin::And, di, kabs);
    let n_abs = self.s.m.bin(c, Bin::And, ni, kabs);
    let d_nan = self.s.m.cmp(c, pred::IUGT, d_abs, kinf);
    let n_nan = self.s.m.cmp(c, pred::IUGT, n_abs, kinf);
    let d_inf = self.s.m.cmp(c, pred::EQ, d_abs, kinf);
    let n_inf = self.s.m.cmp(c, pred::EQ, n_abs, kinf);
    let d_zero = self.s.m.cmp(c, pred::EQ, d_abs, k0);
    let n_zero = self.s.m.cmp(c, pred::EQ, n_abs, k0);
    let both_zero = self.s.m.bin(c, Bin::And, d_zero, n_zero);
    let both_inf = self.s.m.bin(c, Bin::And, d_inf, n_inf);
    let invalid = self.s.m.bin(c, Bin::Or, both_zero, both_inf);
    let to_inf = self.s.m.bin(c, Bin::Or, d_zero, n_inf);
    let to_zero = self.s.m.bin(c, Bin::Or, d_inf, n_zero);
    // From the last check to the first, so the first that holds wins.
    let r = self.s.m.bin(c, Bin::Or, sign, q_abs);
    let signed_inf = self.s.m.bin(c, Bin::Or, sign, kinf);
    let q_overflow = self.s.m.cmp(c, pred::IUGE, q_abs, kinf);
    let r = self.s.m.select(c, q_overflow, signed_inf, r);
    let r = self.s.m.select(c, to_zero, sign, r);
    let r = self.s.m.select(c, to_inf, signed_inf, r);
    let default_nan = k(self, 0x7FC0_0000);
    let r = self.s.m.select(c, invalid, default_nan, r);
    let d_q = self.s.m.bin(c, Bin::Or, di, kqnan);
    let r = self.s.m.select(c, d_nan, d_q, r);
    let n_q = self.s.m.bin(c, Bin::Or, ni, kqnan);
    let r = self.s.m.select(c, n_nan, n_q, r);
    self.as_float(r)
}
}
