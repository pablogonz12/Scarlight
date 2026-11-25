//! `v_mul_hi_u32_u24` and `v_mul_hi_i32_i24`: decoded by name but with no lowering,
//! so a program using one refused to translate. The high dword of the 48-bit product
//! of the low 24 bits of each operand (JMAN730/project-flamingo #747).

use super::*;

/// A program computing `op v0, v1, v2` (v1 = 5, v2 = 7) and storing v0 to LDS, which
/// keeps the result live.
fn stores_result(op: u32) -> Result<(), crate::ShaderError> {
    let words = [
        0x7E02_0285,        // v_mov_b32 v1, 5
        0x7E04_0287,        // v_mov_b32 v2, 7
        op << 25 | 2 << 9 | 0x101, // VOP2 `op v0, v1, v2`
        0x7E06_0280,        // v_mov_b32 v3, 0 (the LDS address)
        0xD834_0000,
        0x0000_0003, // ds_write_b32 v3, v0
        S_ENDPGM,
    ];
    lower_with(&words, vec![], vec![]).map(|_| ())
}

/// Both high-dword forms lower; the low-dword forms already did.
#[test]
fn the_24_bit_high_multiplies_lower() {
    for op in [0x0A, 0x0C] {
        stores_result(op).unwrap_or_else(|e| panic!("VOP2 {op:#x}: {e}"));
    }
}
