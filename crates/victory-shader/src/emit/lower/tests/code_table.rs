//! The constant table a program's code carries starts at the lowest byte its
//! loads read, and a load through a descriptor built around `s_getpc_b64`
//! reads its base plus its own offset, which can lie below the base.

use super::*;

/// A program whose instructions end at 0xa0: `s_getpc_b64` builds a
/// descriptor whose base is 0x80, and a buffer load through it reads at
/// instruction offset -64, byte 0x40.
fn built_descriptor_load() -> Vec<u32> {
    let mut words = vec![
        0xBE88_1F00, // 0x00: s_getpc_b64 s[8:9]
        0x8008_FF08,
        0x0000_007C, // s_add_u32 s8, s8, 0x7c (the base is 0x80)
        0x8209_8009, // s_addc_u32 s9, s9, 0
        0xBE8A_0380, // s_mov_b32 s10, 0
        0xBE8B_0380, // s_mov_b32 s11, 0
        0xF420_0304,
        0xFA1F_FFC0, // 0x18: s_buffer_load_dword s12, s[8:11], -64
        0x7E02_030C, // 0x20: v_mov_b32 v1, s12
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
    ];
    // 0x2c to 0x9c: padding, so the code runs past the descriptor's base.
    words.extend(std::iter::repeat_n(0xBF80_0000, 28));
    words.push(S_ENDPGM);
    words
}

#[test]
fn the_code_table_starts_at_the_lowest_byte_a_load_reads() {
    let words = built_descriptor_load();
    assert_eq!(words.len() * 4, 0xa0);
    let l = lower_with(&words, vec![], vec![]).unwrap();
    let (first, dwords) = l.code_table.expect("a code table");
    // The descriptor's base (0x80) sits above the byte its load reads
    // (0x40), so the table starts there; at 0x80 the load read the table's
    // last dword instead (the index rebased below it and clamped).
    assert_eq!(first, 0x40, "the table starts at the load's byte");
    assert_eq!(dwords, words[0x10..], "the table holds the code from 0x40 on");
}
