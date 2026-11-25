//! Regression tests of bindless heap reads on a capped host view heap
//! (NVIDIA: 1,000,000 shader-visible views), laid out as the host lays out
//! every view heap (`victory-gpu` `descriptor::host`): the title's slots,
//! then the host's per-draw tables in the dynamic slots, the last 21 slots
//! one typed null each. The bounds are constants of the translated code
//! (`view_bounds`), from the limit a capped host sets.
//!
//! Each case stores a marker through a UAV descriptor read straight from
//! the heap at an index the program computes, as bindless code does, or
//! through a table. The properties asserted: a direct (title) index reaches
//! only its own title slot, and an index at or past the title slots no
//! descriptor of another draw; a table read into the dynamic slots, which
//! is legal, still reaches its slot and stops at the guards; the bounds
//! take no user data and no root-signature space. Without a limit (the
//! translation before the bounds) the same direct reads reach other
//! draws' tables: the negative control.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

const fn sop2(op: u8, sdst: u8, src0: u8, src1: u8) -> u32 {
    0x8000_0000
        | (op as u32 & 0x7F) << 23
        | (sdst as u32 & 0x7F) << 16
        | (src1 as u32) << 8
        | src0 as u32
}

const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25,
    ]
}

const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn buffer_store_dword(vdata: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
    ]
}

const fn sgpr(n: u8) -> u16 {
    n as u16
}

/// The capped heap: title slots below `TITLE`, the host's per-draw tables
/// from `TITLE` up, the last slot the clamp target (and with bounds the last
/// 21 slots one typed null each, `GUARD` the first).
const CAPPED: u32 = 1_000_000;
const TITLE: u32 = 737_856;
const GUARD: u32 = CAPPED - 21;
/// Raw UAVs: the last title slot, then slots that hold other draws' tables
/// (first dynamic, one inside, the last dynamic before the guards).
const PLACED: [u32; 5] = [TITLE - 1, TITLE, TITLE + 1, 934_463, GUARD - 1];

/// A 1-thread program storing `s1` through the UAV descriptor at heap
/// address `s4` + (`s0` << 5): a title index from data (`heap`:
/// `ResourceHeap`), or an entry of the table at `s4` (`Table`).
fn program(heap: RootType) -> HostShader {
    let words = [
        s_mov_b32(5, 0x80),
        sop2(0x1E, 6, 0, 0x85),
        s_load_dwordx4(8, 4)[0],
        6 << 25,
        vop1(0x001, 0, sgpr(1)),
        buffer_store_dword(0, 8, 0)[0],
        buffer_store_dword(0, 8, 0)[1],
        S_ENDPGM,
    ];
    lower_words(&words, heap, 0)
}

/// VOP2 `v_add_nc_u32 vdst, src0, vsrc1` (GFX10 opcode 0x25).
const fn v_add_nc_u32(vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    0x25 << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | src0 as u32
}

/// A program that reads `extra` user SGPRs past the view at `s4` (`s5` on,
/// the most a stage has being `s31`) and stores their sum through that
/// view: a table at `s4`, or the heap read straight at index `s1`.
fn reads_program(heap: RootType, extra: u8) -> HostShader {
    let mut words = vec![vop1(0x001, 0, sgpr(0))];
    for n in 0..extra {
        words.push(v_add_nc_u32(0, sgpr(5 + n), 0));
    }
    words.push(s_mov_b32(5, 0x80));
    if heap == RootType::ResourceHeap {
        words.push(sop2(0x1E, 6, 1, 0x85));
        words.extend_from_slice(&[s_load_dwordx4(8, 4)[0], 6 << 25]);
    } else {
        words.extend_from_slice(&s_load_dwordx4(8, 4));
    }
    words.extend_from_slice(&buffer_store_dword(0, 8, 0));
    words.push(S_ENDPGM);
    lower_words(&words, heap, extra)
}

/// Lowers `words` as a 1-thread compute program: 4 root constants in
/// `s0..s3`, the view at `s4` (`heap`: a table or the heap's base), then
/// `extra` more root constants from `s5` on.
fn lower_words(words: &[u32], heap: RootType, extra: u8) -> HostShader {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: heap,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ]
        .into_iter()
        .chain((extra > 0).then_some(RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: u32::from(extra),
            base_register: 1,
            user_data: 5,
            slot: 2,
        }))
        .collect(),
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: heap,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: u32::from(5 + extra) << 1,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// `heap`'s program translated with the limit a host capped at
/// [`CAPPED`] views sets (`set_view_heap_limit(views - 1)`; 0: none).
fn lowered(heap: RootType, limit: u32) -> HostShader {
    super::shader::with_view_limit(limit, || program(heap))
}

/// Per device: for each (index, table base), which placed UAV got the
/// case's marker (`None`: no placed UAV, the read went to a null slot),
/// and the debug-layer messages about a descriptor's kind.
fn run(heap: RootType, cases: &[(u32, u32)]) -> Vec<(&'static str, Vec<Option<u32>>, usize)> {
    run_with(heap, CAPPED - 1, cases)
}

