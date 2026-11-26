//! The wide reads of a console buffer load (`bitcode::opt::WIDE` of
//! `VICTORY_SHADER_OPT`): a burst of `s_buffer_load_dword*` from a raw SRV
//! lowered both ways, the calls counted, and — where a host is there — the
//! dwords they read compared with what the narrow form reads, across the
//! view's end too.

use super::lower::{Parts, wide_for_test};
use super::{HostShader, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// The three loads: (first register, dwords), in the order they run. Their
/// dwords come to 28, read one a call or one for every four: 7.
const LOADS: [(u32, u32); 3] = [(16, 8), (24, 4), (32, 16)];

/// The program: `s_buffer_load_dwordx8`, `x4` and `x16` from a raw SRV, then
/// each dword moved to `v1` and stored to a raw UAV, so nothing is dropped.
fn program() -> Vec<u32> {
    let mut words: Vec<u32> = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0 (the SRV)
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20 (the UAV)
        0xF42C_0404,
        0x0000_0000, // s_buffer_load_dwordx8 s[16:23], s[8:11], s0
        0xF428_0604,
        0xFA00_0008, // s_buffer_load_dwordx4 s[24:27], s[8:11], 0x8
        0xF430_0804,
        0x0200_0004, // s_buffer_load_dwordx16 s[32:47], s[8:11], s1, 0x4
    ];
    let mut out_dword = 0u32;
    for (first, n) in LOADS {
        for k in 0..n {
            // v_mov_b32 v1, s<first + k>
            words.push(0x7E00_0000 | 1 << 17 | 0x001 << 9 | (first + k));
            // buffer_store_dword v1, s[12:15], offset 4 * out_dword
            words.push(0x38 << 26 | 0x1C << 18 | (4 * out_dword));
            words.push(0x80 << 24 | (12 >> 2) << 16 | 1 << 8);
            out_dword += 1;
        }
    }
    words.push(0xBF81_0000); // s_endpgm
    words
}

fn native(words: &[u32]) -> NativeProgram {
    let table = |kind, at| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
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
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![table(ResourceType::Srv, 0), table(ResourceType::Uav, 8)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Lowers the program with the wide reads on or off; the DXIL is validated
/// when the validator is there. Returns the shader and its buffer loads.
fn build(wide: bool) -> (HostShader, u32) {
    wide_for_test(Some(wide));
    let native = native(&program());
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let l = lower::lower(&Parts {
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
    wide_for_test(None);
    #[cfg(windows)]
    if let Ok(mut v) = super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        let mut dxil = l.dxil.clone();
        v.validate(&mut dxil).expect("DXIL validation failed");
    }
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        root_indexed_views: res.root_indexed_views.clone(),
    };
    (shader, l.buffer_loads)
}

/// The dwords of the view the three loads read from byte starts `starts`:
/// the console's load reads `count` dwords from its start on, 0 for those
/// out of the view (12 dwords here, so the loads run across its end).
fn want_dwords(data: &[u32], starts: [u64; 3]) -> Vec<u32> {
    let mut want = Vec::new();
    for (&(_, n), start) in LOADS.iter().zip(starts) {
        for k in 0..u64::from(n) {
            let byte = start + 4 * k;
            want.push(if byte + 4 <= 4 * data.len() as u64 { data[(byte / 4) as usize] } else { 0 });
        }
    }
    want
}

/// The starts of the three loads for a given `s0` and `s1`: the x8 from `s0`,
/// the x4 from the instruction's 0x8, the x16 from `s1` plus its 0x4.
fn starts_of(s0: u32, s1: u32) -> [u64; 3] {
    [u64::from(s0), 8, u64::from(s1) + 4]
}

/// The cases: `s0` and `s1` of the three loads, inside the view and across
/// its end (the first load's from 40 on is past it; the x16's from 44 on is
/// 12 dwords past it).
const CASES: [(u32, u32); 6] = [(0, 0), (16, 0), (24, 4), (40, 28), (48, 44), (8, 12)];

#[test]
fn wide_reads_take_one_call_for_every_four_dwords() {
    let (narrow, narrow_calls) = build(false);
    let (wide, wide_calls) = build(true);
    assert_eq!(narrow_calls, 28, "a rawBufferLoad a dword without the wide reads");
    assert_eq!(wide_calls, 7, "one call for every four with them");
    assert_ne!(narrow.dxil, wide.dxil, "the two forms write different code");
    // The view the Windows test reads: 12 dwords, so its loads run across
    // the end. Here only the cases it runs are named.
    let data: Vec<u32> = (0..12u32).map(|i| 0x1111_1111u32.wrapping_mul(i + 1) ^ 0x8000_0000).collect();
    assert_eq!(want_dwords(&data, [0, 8, 48])[12..], [0u32; 16], "past the view");
}

/// The dwords the two forms read of the same view, on the software adapter:
/// a call a dword and a call for every four must give the same ones, across
/// the view's end too.
#[cfg(windows)]
#[test]
fn scalar_buffer_loads_read_the_same_dwords_one_call_or_four() {
    let (narrow, _) = build(false);
    let (wide, _) = build(true);
    let data: Vec<u32> = (0..12u32).map(|i| 0x1111_1111u32.wrapping_mul(i + 1) ^ 0x8000_0000).collect();
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let host = super::host::Host::new(true).expect("no software adapter");
    let run = |shader: &HostShader, s0: u32, s1: u32| {
        let (root, pso) = host.compute_pipeline(shader).expect("compute_pipeline");
        let out = host
            .dispatch(
                &root,
                &pso,
                &super::host::Dispatch {
                    roots: vec![(0, super::host::Root::Constants(vec![s0, s1, 0, 0, 0]))],
                    views: &[
                        super::host::View::Buffer {
                            data: &data,
                            stride: super::host::Stride::Raw,
                        },
                        super::host::View::RawUav(32),
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        out.into_iter().next().expect("a UAV")
    };
    for (s0, s1) in CASES {
        let want = want_dwords(&data, starts_of(s0, s1));
        assert_eq!(run(&narrow, s0, s1)[..28], want[..], "s0={s0} s1={s1}: a call a dword");
        assert_eq!(run(&wide, s0, s1)[..28], want[..], "s0={s0} s1={s1}: a call for every four");
    }
}
