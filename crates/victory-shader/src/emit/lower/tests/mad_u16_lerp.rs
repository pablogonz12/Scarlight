//! `v_mad_u32_u16`, `v_mad_i32_i16` and `v_lerp_u8`: decoded by name but with no lowering,
//! so a program using one refused to translate (#405).

use super::*;

/// VOP3 `op vdst, src0, src1, src2` with `op_sel` (bits 0/1 pick the high halves of
/// src0/src1; the result's half selection is bit 3 and is not used here).
fn vop3_opsel(op: u16, vdst: u8, src0: u32, src1: u32, src2: u32, opsel: u8) -> [u32; 2] {
    [
        0x35 << 26 | (op as u32 & 0x3FF) << 16 | (opsel as u32 & 0xF) << 11 | vdst as u32,
        (src0 & 0x1FF) | (src1 & 0x1FF) << 9 | (src2 & 0x1FF) << 18,
    ]
}

/// A program computing `op v0, v1, v2, v3` (v0..v3 = 3, 5, 7, 0) and storing v0 to LDS,
/// which keeps the result live.
fn stores_result(op_words: [u32; 2]) -> Result<(), crate::ShaderError> {
    let mut words = vec![
        0x7E00_0283, // v_mov_b32 v0, 3
        0x7E02_0285, // v_mov_b32 v1, 5
        0x7E04_0287, // v_mov_b32 v2, 7
    ];
    words.extend_from_slice(&op_words);
    words.extend_from_slice(&[
        0x7E06_0280, // v_mov_b32 v3, 0 (the LDS address)
        0xD834_0003,
        0x0000_0000, // ds_write_b32 v3, v0
        S_ENDPGM,
    ]);
    lower_with(&words, vec![], vec![]).map(|_| ())
}

/// the 16×16→32 multiply-adds, with either half of src0/src1.
#[test]
fn mad_32_by_16_lowers() {
    for opsel in [0, 0b0011] {
        stores_result(vop3_opsel(0x373, 0, 256, 257, 258, opsel)).expect("v_mad_u32_u16");
        stores_result(vop3_opsel(0x375, 0, 256, 257, 258, opsel)).expect("v_mad_i32_i16");
    }
}

/// The packed 4×8-bit average.
#[test]
fn lerp_u8_lowers() {
    stores_result(vop3_opsel(0x14D, 0, 256, 257, 258, 0)).expect("v_lerp_u8");
}

/// An output modifier is still refused: the lowered form has no saturation, and a
/// silent `VICTORY_...` no-op would store a value the console does not.
#[test]
fn a_clamped_mad_32_by_16_is_still_refused() {
    let mut words = vop3_opsel(0x373, 0, 256, 257, 258, 0);
    words[0] |= 1 << 15; // clamp
    assert!(stores_result(words).is_err(), "a clamped v_mad_u32_u16");
}
