//! A pure scalar EXEC widening (`s_or_b32`/`s_xor_b32` with the saved mask, say) sits
//! outside `Structured::active`, so reading the widened mask as a number must
//! reconstruct it from its operands' ballots instead of refusing.

use super::*;

#[test]
fn widened_exec_read_as_data_reconstructs_scalar_ops() {
    for widen in [
        0x887E_007E, // s_or_b32 exec_lo, exec_lo, s0
        0x897E_007E, // s_xor_b32 exec_lo, exec_lo, s0
    ] {
        let words = [
            0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
            0xBE80_3C6A, // s_and_saveexec_b32 s0, vcc_lo
            0xBF88_0005, // s_cbranch_execz -> end
            widen,       // widen EXEC past Block 1's reachability
            0xBE82_0F7E, // s_bcnt1_i32_b32 s2, exec_lo
            0xBF07_8002, // s_cmp_lg_u32 s2, 0
            0xBF85_0001, // s_cbranch_scc1 -> end
            0xBE84_0380, // s_mov_b32 s4, 0
            S_ENDPGM,
        ];
        lowers(&words, 0)
            .unwrap_or_else(|e| panic!("widened EXEC ({widen:#x}) failed to lower: {e}"));
    }
}
