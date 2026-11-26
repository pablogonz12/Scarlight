//! An immediate sampler constant (the sampler's words are constants in the program, not a table
//! entry) used both for normal and for comparison sampling: the host's static sampler carries the
//! comparison kind, so the two uses take two slots even though their words are equal
//! (HUMAN a7bb3e8, bdycounter-sudo, GPL-3.0-or-later).

use super::*;

/// The words of a program that samples with the same all-zero sampler constant twice: once at a
/// level and once with a comparison.
fn same_constant_sample_and_compare() -> [u32; 17] {
    [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0202,
        0xFA00_0000, // s_load_dwordx8 s[8:15], s[4:5], null
        0xBE90_0380, // s_mov_b32 s16, 0
        0xBE91_0380, // s_mov_b32 s17, 0
        0xBE92_0380, // s_mov_b32 s18, 0
        0xBE93_0380, // s_mov_b32 s19, 0
        0xF09C_0F08,
        0x0082_0402, // image_sample_lz v[4:7], v[2:3], s[8:15], s[16:19]
        0xF0BC_0108,
        0x0082_0801, // image_sample_c_lz v8, v[1:3], s[8:15], s[16:19] (dmask = 0x1)
        0xD834_0000,
        0x0000_0400, // ds_write_b32 v0, v4
        0xD834_0000,
        0x0000_0800, // ds_write_b32 v0, v8
        S_ENDPGM,
    ]
}

#[test]
fn a_sampler_constant_used_with_and_without_comparison_gets_two_slots() {
    let words = same_constant_sample_and_compare();
    let out = lower_with(
        &words,
        vec![root(RootType::Table, 0, 0)],
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        }],
    )
    .expect("a sampler used for both kinds of sampling");
    // Two static samplers of the same words, the second the comparison one.
    assert_eq!(out.samplers.len(), 2);
    assert_eq!(out.samplers[0], out.samplers[1]);
    assert_eq!(out.cmp_samplers, vec![false, true]);
    // And the program is still valid DXIL.
    assert!(crate::Container::parse(&out.dxil).is_ok());
}

#[test]
fn one_constant_used_one_way_keeps_its_one_slot() {
    // The same program without the comparison sampling: a single slot, no
    // comparison flag.
    let all = same_constant_sample_and_compare();
    let words: Vec<u32> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 10 | 11))
        .map(|(_, w)| *w)
        .collect();
    let out = lower_with(
        &words,
        vec![root(RootType::Table, 0, 0)],
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        }],
    )
    .expect("a sampler used for level sampling");
    assert_eq!(out.samplers.len(), 1);
    assert_eq!(out.cmp_samplers, vec![false]);
}
