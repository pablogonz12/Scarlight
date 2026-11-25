//! GPU-based validation of the typed guard slots for every descriptor kind
//! the translator reads through the heap (see `bindless_bounds_tests` for the
//! layout). Filled in by the kind tests below.
//!
//! Each kind gets a 1-thread GFX10 compute program that loads its
//! descriptor straight from the view heap at an index from data (root type
//! `ResourceHeap`, as bindless code does) and accesses it once: loads store
//! what they read to a raw UAV at heap slot [`OUT`], stores and atomics
//! write a marker. The heap is the capped host heap of `bindless_bounds_tests`
//! with views of the kind at [`PLACED`] (the last title slot, then slots of
//! other draws' tables). Asserted on hardware and WARP:
//!
//! * the last title slot is reached and only it ([`IN_BOUNDS`]);
//! * an index past the title slots (other draws' tables) or past the heap
//!   reaches no placed view ([`OUT_OF_BOUNDS`]): a load reads zero, a store
//!   goes nowhere;
//! * with GPU-based validation (`VICTORY_D3D12_GBV=1`, else skipped) such a
//!   read meets a null descriptor of its own kind (`heap_guard`): no message
//!   about a descriptor's kind. The control is the old heap, every guard one
//!   null 2D texture SRV (`host::UNTYPED_GUARDS`); its count is printed, and
//!   a layer that reports nothing for it is noted, not failed.
//!
//! What the translator reads through a heap descriptor, by kind
//! (`emit/lower.rs`): raw buffer UAVs (stores, 32-bit atomics), typed
//! buffer SRVs (`buffer_load_format_*`), raw buffer SRVs (dword loads; an
//! indexed load is a raw load at index times the stride from the host's
//! table by heap slot, `slot_stride`, so a structured buffer is read as a
//! raw SRV), `s_buffer_load` as a constant buffer where the stride table
//! has 0 for the slot and as a raw SRV elsewhere, texture SRVs
//! (`image_load`), texture UAVs (`image_store`) and 64-bit image atomics on
//! a 2D texture UAV viewed as R32G32_UINT. A 64-bit buffer atomic
//! (`buffer_atomic_*_x2`) is not lowered (`buffer` rejects it by name).

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SOP2 `s_lshl_b32 sdst, src, 5`: a heap index to its byte offset (32
/// bytes a descriptor).
const fn index_to_bytes(sdst: u8, src: u8) -> u32 {
    0x8000_0000 | 0x1E << 23 | (sdst as u32 & 0x7F) << 16 | 0x85 << 8 | src as u32
}

/// SMEM `op sdata, sbase, soffset` (offset 0): `op` 2 `s_load_dwordx4`, 3
/// `s_load_dwordx8`, 8 `s_buffer_load_dword`; `soffset` 125 is null.
const fn smem(op: u8, sdata: u8, sbase: u8, soffset: u8) -> [u32; 2] {
    [
        0x3D << 26 | (op as u32) << 18 | (sdata as u32) << 6 | (sbase as u32 >> 1),
        (soffset as u32) << 25,
    ]
}

/// VOP1 `v_mov_b32 vdst, src0` (an SGPR or an inline constant, 0x80 = 0).
const fn v_mov_b32(vdst: u8, src0: u8) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | 0x001 << 9 | src0 as u32
}

