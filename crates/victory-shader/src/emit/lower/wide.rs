//! One call for the dwords of one console load (`bitcode::opt::WIDE` of
//! `VICTORY_SHADER_OPT`), not one a dword.
//!
//! The console's `s_buffer_load_dwordx2` to `x16` from an SRV reads the
//! dwords of one instruction at addresses `addr + 4`, `+ 8`, ... without
//! wrapping at 32 bits; the lowering reads them a `rawBufferLoad` a dword.
//! Here the same dwords come from one call an instruction for every four of
//! them, the last of a load masked to those that are in it. The bytes read
//! are the same, so the translation is exact either way; the switch is for
//! measuring what the driver's joining of the loads is worth.
//!
//! Loads of different console instructions are never joined: the program
//! computes their offsets in 32-bit registers, so `x - 4` and `x` at `x = 0`
//! are one dword before the view and its first one, where one call from
//! `x - 4` on would read both out of it.

use super::{Attr, Bin, Lower, V};

/// Whether the dwords of one console load are read one call for every four
/// of them. Not on unless `VICTORY_SHADER_OPT` asks for the bit; a test may
/// force either form ([`FORCE`]).
pub(super) fn loads() -> bool {
    #[cfg(test)]
    if let Some(wide) = FORCE.with(|f| *f.borrow()) {
        return wide;
    }
    super::super::shader_opt() & super::super::bitcode::opt::WIDE != 0
}

#[cfg(test)]
thread_local! {
    /// `Some(true)` or `Some(false)`: the form a test asks for, whatever
    /// the process's switch says. Thread-local: tests run in parallel and
    /// the switch itself is read once.
    pub(super) static FORCE: std::cell::RefCell<Option<bool>> = const { std::cell::RefCell::new(None) };
}

impl<'a> Lower<'a> {
    /// `count` dwords from byte address `addr` on of the raw view `h`, as
    /// integers, read by one call for every four of them ([`loads`]).
    pub(super) fn raw_dwords(&mut self, h: V, addr: V, count: u32) -> Vec<V> {
        let c = self.cur;
        let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let mut out = Vec::with_capacity(count as usize);
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4);
            let off = self.s.m.i32(4 * first);
            let a = self.s.m.bin(c, Bin::Add, addr, off);
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(c, "rawBufferLoad.f32", Attr::ReadOnly, self.s.t.res_ret, 139, &[h, a, u, mask, al]);
            for k in 0..n {
                let x = self.s.m.extract(c, r, k);
                out.push(self.as_int(x));
            }
        }
        out
    }
}