/// [`run`] with the program translated with heap limit `limit`.
fn run_with(
    heap: RootType,
    limit: u32,
    cases: &[(u32, u32)],
) -> Vec<(&'static str, Vec<Option<u32>>, usize)> {
    let shader = lowered(heap, limit);
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let _ = host.debug_messages();
        let mut reads = Vec::new();
        for (i, &(index, base)) in cases.iter().enumerate() {
            let marker = 0x7000 + i as u32;
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot, u.dword) {
                    (RootType::Constants, 0, 0) => index,
                    (RootType::Constants, 0, 1) => marker,
                    (RootType::Constants, 0, _) => 0,
                    (RootType::Table, 1, _) => base * 32,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let views: Vec<View> = PLACED.iter().map(|_| View::RawUav(1)).collect();
            let got = host
                .dispatch_placed(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                    Some((CAPPED, &PLACED)),
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            let hit: Vec<u32> = (0..PLACED.len())
                .filter(|&j| got[j][0] == marker)
                .map(|j| PLACED[j])
                .collect();
            assert!(
                hit.len() <= 1,
                "{label}: index {index}: written through {hit:?}"
            );
            reads.push(hit.first().copied());
        }
        let kind = host
            .debug_messages()
            .iter()
            .filter(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("descriptor") && m.contains("match")
            })
            .count();
        out.push((label, reads, kind));
    }
    assert!(!out.is_empty(), "no SM 6.6 device");
    eprintln!(
        "{} lowering ({} user-data dwords, {} DXIL bytes)",
        if limit != 0 { "bounded" } else { "unbounded" },
        shader.user_data_dwords(),
        shader.dxil.len()
    );
    out
}

/// Indices just below, at and past the title heap's end, inside other
/// draws' tables, at the guards, the clamp slot, the heap's end and far
/// past it: only the last title slot may be written, and only its own.
#[test]
fn a_direct_index_reaches_its_title_slot_and_no_other_draws_table() {
    let indices = [
        TITLE - 1,
        TITLE,
        TITLE + 1,
        934_463,
        GUARD - 1,
        GUARD,
        CAPPED - 1,
        CAPPED,
        1 << 25,
        0x07FF_FFFF,
    ];
    let cases: Vec<(u32, u32)> = indices.iter().map(|&i| (i, 0)).collect();
    for (label, reads, _) in run(RootType::ResourceHeap, &cases) {
        for (&index, read) in indices.iter().zip(&reads) {
            eprintln!("{label}: direct index {index}: {read:?}");
        }
        for (&index, read) in indices.iter().zip(&reads) {
            let want = (index == TITLE - 1).then_some(TITLE - 1);
            assert_eq!(
                *read, want,
                "{label}: direct index {index} reached {read:?}"
            );
        }
    }
}

/// A table the host made in the dynamic slots is read through: its entries
/// there must still be reached (the bound must not cut legal reads).
#[test]
fn a_table_in_the_dynamic_slots_is_still_read() {
    // A table at the first dynamic slot: entries up to the last dynamic
    // slot are its own; at the guards and past the heap, nothing.
    let cases = [
        (0, TITLE),
        (934_463 - TITLE, TITLE),
        (GUARD - 1 - TITLE, TITLE),
        (GUARD - TITLE, TITLE),
        (CAPPED - TITLE, TITLE),
        (0, TITLE - 1),
    ];
    let want = vec![
        Some(TITLE),
        Some(934_463),
        Some(GUARD - 1),
        None,
        None,
        Some(TITLE - 1),
    ];
    for (label, reads, _) in run(RootType::Table, &cases) {
        assert_eq!(reads, want, "{label}");
    }
}

/// GPU-based validation (`VICTORY_D3D12_GBV=1`; else skipped): an index
/// past the bounds reads a null descriptor of the kind the shader reads.
#[test]
fn an_index_out_of_bounds_reads_a_null_of_its_own_kind() {
    if std::env::var_os("VICTORY_D3D12_GBV").is_none() {
        eprintln!("VICTORY_D3D12_GBV unset; skipped");
        return;
    }
    for (label, _, kind) in run(RootType::ResourceHeap, &[(CAPPED, 0), (1 << 25, 0)]) {
        eprintln!("{label}: {kind} validation messages about a descriptor's kind");
        assert_eq!(
            kind, 0,
            "{label}: a null descriptor of another kind than the read was read"
        );
    }
}

/// The negative control: translated without a heap limit (no bounds, as
/// before them), a direct read at a title index past the title slots
/// reaches whatever table another draw has there. Only indices inside the
/// heap are run unbounded.
#[test]
fn without_the_bounds_direct_reads_reach_other_draws_tables() {
    let cases: Vec<(u32, u32)> = PLACED.iter().map(|&i| (i, 0)).collect();
    for (label, reads, _) in run_with(RootType::ResourceHeap, 0, &cases) {
        let want: Vec<Option<u32>> = PLACED.iter().map(|&i| Some(i)).collect();
        assert_eq!(reads, want, "{label}");
    }
}

/// The bounds take no user data and no root-signature space: a stage that
/// reads none to all of its 32 user SGPRs and the heap has the very user
/// data and root constants it has translated without a limit (no bounds),
/// 4 to 32 dwords. So a pipeline that fits the host's 64 root dwords
/// without the bounds fits with them (the host's root signature is made
/// from these alone, `victory-gpu` `pipeline::host`).
#[test]
fn the_bounds_take_no_user_data_or_root_dwords() {
    for heap in [RootType::ResourceHeap, RootType::Table] {
        for extra in 0..=27u8 {
            let bounded = super::shader::with_view_limit(CAPPED - 1, || reads_program(heap, extra));
            let plain = super::shader::with_view_limit(0, || reads_program(heap, extra));
            assert_eq!(bounded.user_data, plain.user_data, "{heap:?}, {extra} more");
            assert_eq!(
                bounded.user_data_dwords(),
                plain.user_data_dwords(),
                "{heap:?}, {extra} more"
            );
            // Read: s0 and s1 (the heap's base is no dword), or s0 and the
            // table at s4; then s5 on.
            let last = if extra > 0 {
                5 + u32::from(extra)
            } else if heap == RootType::Table {
                5
            } else {
                2
            };
            assert_eq!(
                bounded.user_data_dwords(),
                last.next_multiple_of(4),
                "{heap:?}, {extra} more"
            );
        }
    }
}
