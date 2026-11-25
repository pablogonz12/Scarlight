//! Each single's off switch, `VICTORY_*=0`, must reach the path it names: the
//! program below lowers one way with the switch on and main's way with it off.

use super::super::switches::{exclusive, force};
use super::super::{field_at, field_in_dword, host_mods};
use crate::gfx10::Valu;
use super::{S_ENDPGM, lowers_cs};

/// VOP3 `op vdst, src0, src1, src2`, no modifiers (`src0`: 0..105 SGPR,
/// 256..511 VGPR).
const fn vop3(op: u16, vdst: u8, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
    vop3_abs(op, vdst, src0, src1, src2, false)
}

/// [`vop3`] with `src0`'s abs modifier set if `abs`.
const fn vop3_abs(op: u16, vdst: u8, src0: u16, src1: u16, src2: u16, abs: bool) -> [u32; 2] {
    [
        (0x35 << 26) | ((op as u32 & 0x3FF) << 16) | (abs as u32) << 8 | vdst as u32,
        (src0 as u32 & 0x1FF) | ((src1 as u32 & 0x1FF) << 9) | ((src2 as u32 & 0x1FF) << 18),
    ]
}

/// Lower `words` with `switch` forced `on`, as a compute program.
fn lower_forced(words: &[u32], switch: &'static str, on: bool) -> Vec<u8> {
    let _held = exclusive();
    force(switch, Some(on));
    // 512 bytes of LDS for the store that keeps the result alive.
    let lowered = lowers_cs(words, 1 << 15, false, [1, 1, 1]);
    force(switch, None);
    lowered.unwrap_or_else(|e| panic!("{switch}={} refused: {e}", on as u8)).dxil
}

#[test]
fn a_fused_half_multiply_add_rounds_once_only_with_its_switch() {
    // v_fma_f16 v0, v1, v2, v3: one rounding carries the error-free residual and
    // the ties-to-even adjustment (the long form); two round in f32.
    let words: Vec<u32> = vop3(0x34b, 0, 256, 257, 258)
        .into_iter()
        .chain(std::iter::once(S_ENDPGM))
        .collect();
    let once = lower_forced(&words, "VICTORY_F16_FMA_ONCE", true);
    let twice = lower_forced(&words, "VICTORY_F16_FMA_ONCE", false);
    assert_ne!(
        once, twice,
        "VICTORY_F16_FMA_ONCE=0 must lower the f16 fma a different way"
    );
    assert!(
        twice.len() < once.len(),
        "the switch off must be the short FMad: {} vs {}",
        twice.len(),
        once.len()
    );
}

#[test]
fn a_bitwise_modifier_reads_bits_only_with_its_switch() {
    let _held = exclusive();
    let abs = Valu {
        abs: 1,
        ..Default::default()
    };
    let clamp = Valu {
        clamp: true,
        ..Default::default()
    };
    force("VICTORY_BITWISE_MODS", Some(true));
    assert!(!host_mods(&abs, true), "the bitwise path keeps abs on the bits");
    assert!(!host_mods(&clamp, true));
    force("VICTORY_BITWISE_MODS", Some(false));
    assert!(host_mods(&abs, true), "VICTORY_BITWISE_MODS=0 takes fabs");
    assert!(host_mods(&clamp, true));
    assert!(
        !host_mods(&Valu::default(), true),
        "a source with no modifiers has no float path to take"
    );
    force("VICTORY_BITWISE_MODS", None);
}

#[test]
fn a_constant_field_offset_aligns_only_with_its_switch() {
    let _held = exclusive();
    for off in [1u32, 5, 0xFFFE] {
        force("VICTORY_FIELD_DWORD", Some(true));
        assert_eq!(field_at(off), Some(field_in_dword(off)), "offset {off}");
        force("VICTORY_FIELD_DWORD", Some(false));
        assert_eq!(field_at(off), None, "offset {off} off");
        force("VICTORY_FIELD_DWORD", None);
    }
}
