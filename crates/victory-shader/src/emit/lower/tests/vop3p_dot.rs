//! VOP3P `0x18` is `v_dot8_i32_i4` (RDNA2 ISA, between `v_dot4_u32_u8` and
//! `v_dot8_u32_u4`; LLVM's gfx1030 disassembler names it so). The opcode table
//! skipped it, so the decoder gave it no name and the program was refused as an
//! unnamed opcode (project-flamingo #737). Neither nibble dot product is
//! lowered, so a program using one is still refused — now by name, which is what
//! a "shader does not translate" line needs to say.

use super::*;

/// A program computing `op v0, v1, v2, v3` (v0..v3 = 3, 5, 7, 0) and storing
/// v0 to LDS, which keeps the result live.
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

/// The signed nibble dot product (VOP3P `0x18`) is decoded by name and refused
/// by that name: the failure says which instruction was not lowered.
#[test]
fn the_signed_nibble_dot_is_refused_by_name() {
    let err = stores_result([0xCC18_4000, 0x1C0E_0501]).expect_err("v_dot8_i32_i4 is not lowered");
    let text = format!("{err:?}");
    assert!(text.contains("v_dot8_i32_i4"), "{text}");
}

/// The unsigned one beside it (`0x19`) behaves the same way, which pins that the
/// table entry above is what changed and not the refusal path.
#[test]
fn the_unsigned_nibble_dot_is_refused_by_name() {
    let err = stores_result([0xCC19_4000, 0x1C0E_0501]).expect_err("v_dot8_u32_u4 is not lowered");
    let text = format!("{err:?}");
    assert!(text.contains("v_dot8_u32_u4"), "{text}");
}
