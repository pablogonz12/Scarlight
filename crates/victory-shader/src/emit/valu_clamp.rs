//! VOP3 integer `clamp` on add and subtract: unsigned saturation, from the
//! carry bit (add `→ 0xFFFF_FFFF`) or the borrow bit (sub `→ 0`). VOP3's
//! `clamp` on an integer operation is not a float clamp; the title uses it
//! for saturating index and bound arithmetic (`VIC-BT-010`).

use super::super::bitcode::V;
use super::Lower;

impl<'a> Lower<'a> {
    /// VOP3 integer ops whose `clamp` means unsigned saturation via the
    /// carry (add) or borrow (sub) bit.
    pub(super) fn integer_clamp_op(name: &str) -> bool {
        matches!(
            name,
            "v_add_co_u32"
                | "v_sub_co_u32"
                | "v_subrev_co_u32"
                | "v_add_co_ci_u32"
                | "v_sub_co_ci_u32"
                | "v_subrev_co_ci_u32"
                | "v_add_nc_u32"
                | "v_sub_nc_u32"
                | "v_subrev_nc_u32"
        )
    }

    /// The saturated result: the all-ones value where an add carried (the
    /// sum wrapped) and zero where a sub borrowed (the difference went below
    /// zero), the plain result elsewhere.
    pub(super) fn clamp_u32(&mut self, c: usize, add: bool, r: V, carry: V) -> V {
        let sat = self.s.m.i32(if add { u32::MAX } else { 0 });
        self.s.m.select(c, carry, sat, r)
    }
}

#[cfg(test)]
mod tests {
    use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
    use crate::{io, ir, resource, structure};

    use super::super::{Parts, lower};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// Lowers a synthetic compute program with `rsrc2` (an LDS size).
    fn lowers(words: &[u32], rsrc2: u32) -> Result<(), crate::ShaderError> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2,
                threads: [1, 1, 1],
            }),
            code,
        };
        let p = ir::build(&native.code, false, 256).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native).unwrap();
        lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        })
        .map(|_| ())
    }

    /// `v1` and `v2` as source operands.
    const V1: u32 = 256 + 1;
    const V2: u32 = 256 + 2;

    /// VOP3 integer `clamp` on carry and non-carry add/sub lowers (before
    /// this it was refused as an integer output modifier, `VIC-BT-010`). The
    /// LDS store keeps the result live.
    #[test]
    fn integer_vop3_clamp_lowers_add_and_sub() {
        // A 16-byte LDS allocation for the store.
        let rsrc2 = 1 << 15 | 16;
        let store = |op_words: [u32; 2]| {
            let mut words = vec![
                0x7E02_0281, // v_mov_b32 v1, 1
                0x7E04_0282, // v_mov_b32 v2, 2
            ];
            words.extend_from_slice(&op_words);
            words.extend_from_slice(&[
                0x7E06_0280, // v_mov_b32 v3, 0
                0xD834_0000,
                0x0000_0003, // ds_write_b32 v3, v0
                S_ENDPGM,
            ]);
            lowers(&words, rsrc2).unwrap();
        };
        // VOP2 ops in the VOP3 encoding take opcode + 0x100 (bare 0x025 is
        // the VOPC `v_cmp_lg_f64`).
        for op in [0x125u16, 0x126, 0x127] {
            store([0x35 << 26 | (op as u32) << 16 | 1 << 15, V1 | V2 << 9]);
        }
        // v_add_co_u32 / v_sub_co_u32 / v_subrev_co_u32 with clamp (carry out
        // to s16, unused).
        for op in [0x30fu16, 0x310, 0x319] {
            store([
                0x35 << 26 | (op as u32) << 16 | 1 << 15 | 16 << 8,
                V1 | V2 << 9,
            ]);
        }
    }
}