/// MUBUF `op vdata, vaddr, srsrc, 0` (soffset the inline 0).
const fn mubuf(op: u8, vdata: u8, vaddr: u8, srsrc: u8, offen: bool, idxen: bool) -> [u32; 2] {
    [
        0x38 << 26 | (op as u32 & 0x7F) << 18 | (idxen as u32) << 13 | (offen as u32) << 12,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// MIMG `op vdata, vaddr, srsrc` of a 2D image with an 8-dword descriptor.
const fn mimg_2d(op: u8, vdata: u8, vaddr: u8, srsrc: u8, dmask: u8) -> [u32; 2] {
    [
        0x3C << 26 | (op as u32 & 0x7F) << 18 | (dmask as u32 & 0xF) << 8 | 1 << 3,
        ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// The capped heap of `bindless_bounds_tests`: title slots below `TITLE`,
/// the host's per-draw tables from `TITLE` up, the last 21 slots the typed
/// guards (`GUARD` the first).
const CAPPED: u32 = 1_000_000;
const TITLE: u32 = 737_856;
const GUARD: u32 = CAPPED - super::HEAP_GUARDS.len() as u32;
/// The raw UAV the loads store what they read to (a title slot).
const OUT: u32 = 0;
/// Views of the kind under test: the last title slot, then slots that hold
/// other draws' tables (the first dynamic, one inside, the last before the
/// guards).
const PLACED: [u32; 4] = [TITLE - 1, TITLE, 934_463, GUARD - 1];
/// The title index that must reach its slot, `PLACED[0]`.
const IN_BOUNDS: u32 = TITLE - 1;
/// Indices that must reach no placed view: the first dynamic slot, one
/// inside another draw's table, past the heap.
const OUT_OF_BOUNDS: [u32; 3] = [TITLE, 934_463, 1 << 25];

/// A descriptor kind read through the heap, and the access its program
/// makes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// `buffer_store_dword` through a raw buffer UAV.
    RawUav,
    /// `buffer_load_format_x` (idxen) through a typed buffer SRV, RGBA32F.
    TypedSrv,
    /// `buffer_load_dword` (offen) through a raw buffer SRV.
    RawSrv,
    /// `buffer_load_dword` (idxen, element 1): a structured read, lowered
    /// to a raw SRV read at element times the slot's stride.
    StructuredSrv,
    /// `s_buffer_load_dword` of a slot whose stride is 0: a constant buffer.
    Cbv,
    /// `s_buffer_load_dword` of a slot with a buffer stride: a raw SRV. Out
    /// of bounds the stride is the raw SRV guard's, 0, so the read is the
    /// constant buffer one, at the constant buffer guard.
    ScalarRawSrv,
    /// `image_load` (dmask 0x1) through a 2D texture SRV, RGBA32F.
    TextureSrv,
    /// `image_store` (dmask 0xF) through a 2D texture UAV, RGBA32F.
    TextureUav,
    /// `image_atomic_add` of 64 bits (dmask 0x3) through a 2D texture UAV
    /// viewed as R32G32_UINT.
    Atomic64,
}

impl Kind {
    /// Whether the access writes (a marker) rather than reads.
    fn writes(self) -> bool {
        matches!(self, Kind::RawUav | Kind::TextureUav | Kind::Atomic64)
    }

    /// Whether [`View`] can place a view the access reads as its own kind
    /// (no R32G32_UINT texture UAV for 64-bit atomics).
    fn placeable(self) -> bool {
        self != Kind::Atomic64
    }

    /// The bytes per element the host's stride table (`SLOT_STRIDES`) has
    /// for this kind's placed slots: a buffer view's stride, 0 for a
    /// constant buffer or a texture.
    fn stride(self) -> u32 {
        match self {
            Kind::RawUav | Kind::RawSrv | Kind::StructuredSrv | Kind::ScalarRawSrv => 4,
            Kind::TypedSrv => 16,
            _ => 0,
        }
    }

    /// The program: the descriptor at heap index `s0` into `s[8:11]` (or
    /// `s[8:15]` for an image), the output UAV at heap index `s2` into
    /// `s[16:19]`, `s1` the marker.
    fn words(self) -> Vec<u32> {
        let mut w = vec![
            s_mov_b32(5, 0x80), // s5 = 0: the heap address's high half
            index_to_bytes(6, 0),
            index_to_bytes(7, 2),
        ];
        let image = matches!(self, Kind::TextureSrv | Kind::TextureUav | Kind::Atomic64);
        w.extend(smem(if image { 3 } else { 2 }, 8, 4, 6));
        if !self.writes() {
            w.extend(smem(2, 16, 4, 7));
        }
        w.push(v_mov_b32(0, 0x80)); // v0 = 0: offset, element or x
        w.push(v_mov_b32(1, 0x80)); // v1 = 0: y
        match self {
            Kind::RawUav => {
                w.push(v_mov_b32(2, 1));
                w.extend(mubuf(0x1C, 2, 0, 8, false, false));
            }
            Kind::TypedSrv => w.extend(mubuf(0x00, 2, 0, 8, false, true)),
            Kind::RawSrv => w.extend(mubuf(0x0C, 2, 0, 8, true, false)),
            Kind::StructuredSrv => {
                w.push(v_mov_b32(0, 0x81)); // element 1
                w.extend(mubuf(0x0C, 2, 0, 8, false, true));
            }
            Kind::Cbv | Kind::ScalarRawSrv => {
                w.extend(smem(8, 20, 8, 125));
                w.push(v_mov_b32(2, 20));
            }
            Kind::TextureSrv => w.extend(mimg_2d(0x00, 2, 0, 8, 0x1)),
            Kind::TextureUav => {
                for k in 2..6 {
                    w.push(v_mov_b32(k, 1));
                }
                w.extend(mimg_2d(0x08, 2, 0, 8, 0xF));
            }
            Kind::Atomic64 => {
                w.push(v_mov_b32(2, 1));
                w.push(v_mov_b32(3, 0x80));
                w.extend(mimg_2d(0x11, 2, 0, 8, 0x3));
            }
        }
        if !self.writes() {
            w.extend(mubuf(0x1C, 2, 0, 16, false, false));
        }
        w.push(S_ENDPGM);
        w
    }
}

/// Lowers `kind`'s program with the limit a host capped at [`CAPPED`] views
/// sets (`set_view_heap_limit(views - 1)`), restoring the limit after.
fn lowered(kind: Kind) -> HostShader {
    super::shader::with_view_limit(CAPPED - 1, || lower_cs(&kind.words()))
}

/// Lowers a 1-thread wave32 program with 4 root constants at `s0` and the
/// view heap's base at `s4`, validated by `dxil.dll` when present.
fn lower_cs(words: &[u32]) -> HostShader {
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
                root: RootType::ResourceHeap,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::ResourceHeap,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
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

/// The value placed view `j` holds (loads), as float bits.
fn value(j: usize) -> u32 {
    (10.0 + j as f32).to_bits()
}

/// The marker case `i` stores, as float bits (exact in RGBA32F, nonzero).
fn marker(i: usize) -> u32 {
    (2.0 + i as f32).to_bits()
}

/// The contents of the views of a kind at [`PLACED`].
struct Contents {
    buffers: Vec<Vec<u32>>,
    texels: Vec<[[f32; 4]; 1]>,
}

impl Contents {
    fn of(kind: Kind) -> Self {
        let buffers = (0..PLACED.len())
            .map(|j| match kind {
                // Element 1 holds the value: a read at element 0 (no
                // stride) gives a value no case accepts.
                Kind::StructuredSrv => vec![0xBAD, value(j), 0xBAD, 0xBAD],
                _ => vec![value(j); 4],
            })
            .collect();
        let texels = (0..PLACED.len())
            .map(|j| [[f32::from_bits(value(j)), 0.0, 0.0, 1.0]])
            .collect();
        Self { buffers, texels }
    }

    /// The heap's views and their slots: the output UAV at [`OUT`], then
    /// one of `kind` at each of [`PLACED`].
    fn views(&self, kind: Kind) -> (Vec<View<'_>>, Vec<u32>) {
        let mut views = vec![View::RawUav(1)];
        for (j, data) in self.buffers.iter().enumerate() {
            views.push(match kind {
                Kind::RawUav => View::RawUav(1),
                Kind::TypedSrv => View::Buffer {
                    data,
                    stride: Stride::Typed,
                },
                Kind::RawSrv | Kind::StructuredSrv | Kind::ScalarRawSrv => View::Buffer {
                    data,
                    stride: Stride::Raw,
                },
                Kind::Cbv => View::Cbv(data),
                Kind::TextureSrv => View::Texture(1, 1, &self.texels[j]),
                Kind::TextureUav => View::TextureUav(1, 1),
                // Not this kind's view (R32_UINT, not R32G32_UINT): only
                // the out-of-bounds cases run, which must leave it alone.
                Kind::Atomic64 => View::UintTextureUav(1, 1),
            });
        }
        let slots = std::iter::once(OUT).chain(PLACED).collect();
        (views, slots)
    }
}

/// What a device did for one kind: per index, which of [`PLACED`] was
/// reached (`None`: none, the access met a null view), the debug layer's
/// messages, and on the old heap the dispatch that removed the device.
struct Run {
    label: &'static str,
    reached: Vec<Option<u32>>,
    messages: Vec<String>,
    removed: Option<String>,
}

impl Run {
    /// Messages about a descriptor's kind (GPU-based validation: the
    /// descriptor at the index is not of the type the shader declares).
    fn kind_messages(&self) -> usize {
        self.messages.iter().filter(|m| is_kind_message(m)).count()
    }
}

/// "Descriptor type doesn't match shader register type" (an SRV for a UAV
/// register, ...) or "SRV resource dimensions differs from that expected
/// by shader" (a 2D texture for a buffer, ...).
fn is_kind_message(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    m.contains("descriptor")
        && (m.contains("match") || m.contains("type") || m.contains("dimension"))
}

/// Runs `kind`'s program for each of `indices` on hardware and WARP (each
/// skipped without a device, or for [`Kind::Atomic64`] where the device has
/// no 64-bit atomics on heap resources), the guards typed or, with
/// `untyped`, all one null 2D texture SRV.
fn run(kind: Kind, indices: &[u32], untyped: bool) -> Vec<Run> {
    let shader = lowered(kind);
    let contents = Contents::of(kind);
    let (views, slots) = contents.views(kind);
    // The host's stride table by heap slot, for shaders that read it.
    let mut strides = vec![0u32; super::SLOT_STRIDES_COUNT as usize];
    for &slot in &PLACED {
        strides[slot as usize] = kind.stride();
    }
    strides[OUT as usize] = 4;
    // Root views in the order of `Host::root_signature_for`, after the
    // user data (root parameter 0).
    let mut root_views: Vec<(u8, u8)> = shader
        .root_cbvs
        .iter()
        .map(|&r| (r, 0))
        .chain(shader.root_srvs.iter().map(|&r| (r, 1)))
        .chain(shader.root_uavs.iter().map(|&r| (r, 2)))
        .collect();
    root_views.sort_unstable();
    root_views.dedup();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    super::host::UNTYPED_GUARDS.with(|c| c.set(untyped));
    let mut out = Vec::new();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = match host.compute_pipeline(&shader) {
            Ok(p) => p,
            Err(e) if kind == Kind::Atomic64 => {
                eprintln!("{label}: no 64-bit atomics on heap resources ({e}); skipped");
                continue;
            }
            Err(e) => panic!("{label}: {kind:?}: compute_pipeline failed: {e}"),
        };
        let _ = host.debug_messages();
        let mut reached = Vec::new();
        let mut removed = None;
        for (i, &index) in indices.iter().enumerate() {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot, u.dword) {
                    (RootType::Constants, 0, 0) => index,
                    (RootType::Constants, 0, 1) => marker(i),
                    (RootType::Constants, 0, 2) => OUT,
                    (RootType::Constants, 0, _) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let mut roots = vec![(0, Root::Constants(ud))];
            for (n, &view) in root_views.iter().enumerate() {
                assert_eq!(
                    view,
                    (super::SLOT_STRIDES, 1),
                    "{kind:?}: an unexpected root view"
                );
                let data = strides.clone();
                roots.push((1 + n as u32, Root::View { srv: true, data }));
            }
            let got = host.dispatch_placed(
                &root,
                &pso,
                &Dispatch {
                    roots,
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
                Some((CAPPED, &slots)),
            );
            let got = match got {
                Ok(got) => got,
                // The old heap's null of another kind may remove the
                // device (undefined): recorded for the report, the
                // device's other cases left out.
                Err(e) if untyped => {
                    removed = Some(format!("index {index}: {e}"));
                    break;
                }
                Err(e) => panic!("{label}: {kind:?}: index {index}: dispatch failed: {e}"),
            };
            // UAVs in heap order: the output, then the placed views of a
            // kind that writes.
            let hit: Vec<u32> = if kind.writes() {
                for (j, uav) in got[1..].iter().enumerate() {
                    assert!(
                        uav[0] == 0 || uav[0] == marker(i),
                        "{label}: {kind:?}: index {index}: {:#x} at {}",
                        uav[0],
                        PLACED[j]
                    );
                }
                (0..PLACED.len())
                    .filter(|&j| got[1 + j][0] != 0)
                    .map(|j| PLACED[j])
                    .collect()
            } else {
                let read = got[0][0];
                let from: Vec<u32> = (0..PLACED.len())
                    .filter(|&j| read == value(j))
                    .map(|j| PLACED[j])
                    .collect();
                assert!(
                    read == 0 || !from.is_empty(),
                    "{label}: {kind:?}: index {index} read {read:#x}, no placed view's"
                );
                from
            };
            assert!(
                hit.len() <= 1,
                "{label}: {kind:?}: index {index} reached {hit:?}"
            );
            reached.push(hit.first().copied());
        }
        out.push(Run {
            label,
            reached,
            messages: host.debug_messages(),
            removed,
        });
    }
    super::host::UNTYPED_GUARDS.with(|c| c.set(false));
    out
}

/// The test of one kind: the title index reaches its slot, an index out of
/// bounds none; then, with GPU-based validation, the same out-of-bounds
/// accesses with typed guards (no message about a descriptor's kind) and
/// on the old heap of one null 2D SRV (counted and printed).
fn check(kind: Kind) {
    let mut indices = Vec::new();
    if kind.placeable() {
        indices.push(IN_BOUNDS);
    } else {
        eprintln!("{kind:?}: no view of this kind to place; out-of-bounds cases only");
    }
    indices.extend(OUT_OF_BOUNDS);
    let want: Vec<Option<u32>> = indices
        .iter()
        .map(|&i| (i == IN_BOUNDS).then_some(IN_BOUNDS))
        .collect();
    let runs = run(kind, &indices, false);
    assert!(
        !runs.is_empty() || kind == Kind::Atomic64,
        "{kind:?}: no SM 6.6 device"
    );
    for r in &runs {
        eprintln!(
            "{}: {kind:?}: indices {indices:?} reached {:?}",
            r.label, r.reached
        );
        assert_eq!(r.reached, want, "{}: {kind:?}", r.label);
    }
    if std::env::var_os("VICTORY_D3D12_GBV").is_none() {
        eprintln!("{kind:?}: VICTORY_D3D12_GBV unset; GPU-based validation skipped");
        return;
    }
    let typed = run(kind, &OUT_OF_BOUNDS, false);
    let untyped = run(kind, &OUT_OF_BOUNDS, true);
    for (t, u) in typed.iter().zip(&untyped) {
        let label = t.label;
        eprintln!(
            "{label}: {kind:?}: GBV messages about a descriptor's kind: typed guards {} (of {}), \
             one null 2D SRV {} (of {})",
            t.kind_messages(),
            t.messages.len(),
            u.kind_messages(),
            u.messages.len()
        );
        let shown = t
            .messages
            .iter()
            .chain(&u.messages)
            .filter(|m| is_kind_message(m));
        for m in shown.take(2) {
            eprintln!("  {}", m.chars().take(300).collect::<String>());
        }
        if let Some(e) = &u.removed {
            eprintln!("{label}: {kind:?}: one null 2D SRV removed the device ({e})");
        } else if u.kind_messages() == 0 {
            eprintln!("{label}: {kind:?}: validation reported nothing for the control either");
        }
        assert_eq!(
            t.kind_messages(),
            0,
            "{label}: {kind:?}: a guard of another kind than the access was read"
        );
    }
}

#[test]
fn raw_buffer_uav_store_meets_a_raw_uav_guard() {
    check(Kind::RawUav);
}

#[test]
fn typed_buffer_srv_load_meets_a_typed_buffer_guard() {
    check(Kind::TypedSrv);
}

#[test]
fn raw_buffer_srv_load_meets_a_raw_srv_guard() {
    check(Kind::RawSrv);
}

#[test]
fn structured_buffer_load_meets_a_raw_srv_guard() {
    check(Kind::StructuredSrv);
}

#[test]
fn scalar_load_of_a_constant_buffer_meets_a_cbv_guard() {
    check(Kind::Cbv);
}

#[test]
fn scalar_load_of_a_buffer_view_reads_it_raw_and_meets_the_cbv_guard() {
    check(Kind::ScalarRawSrv);
}

/// (The old heap's one null view is a 2D texture SRV too: there the
/// control is the same heap, and validation reports nothing either way.)
#[test]
fn texture_srv_load_meets_a_2d_srv_guard() {
    check(Kind::TextureSrv);
}

#[test]
fn texture_uav_store_meets_a_2d_uav_guard() {
    check(Kind::TextureUav);
}

#[test]
fn image_atomic_64_meets_a_2d_uav_guard() {
    check(Kind::Atomic64);
}
