//! A table descriptor that a program reads only as numbers (a size, a tile width) takes
//! its raw view's values instead of being refused.

use super::*;

/// A table at s0 of `dwords`-dword `Srv` entries `stride` bytes apart.
fn table_range(dwords: u8, stride: u32) -> (Vec<RootParam>, Vec<UsedResource>) {
    (
        vec![root(RootType::Table, 0, 0)],
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride,
            dwords,
            user_data: 0,
            slot: 0,
        }],
    )
}

#[test]
fn a_buffer_descriptor_read_only_as_numbers_takes_the_record_count() {
    // `s_load_dwordx4 s[8:11], s[4:5], null` fetches a table entry no
    // resource instruction uses (a program reads a size to bound a
    // loop): dword 2 is the host view's record count, the address and
    // the format word have no host value and read 0.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
        0x8F81_0A08, // s_lshl_b32 s8, s10, 1
        0x7E02_0308, // v_mov_b32 v1, s8
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let (layout, used) = table_range(4, 4);
    lower_with(&words, layout, used).unwrap();
}

#[test]
fn an_image_descriptor_read_only_as_numbers_takes_the_dimensions() {
    // An 8 dword entry: the width is in dword 1 bits 30-31 and dword 2,
    // as code that computes its own tile size from a view reads it.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0202,
        0xFA00_0000, // s_load_dwordx8 s[8:15], s[4:5], null
        0x9008_9E09, // s_lshr_b32 s8, s9, 30
        0x8F82_0A09, // s_lshl_b32 s9, s10, 2
        0x8808_0908, // s_or_b32 s8, s8, s9
        0x7E02_0308, // v_mov_b32 v1, s8
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let (layout, used) = table_range(8, 0);
    lower_with(&words, layout, used).unwrap();
}
