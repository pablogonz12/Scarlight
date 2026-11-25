//! SPIR-V emitter (native-path spike, branch `spike/spirv-emit`): GFX10 ISA
//! -> SPIR-V, no DXIL and no dxil-spirv.
//!
//! This is deliberately *not* a port of [`super::lower`]: it walks the same
//! decoded IR ([`crate::ir`]) one machine instruction at a time, but each
//! register dword is a function-local SPIR-V variable and every instruction
//! is load/op/store. That drops SSA plumbing (phis, dominance) so the whole
//! CFG can be emitted as one dispatcher loop with an `OpSwitch`: valid
//! structured SPIR-V for any program, at the cost of code quality. The spike
//! measures coverage and validation, not speed; mem2reg and real structuring
//! are the next step (`xcl-bin/notes/spirv-spike.md`).
//!
//! Memory is the console's own address space: every `s_load_*`,
//! `s_buffer_load_*`, `buffer_*`, `global_*` and `flat_*` becomes an
//! `OpLoad`/`OpStore` through a 64-bit address the program itself computes,
//! i.e. `physical_storage_buffer` + buffer device address. The module has no
//! descriptors at all; the root arguments arrive as push constants, as the
//! console's user SGPRs do.
//!
//! What this does not model yet: EXEC is a scalar all-ones/all-zeros mask
//! rather than a per-lane one (as `lower`'s per-lane form is), so programs
//! whose lanes diverge inside a wave compute the wrong values while still
//! emitting valid modules; images, DPP/SDWA source modifiers, typed buffer
//! formats and wave/ballot operations are reported as gaps.

use std::collections::HashMap;

use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::io::{Input, Io};
use crate::ir::{Exit, Inst, Program, Reg};
use crate::native::{HwStage, NativeProgram, StageIo};

// ---- SPIR-V constants -------------------------------------------------------
// The SPIR-V numbers, named: a few are not used yet, and deleting them
// would only mean looking them up again in the next round.
#[allow(dead_code)]
mod op {
    pub const NAME: u16 = 5;
    pub const EXTENSION: u16 = 10;
    pub const EXT_INST_IMPORT: u16 = 11;
    pub const EXT_INST: u16 = 12;
    pub const MEMORY_MODEL: u16 = 14;
    pub const ENTRY_POINT: u16 = 15;
    pub const EXECUTION_MODE: u16 = 16;
    pub const CAPABILITY: u16 = 17;
    pub const TYPE_VOID: u16 = 19;
    pub const TYPE_BOOL: u16 = 20;
    pub const TYPE_INT: u16 = 21;
    pub const TYPE_FLOAT: u16 = 22;
    pub const TYPE_VECTOR: u16 = 23;
    pub const TYPE_IMAGE: u16 = 25;
    pub const TYPE_SAMPLER: u16 = 26;
    pub const TYPE_SAMPLED_IMAGE: u16 = 27;
    pub const TYPE_ARRAY: u16 = 28;
    pub const TYPE_STRUCT: u16 = 30;
    pub const TYPE_POINTER: u16 = 32;
    pub const TYPE_FUNCTION: u16 = 33;
    pub const CONSTANT: u16 = 43;
    pub const FUNCTION: u16 = 54;
    pub const FUNCTION_END: u16 = 56;
    pub const VARIABLE: u16 = 59;
    pub const LOAD: u16 = 61;
    pub const STORE: u16 = 62;
    pub const ACCESS_CHAIN: u16 = 65;
    pub const DECORATE: u16 = 71;
    pub const MEMBER_DECORATE: u16 = 72;
    pub const COMPOSITE_CONSTRUCT: u16 = 80;
    pub const COMPOSITE_EXTRACT: u16 = 81;
    pub const CONVERT_F_TO_U: u16 = 109;
    pub const CONVERT_F_TO_S: u16 = 110;
    pub const CONVERT_S_TO_F: u16 = 111;
    pub const CONVERT_U_TO_F: u16 = 112;
    pub const U_CONVERT: u16 = 113;
    pub const S_CONVERT: u16 = 114;
    pub const F_CONVERT: u16 = 115;
    pub const CONVERT_U_TO_PTR: u16 = 120;
    pub const BITCAST: u16 = 124;
    pub const F_NEGATE: u16 = 127;
    pub const I_ADD: u16 = 128;
    pub const F_ADD: u16 = 129;
    pub const I_SUB: u16 = 130;
    pub const F_SUB: u16 = 131;
    pub const I_MUL: u16 = 132;
    pub const F_MUL: u16 = 133;
    pub const U_DIV: u16 = 134;
    pub const S_DIV: u16 = 135;
    pub const F_DIV: u16 = 136;
    pub const U_MOD: u16 = 137;
    pub const S_REM: u16 = 139;
    pub const F_REM: u16 = 140;
    pub const F_MOD: u16 = 141;
    pub const LOGICAL_AND: u16 = 167;
    pub const LOGICAL_NOT: u16 = 168;
    pub const SELECT: u16 = 169;
    pub const I_EQUAL: u16 = 170;
    pub const I_NOT_EQUAL: u16 = 171;
    pub const U_GREATER_THAN: u16 = 172;
    pub const S_GREATER_THAN: u16 = 173;
    pub const U_GREATER_THAN_EQUAL: u16 = 174;
    pub const S_GREATER_THAN_EQUAL: u16 = 175;
    pub const U_LESS_THAN: u16 = 176;
    pub const S_LESS_THAN: u16 = 177;
    pub const U_LESS_THAN_EQUAL: u16 = 178;
    pub const S_LESS_THAN_EQUAL: u16 = 179;
    pub const F_ORD_EQUAL: u16 = 180;
    pub const F_UNORD_EQUAL: u16 = 181;
    pub const F_ORD_NOT_EQUAL: u16 = 182;
    pub const F_UNORD_NOT_EQUAL: u16 = 183;
    pub const F_ORD_LESS_THAN: u16 = 184;
    pub const F_UNORD_LESS_THAN: u16 = 185;
    pub const F_ORD_GREATER_THAN: u16 = 186;
    pub const F_UNORD_GREATER_THAN: u16 = 187;
    pub const F_ORD_LESS_THAN_EQUAL: u16 = 188;
    pub const F_UNORD_LESS_THAN_EQUAL: u16 = 189;
    pub const F_ORD_GREATER_THAN_EQUAL: u16 = 190;
    pub const F_UNORD_GREATER_THAN_EQUAL: u16 = 191;
    pub const SHIFT_RIGHT_LOGICAL: u16 = 194;
    pub const SHIFT_RIGHT_ARITHMETIC: u16 = 195;
    pub const SHIFT_LEFT_LOGICAL: u16 = 196;
    pub const BITWISE_OR: u16 = 197;
    pub const BITWISE_XOR: u16 = 198;
    pub const BITWISE_AND: u16 = 199;
    pub const BITWISE_NOT: u16 = 200;
    pub const BIT_FIELD_INSERT: u16 = 201;
    pub const BIT_FIELD_S_EXTRACT: u16 = 202;
    pub const BIT_FIELD_U_EXTRACT: u16 = 203;
    pub const BIT_REVERSE: u16 = 204;
    pub const BIT_COUNT: u16 = 205;
    pub const CONTROL_BARRIER: u16 = 224;
    pub const IMAGE_SAMPLE_EXPLICIT_LOD: u16 = 88;
    pub const IMAGE_SAMPLE_DREF_EXPLICIT_LOD: u16 = 90;
    pub const IMAGE_FETCH: u16 = 95;
    pub const IMAGE_GATHER: u16 = 96;
    pub const IMAGE_DREF_GATHER: u16 = 97;
    pub const IMAGE_READ: u16 = 98;
    pub const IMAGE_WRITE: u16 = 99;
    pub const IMAGE: u16 = 100;
    pub const IMAGE_QUERY_SIZE_LOD: u16 = 103;
    pub const IMAGE_QUERY_SIZE: u16 = 104;
    pub const IMAGE_QUERY_LEVELS: u16 = 106;
    pub const SAMPLED_IMAGE: u16 = 86;
    pub const ATOMIC_I_ADD: u16 = 234;
    pub const ATOMIC_I_SUB: u16 = 235;
    pub const ATOMIC_S_MIN: u16 = 236;
    pub const ATOMIC_U_MIN: u16 = 237;
    pub const ATOMIC_S_MAX: u16 = 238;
    pub const ATOMIC_U_MAX: u16 = 239;
    pub const ATOMIC_AND: u16 = 240;
    pub const ATOMIC_OR: u16 = 241;
    pub const ATOMIC_XOR: u16 = 242;
    pub const ATOMIC_EXCHANGE: u16 = 229;
    pub const LOOP_MERGE: u16 = 246;
    pub const SELECTION_MERGE: u16 = 247;
    pub const LABEL: u16 = 248;
    pub const BRANCH: u16 = 249;
    pub const BRANCH_CONDITIONAL: u16 = 250;
    pub const SWITCH: u16 = 251;
    pub const RETURN: u16 = 253;
}

mod decoration {
    pub const BLOCK: u32 = 2;
    pub const ARRAY_STRIDE: u32 = 6;
    pub const BUILT_IN: u32 = 11;
    pub const BINDING: u32 = 33;
    pub const DESCRIPTOR_SET: u32 = 34;
    pub const OFFSET: u32 = 35;
}

/// Capabilities the image bindings and the cross-lane code need (SPIR-V 1.6
/// numbering).
#[allow(dead_code)]
mod cap {
    pub const SAMPLED_1D: u32 = 43;
    pub const IMAGE_1D: u32 = 44;
    pub const SAMPLED_CUBE_ARRAY: u32 = 45;
    pub const IMAGE_QUERY: u32 = 50;
    pub const STORAGE_READ_WITHOUT_FORMAT: u32 = 55;
    pub const STORAGE_WRITE_WITHOUT_FORMAT: u32 = 56;
    /// `OpGroupNonUniform*`: cross-lane code runs on the console's own wave.
    pub const GROUP_NON_UNIFORM: u32 = 61;
    /// `OpGroupNonUniformShuffle`: another lane's value, DPP and lane reads.
    pub const GROUP_NON_UNIFORM_SHUFFLE: u32 = 65;
}

mod storage {
    pub const UNIFORM_CONSTANT: u32 = 0;
    pub const INPUT: u32 = 1;
    pub const FUNCTION: u32 = 7;
    pub const PUSH_CONSTANT: u32 = 9;
    pub const WORKGROUP: u32 = 4;
    pub const PHYSICAL_STORAGE_BUFFER: u32 = 5349;
}

mod builtin {
    pub const WORKGROUP_ID: u32 = 26;
    pub const LOCAL_INVOCATION_ID: u32 = 27;
}

/// GLSL.std.450 instruction numbers used here.
#[allow(dead_code)]
mod glsl {
    pub const ROUND_EVEN: u32 = 2;
    pub const TRUNC: u32 = 3;
    pub const FABS: u32 = 4;
    pub const FLOOR: u32 = 8;
    pub const CEIL: u32 = 9;
    pub const FRACT: u32 = 10;
    pub const SIN: u32 = 13;
    pub const COS: u32 = 14;
    pub const POW: u32 = 26;
    pub const EXP2: u32 = 29;
    pub const LOG2: u32 = 30;
    pub const SQRT: u32 = 31;
    pub const INVERSE_SQRT: u32 = 32;
    pub const FMIN: u32 = 37;
    pub const UMIN: u32 = 38;
    pub const SMIN: u32 = 39;
    pub const FMAX: u32 = 40;
    pub const UMAX: u32 = 41;
    pub const SMAX: u32 = 42;
    pub const FCLAMP: u32 = 43;
    pub const FIND_I_LSB: u32 = 73;
    pub const FIND_S_MSB: u32 = 75;
}

/// The register dwords the machine can name, mapped into one space.
const SCC_SLOT: u32 = 384;
const VGPR_BASE: u32 = 128;
/// Slots past the machine's scalars: the scalars `ir` parked in VGPR lanes.
const PARKED_BASE: u32 = 320;
/// One slot per scalar code the IR can name (0-127 and the parked ones).
const SLOTS: u32 = PARKED_BASE + (256 - crate::ir::PARKED as u32);

fn slot(reg: Reg) -> u32 {
    match reg {
        Reg::S(code) if code < crate::ir::PARKED => u32::from(code),
        Reg::S(code) => PARKED_BASE + u32::from(code) - u32::from(crate::ir::PARKED),
        Reg::V(reg) => VGPR_BASE + u32::from(reg),
        Reg::Scc => SCC_SLOT,
    }
}

// ---- binary writer ----------------------------------------------------------

struct M {
    words: Vec<u32>,
    next: u32,
}

impl M {
    fn new() -> M {
        M {
            words: vec![0x0723_0203, 0x0001_0500, 0, 0, 0],
            next: 1,
        }
    }

    fn id(&mut self) -> u32 {
        let i = self.next;
        self.next += 1;
        i
    }

    fn ins(&mut self, opcode: u16, ty: u32, result: u32, ops: &[u32]) {
        let n = 1 + u32::from(ty != 0) + u32::from(result != 0) + ops.len() as u32;
        self.words.push((n << 16) | u32::from(opcode));
        if ty != 0 {
            self.words.push(ty);
        }
        if result != 0 {
            self.words.push(result);
        }
        self.words.extend_from_slice(ops);
    }

    /// An instruction whose operands end in a literal string.
    fn ins_str(&mut self, opcode: u16, ty: u32, result: u32, ops: &[u32], s: &str) {
        let mut tail = Vec::new();
        let bytes = s.as_bytes();
        for chunk in bytes.chunks(4) {
            let mut w = 0u32;
            for (i, b) in chunk.iter().enumerate() {
                w |= u32::from(*b) << (8 * i);
            }
            tail.push(w);
        }
        if bytes.len().is_multiple_of(4) {
            tail.push(0);
        }
        let n =
            1 + u32::from(ty != 0) + u32::from(result != 0) + ops.len() as u32 + tail.len() as u32;
        self.words.push((n << 16) | u32::from(opcode));
        if ty != 0 {
            self.words.push(ty);
        }
        if result != 0 {
            self.words.push(result);
        }
        self.words.extend_from_slice(ops);
        self.words.extend_from_slice(&tail);
    }

    /// An instruction whose string sits between two operand lists: what
    /// `OpEntryPoint` needs (execution model, entry id, name, interface).
    fn ins_str_at(&mut self, opcode: u16, pre: &[u32], s_: &str, post: &[u32]) {
        let mut tail = Vec::new();
        let bytes = s_.as_bytes();
        for chunk in bytes.chunks(4) {
            let mut w = 0u32;
            for (i, b) in chunk.iter().enumerate() {
                w |= u32::from(*b) << (8 * i);
            }
            tail.push(w);
        }
        if bytes.len().is_multiple_of(4) {
            tail.push(0);
        }
        let n = 1 + pre.len() as u32 + tail.len() as u32 + post.len() as u32;
        self.words.push((n << 16) | u32::from(opcode));
        self.words.extend_from_slice(pre);
        self.words.extend_from_slice(&tail);
        self.words.extend_from_slice(post);
    }

    fn finish(mut self) -> Vec<u32> {
        self.words[3] = self.next;
        self.words
    }
}

type Why = &'static str;
type R<T> = Result<T, Gap>;

/// What the spike does not emit yet, with a reason a report can group.
#[derive(Debug, Clone, Copy)]
pub struct Gap(pub Why);

impl From<Gap> for ShaderError {
    fn from(g: Gap) -> Self {
        ShaderError::Unsupported {
            offset: 0,
            what: g.0,
        }
    }
}

fn gap<T>(why: Why) -> R<T> {
    Err(Gap(why))
}

// ---- emitter ----------------------------------------------------------------

/// One image binding: a static site, one `UniformConstant` variable in
/// descriptor set 0. `rsrc` is the console descriptor's register base and
/// `dim` the instruction's dimension field, so a site is exactly what one
/// `MIMG` instruction names; the host builds the `VkImageView` from the raw
/// descriptor words the program would otherwise have loaded (see the notes'
/// binding model). The module carries no format: the view's format does the
/// console's texture-unit conversion.
#[derive(Debug, Clone, Copy)]
struct ImgBind {
    rsrc: u8,
    dim: u8,
    storage: bool,
    ty: u32,
    si_ty: u32,
    pty: u32,
    var: u32,
}

/// The SPIR-V `Dim`, arrayed flag and coordinate count of a console image
/// dimension (the ISA's order: 1D, 2D, 3D, cube, 1D array, 2D array, then the
/// two MSAA forms, `gfx10::fields::image_addr_dwords`).
fn spv_dim(dim: u8) -> Option<(u32, bool, usize)> {
    match dim {
        0 => Some((0, false, 1)),
        1 => Some((1, false, 2)),
        2 => Some((2, false, 3)),
        3 => Some((3, false, 3)),
        4 => Some((0, true, 2)),
        5 => Some((1, true, 3)),
        _ => None,
    }
}

/// Where logical address dword `k` of an image access sits in the address
/// VGPRs: the extras (texel offset, bias, comparison value) and the gradients
/// are whole dwords, the coordinates and the level pack two per dword with
/// `a16` (`gfx10::fields::image_addr_dwords`, LLVM's `lowerImage`).
fn addr_place(k: usize, extras: usize, grads: usize, a16: bool) -> (usize, Option<u32>) {
    let block = extras + grads;
    if k < block {
        return (k, None);
    }
    let r = k - block;
    if !a16 {
        return (block + r, None);
    }
    (block + r / 2, Some(r as u32 % 2))
}

/// The `_c`/`_d`/`_l`/`_b`/`_o`/`_cl` tokens of an `image_sample` or
/// `image_gather4` name (`lower::sample_form`'s spelling).
#[derive(Debug, Default, Clone, Copy)]
struct SampleFlags {
    c: bool,
    d: bool,
    l: bool,
    lz: bool,
    b: bool,
    o: bool,
    g16: bool,
    gather: bool,
}

impl SampleFlags {
    fn parse(name: &str) -> Option<SampleFlags> {
        let (gather, rest) = match name.strip_prefix("image_sample") {
            Some(rest) => (false, rest),
            None => (true, name.strip_prefix("image_gather4")?),
        };
        let mut f = SampleFlags {
            gather,
            ..SampleFlags::default()
        };
        for token in rest.split('_').skip(1) {
            match token {
                "c" => f.c = true,
                "d" | "cd" => f.d = true,
                "g16" => f.g16 = true,
                "l" => f.l = true,
                "lz" => f.lz = true,
                "b" => f.b = true,
                "o" => f.o = true,
                "cl" => {}
                _ => return None,
            }
        }
        Some(f)
    }
}


pub struct Emitter<'a> {
    m: M,
    p: &'a Program,
    io: &'a Io,
    t_void: u32,
    t_bool: u32,
    t_i32: u32,
    t_i64: u32,
    t_f32: u32,
    t_f16: u32,
    t_i16: u32,
    t_u16: u32,
    t_u32: u32,
    t_u64: u32,
    t_v3u32: u32,
    pf_i32: u32,
    pf_i64: u32,
    pf_f32: u32,
    pf_bool: u32,
    ps32: u32,
    ps64: u32,
    psf32: u32,
    pw32: u32,
    p_pc: u32,
    p_pc_i32: u32,
    p_arr: u32,
    t_lds: u32,
    t_arr: u32,
    t_struct: u32,
    pfn: u32,
    p_in_v3: u32,
    pc: u32,
    lds: u32,
    ext: u32,
    /// The entry point function.
    main: u32,
    /// The vec3 thread-group id and local invocation id inputs, if read.
    tid_var: Option<u32>,
    gid_var: Option<u32>,
    regs: Vec<u32>,
    scc: u32,
    zero: u32,
    one: u32,
    minus_one: u32,
    ci: HashMap<u32, u32>,
    ci64: HashMap<u64, u32>,
    /// Constant instructions, spliced in before the functions (a constant
    /// may be created while the body is emitted, and the module's layout
    /// wants every constant before the first function).
    consts: Vec<u32>,
    pc_var: u32,
    loop_head: u32,
    dispatch: u32,
    cont: u32,
    /// The dispatch select's merge block, inside the loop.
    sel_merge: u32,
    exit: u32,
    labels: Vec<u32>,
    lds_words: u32,
    /// The thread-group size the module declares (`LocalSize`).
    local_size: [u32; 3],
    scope_device: u32,
    scope_workgroup: u32,
    sem_acq_rel_workgroup: u32,
    /// The image and sampler bindings this program names, in program order.
    images: Vec<ImgBind>,
    /// Sampler descriptor register and its variable.
    samplers: Vec<(u8, u32)>,
    /// Source operands whose read this instruction replaces (SDWA field
    /// selection, `opsel` half selection).
    src_patch: Vec<(Operand, u32)>,
    /// The SDWA destination mapping of the instruction being emitted.
    sdwa_dst: Option<crate::gfx10::Sdwa>,
    t_smp: u32,
    p_smp: u32,
    v2i: u32,
    v3i: u32,
    v4i: u32,
    v2f: u32,
    v3f: u32,
    v4f: u32,
}

/// One compute program as a SPIR-V module, with the two facts the pipeline
/// that runs it needs beyond the words.
///
/// The console runs a program's threads in waves of 32 or 64 lanes, and the
/// program reads other lanes of its own wave (DPP, `v_readlane`, `v_mbcnt`).
/// A host subgroup is a different wave unless it is pinned, so the module
/// carries the console's wave size: the pipeline asks for it with
/// `VkPipelineShaderStageRequiredSubgroupSizeCreateInfo`
/// (`requiredSubgroupSize = wave_size`) chained onto the shader stage, sets
/// `VK_PIPELINE_SHADER_STAGE_CREATE_REQUIRE_FULL_SUBGROUPS_BIT`, and checks
/// `VkPhysicalDeviceSubgroupSizeControlProperties` first (32..64 on RADV
/// RDNA4). `REQUIRE_FULL_SUBGROUPS` needs `local_size[0]` to be a multiple of
/// `wave_size` (see [`SpirvProgram::full_subgroups`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpirvProgram {
    /// The SPIR-V words.
    pub words: Vec<u32>,
    /// The console's wave size, 32 or 64 (the ISA record's `wave32` flag, the
    /// same source the DXIL path takes its `wave_size` from).
    pub wave_size: u32,
    /// The thread-group size (the module's `LocalSize`).
    pub local_size: [u32; 3],
}

impl SpirvProgram {
    /// Whether the pipeline may set `REQUIRE_FULL_SUBGROUPS`: it needs a whole
    /// number of subgroups per group along x, which every console group size
    /// does not give (`v_mbcnt` programs with 32 threads and 64-lane waves).
    pub fn full_subgroups(&self) -> bool {
        self.local_size[0].is_multiple_of(self.wave_size)
    }

    /// What a Vulkan compute pipeline has to ask for to run this program the
    /// way the console does, for whoever creates it (see
    /// `docs/spirv-subgroup-size.md`): the subgroup size pinned to the
    /// program's wave size, and full subgroups where the group allows it.
    /// A group that cannot have full subgroups gets the reason here, so the
    /// caller reports it rather than dropping the flag quietly.
    pub fn subgroup_plan(&self) -> SubgroupPlan {
        let full = self.full_subgroups();
        SubgroupPlan {
            required_subgroup_size: self.wave_size,
            require_full_subgroups: full,
            no_full_subgroups: (!full).then_some(NO_FULL_SUBGROUPS_GROUP),
        }
    }
}

/// `REQUIRE_FULL_SUBGROUPS` is out: `local_size[0]` is not a multiple of the
/// wave size (a 32-thread group of a 64-lane wave, say). The subgroup size is
/// still pinned; only the flag is left off.
pub const NO_FULL_SUBGROUPS_GROUP: &str = "local_size_x is not a multiple of the wave size";

/// The pipeline's side of [`SpirvProgram::wave_size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubgroupPlan {
    /// `requiredSubgroupSize` for
    /// `VkPipelineShaderStageRequiredSubgroupSizeCreateInfo`, which must be
    /// chained onto the shader stage (with `ALLOW_VARYING` off).
    pub required_subgroup_size: u32,
    /// Whether `VK_PIPELINE_SHADER_STAGE_CREATE_REQUIRE_FULL_SUBGROUPS_BIT` can
    /// be set: every subgroup complete, as a console wave is.
    pub require_full_subgroups: bool,
    /// Why it cannot, when it cannot.
    pub no_full_subgroups: Option<&'static str>,
}

/// Emits the SPIR-V module of a compute program.
pub fn emit(native: &NativeProgram, p: &Program, io: &Io) -> Result<SpirvProgram, ShaderError> {
    if native.stage != HwStage::Cs {
        return Err(Gap("a non-compute stage").into());
    }
    let StageIo::Cs(cs) = native.io else {
        return Err(Gap("compute code without compute stage data").into());
    };
    let local_size = [
        cs.threads[0].max(1),
        cs.threads[1].max(1),
        cs.threads[2].max(1),
    ];
    let wave_size = wave_size(native);
    let mut e = Emitter::new(p, io, cs.lds_bytes() / 4, local_size);
    e.build()?;
    let mut pre = std::mem::take(&mut e.m.words);
    let mut consts = std::mem::take(&mut e.consts);
    e.function()?;
    consts.extend(std::mem::take(&mut e.consts));
    pre.extend_from_slice(&consts);
    pre.extend_from_slice(&e.m.words);
    e.m.words = pre;
    Ok(SpirvProgram {
        words: e.m.finish(),
        wave_size,
        local_size,
    })
}

/// The console wave size the program runs with, 32 or 64: the ISA record's
/// `wave32` flag, as [`super::Psv::wave_size`] passes it on for DXIL.
fn wave_size(native: &NativeProgram) -> u32 {
    if native.wave64 { 64 } else { 32 }
}

/// Whether the program reads other lanes of its wave: DPP source modifiers,
/// lane reads and writes, `v_mbcnt`, `ds_swizzle` and `ds_(b)permute`. Those
/// are the programs a host subgroup must line up with the console's wave for,
/// and the ones that need the group-non-uniform capabilities declared.
fn crosses_lanes(p: &Program) -> bool {
    p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
        let n = i.name();
        n.starts_with("ds_swizzle")
            || n.contains("permute")
            || n.starts_with("v_mbcnt")
            || matches!(n, "v_readlane_b32" | "v_writelane_b32")
            || matches!(&i.fields, Fields::Valu(v) if v.dpp.is_some())
    })
}

impl<'a> Emitter<'a> {
    fn new(
        p: &'a Program,
        io: &'a Io,
        lds_words: u32,
        local_size: [u32; 3],
    ) -> Emitter<'a> {
        let mut m = M::new();
        let mut ids = Vec::new();
        for _ in 0..23 {
            ids.push(m.id());
        }
        let [
            t_void,
            t_bool,
            t_i32,
            t_i64,
            t_f32,
            t_f16,
            t_v3u32,
            pf_i32,
            pf_i64,
            pf_f32,
            ps32,
            ps64,
            psf32,
            pw32,
            p_pc,
            t_arr,
            t_struct,
            pfn,
            t_i16,
            pf_bool,
            t_u16,
            t_u32,
            t_u64,
        ] = ids[..23]
        else {
            unreachable!()
        };
        let p_in_v3 = m.id();
        let pc = m.id();
        let lds = m.id();
        let ext = m.id();
        let zero = m.id();
        let one = m.id();
        let minus_one = m.id();
        let scope_device = m.id();
        let scope_workgroup = m.id();
        let sem = m.id();
        let pc_var = m.id();
        let loop_head = m.id();
        let dispatch = m.id();
        let cont = m.id();
        let sel_merge = m.id();
        let exit = m.id();
        let _arr_len = m.id();
        let tid_var = m.id();
        let gid_var = m.id();
        let main = m.id();
        let p_pc_i32 = m.id();
        let p_arr = m.id();
        let t_lds = m.id();
        // The image and sampler sites (see `ImgBind`), planned before any
        // type is emitted so declarations and uses agree.
        let mut images: Vec<ImgBind> = Vec::new();
        let mut samplers: Vec<(u8, u32)> = Vec::new();
        // (console dim, storage, image type, sampled-image type, pointer) per
        // distinct image type the sites need.
        let mut shared: Vec<(u8, bool, u32, u32, u32)> = Vec::new();
        for b in &p.blocks {
            for i in &b.insts {
                let Fields::Image {
                    rsrc,
                    sampler,
                    dim,
                    ..
                } = &i.fields
                else {
                    continue;
                };
                let Operand::Scalar { code: rsrc, .. } = *rsrc else {
                    continue;
                };
                let name = i.name();
                let storage =
                    name.starts_with("image_store") || name.starts_with("image_atomic");
                if spv_dim(*dim).is_some()
                    && !images
                        .iter()
                        .any(|s| s.rsrc == rsrc && s.dim == *dim && s.storage == storage)
                {
                    // One type per (dimension, kind): the module may not
                    // declare the same image type twice.
                    let ty = match shared.iter().find(|t| t.0 == *dim && t.1 == storage) {
                        Some(t) => (t.2, t.3, t.4),
                        None => {
                            let t = (m.id(), m.id(), m.id());
                            shared.push((*dim, storage, t.0, t.1, t.2));
                            t
                        }
                    };
                    images.push(ImgBind {
                        rsrc,
                        dim: *dim,
                        storage,
                        ty: ty.0,
                        si_ty: ty.1,
                        pty: ty.2,
                        var: m.id(),
                    });
                }
                if let Some(Operand::Scalar { code, .. }) = *sampler
                    && !samplers.iter().any(|s| s.0 == code)
                {
                    samplers.push((code, m.id()));
                }
            }
        }
        let t_smp = m.id();
        let p_smp = m.id();
        let mut vecs = Vec::new();
        for _ in 0..5 {
            vecs.push(m.id());
        }
        let [v2i, v4i, v2f, v3f, v4f] = vecs[..] else {
            unreachable!()
        };
        Emitter {
            m,
            p,
            io,
            t_void,
            t_bool,
            t_i32,
            t_i64,
            t_f32,
            t_f16,
            t_v3u32,
            pf_i32,
            pf_i64,
            pf_f32,
            ps32,
            ps64,
            psf32,
            pw32,
            p_pc,
            p_pc_i32,
            p_arr,
            t_lds,
            t_arr,
            t_struct,
            t_i16,
            t_u16,
            t_u32,
            t_u64,
            pf_bool,
            pfn,
            p_in_v3,
            pc,
            lds,
            ext,
            main,
            tid_var: Some(tid_var),
            gid_var: Some(gid_var),
            regs: Vec::new(),
            scc: 0,
            zero,
            one,
            minus_one,
            ci: HashMap::new(),
            ci64: HashMap::new(),
            consts: Vec::new(),
            pc_var,
            loop_head,
            dispatch,
            cont,
            sel_merge,
            exit,
            labels: Vec::new(),
            lds_words,
            local_size,
            scope_device,
            scope_workgroup,
            sem_acq_rel_workgroup: sem,
            images,
            samplers,
            src_patch: Vec::new(),
            sdwa_dst: None,
            t_smp,
            p_smp,
            v2i,
            v3i: t_v3u32,
            v4i,
            v2f,
            v3f,
            v4f,
        }
    }

    // ---- small helpers ------------------------------------------------------

    fn ins(&mut self, opcode: u16, ty: u32, result: u32, ops: &[u32]) {
        self.m.ins(opcode, ty, result, ops);
    }

    fn c32(&mut self, v: u32) -> u32 {
        if v == 0 {
            return self.zero;
        }
        if v == 1 {
            return self.one;
        }
        if v == 0xffff_ffff {
            return self.minus_one;
        }
        if let Some(&id) = self.ci.get(&v) {
            return id;
        }
        let id = self.m.id();
        let at = self.m.words.len();
        self.m.ins(op::CONSTANT, self.t_i32, id, &[v]);
        let new = self.m.words.split_off(at);
        self.consts.extend_from_slice(&new);
        self.ci.insert(v, id);
        id
    }

    fn c64(&mut self, v: u64) -> u32 {
        if let Some(&id) = self.ci64.get(&v) {
            return id;
        }
        let id = self.m.id();
        let at = self.m.words.len();
        self.m
            .ins(op::CONSTANT, self.t_i64, id, &[v as u32, (v >> 32) as u32]);
        let new = self.m.words.split_off(at);
        self.consts.extend_from_slice(&new);
        self.ci64.insert(v, id);
        id
    }

    fn cf32(&mut self, v: f32) -> u32 {
        let bits = self.c32(v.to_bits());
        self.bitcast_f32(bits)
    }

    fn bitcast_f32(&mut self, i: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::BITCAST, self.t_f32, id, &[i]);
        id
    }

    fn bitcast_i32(&mut self, f: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::BITCAST, self.t_i32, id, &[f]);
        id
    }

    fn un(&mut self, opcode: u16, ty: u32, a: u32) -> u32 {

        let id = self.m.id();
        self.ins(opcode, ty, id, &[a]);
        id
    }

    fn bin(&mut self, opcode: u16, ty: u32, a: u32, b: u32) -> u32 {
        let id = self.m.id();
        self.ins(opcode, ty, id, &[a, b]);
        id
    }

    fn tern(&mut self, opcode: u16, ty: u32, a: u32, b: u32, c: u32) -> u32 {
        let id = self.m.id();
        self.ins(opcode, ty, id, &[a, b, c]);
        id
    }

    fn ext1(&mut self, inst: u32, a: u32) -> u32 {
        self.ext1t(inst, self.t_f32, a)
    }

    /// [`ext1`](Self::ext1) with the result type the ext instruction returns.
    fn ext1t(&mut self, inst: u32, ty: u32, a: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::EXT_INST, ty, id, &[self.ext, inst, a]);
        id
    }

    fn ext2(&mut self, inst: u32, ty: u32, a: u32, b: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::EXT_INST, ty, id, &[self.ext, inst, a, b]);
        id
    }

    fn select(&mut self, cond: u32, a: u32, b: u32) -> u32 {
        self.tern(op::SELECT, self.t_i32, cond, a, b)
    }

    fn load(&mut self, slot: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::LOAD, self.t_i32, id, &[self.regs[slot as usize]]);
        id
    }

    fn store(&mut self, slot: u32, v: u32) {
        self.ins(op::STORE, 0, 0, &[self.regs[slot as usize], v]);
    }

    /// The dword `v` at a byte address, through a physical-storage pointer.
    fn load_bda(&mut self, addr: u32) -> u32 {
        let p = self.un(op::CONVERT_U_TO_PTR, self.ps32, addr);
        let id = self.m.id();
        // Physical storage buffer accesses carry their alignment.
        self.ins(op::LOAD, self.t_i32, id, &[p, 2, 4]);
        id
    }

    fn store_bda(&mut self, addr: u32, v: u32) {
        let p = self.un(op::CONVERT_U_TO_PTR, self.ps32, addr);
        self.ins(op::STORE, 0, 0, &[p, v, 2, 4]);
    }

    /// Zero-extends a 32-bit value to the module's i64 currency.
    fn u32_to_u64(&mut self, v: u32) -> u32 {
        let u = self.un(op::BITCAST, self.t_u32, v);
        let w = self.un(op::U_CONVERT, self.t_u64, u);
        self.un(op::BITCAST, self.t_i64, w)
    }

    /// Truncates the i64 currency to 32 bits.
    fn u64_to_u32(&mut self, v: u32) -> u32 {
        let u = self.un(op::BITCAST, self.t_u64, v);
        let w = self.un(op::U_CONVERT, self.t_u32, u);
        self.un(op::BITCAST, self.t_i32, w)
    }

    /// Truncates a 32-bit value to 16 bits (an unsigned half).
    fn u32_to_u16(&mut self, v: u32) -> u32 {
        let u = self.un(op::BITCAST, self.t_u32, v);
        self.un(op::U_CONVERT, self.t_u16, u)
    }

    /// Zero-extends a 16-bit value to the i32 currency.
    fn u16_to_u32(&mut self, v: u32) -> u32 {
        let w = self.un(op::U_CONVERT, self.t_u32, v);
        self.un(op::BITCAST, self.t_i32, w)
    }

    fn sext64(&mut self, v: u32) -> u32 {
        self.un(op::S_CONVERT, self.t_i64, v)
    }

    fn add64(&mut self, a: u32, b: u32) -> u32 {
        self.bin(op::I_ADD, self.t_i64, a, b)
    }

    /// The i32 sum of some values, wrapping.
    fn sum32(&mut self, parts: &[u32]) -> u32 {
        match parts {
            [] => self.zero,
            [a] => *a,
            [a, rest @ ..] => {
                let b = self.sum32(rest);
                self.bin(op::I_ADD, self.t_i32, *a, b)
            }
        }
    }

    /// The i64 value of the register pair at `start`.
    fn pair64(&mut self, start: u32) -> u32 {
        let (lo, hi) = (self.load(start), self.load(start + 1));
        let lo = self.u32_to_u64(lo);
        let hi = self.u32_to_u64(hi);
        let shift = self.c64(32);
        let hi = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i64, hi, shift);
        self.bin(op::BITWISE_OR, self.t_i64, lo, hi)
    }

    fn write_pair64(&mut self, start: u32, v: u32) {
        let lo = self.u64_to_u32(v);
        let shift = self.c64(32);
        let hi = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i64, v, shift);
        let hi = self.u64_to_u32(hi);
        self.store(start, lo);
        self.store(start + 1, hi);
    }

    /// The value as an unsigned 32-bit integer (same width).
    fn as_u32(&mut self, v: u32) -> u32 {
        self.un(op::BITCAST, self.t_u32, v)
    }

    /// The value as a signed 32-bit integer (same width).
    fn as_s32(&mut self, v: u32) -> u32 {
        self.un(op::BITCAST, self.t_i32, v)
    }

    fn is_zero32(&mut self, v: u32) -> u32 {
        self.bin(op::I_EQUAL, self.t_bool, v, self.zero)
    }

    fn logical_not(&mut self, v: u32) -> u32 {
        self.un(op::LOGICAL_NOT, self.t_bool, v)
    }

    /// Where a source operand's first dword lives, and how many dwords it has.
    fn op_regs(&self, o: &Operand) -> R<(u32, u8)> {
        match *o {
            Operand::Scalar { code: 125, .. } => gap("a null register read"),
            Operand::Scalar { code, dwords } => Ok((u32::from(code), dwords)),
            Operand::Vector { reg, dwords } => Ok((VGPR_BASE + u32::from(reg), dwords)),
            _ => gap("a source that is not a register"),
        }
    }

    /// A 32-bit source as an i32 value.
    fn src32(&mut self, o: &Operand) -> R<u32> {
        if let Some((_, v)) = self.src_patch.iter().find(|(p, _)| p == o) {
            return Ok(*v);
        }
        match *o {
            Operand::Scalar { code: 125, .. } => Ok(self.zero),
            Operand::Scalar { code, .. } => Ok(self.load(u32::from(code))),
            Operand::Vector { reg, .. } => Ok(self.load(VGPR_BASE + u32::from(reg))),
            Operand::Const(code) => self.const_operand(code),
            Operand::Literal(v) => Ok(self.c32(v)),
            _ => gap("a source that is not a scalar or vector register"),
        }
    }

    /// A 32-bit source as an f32 value.
    fn srcf32(&mut self, o: &Operand, neg: bool, abs: bool) -> R<u32> {
        let bits = self.src32(o)?;
        let mut f = self.bitcast_f32(bits);
        if abs {
            f = self.ext1(glsl::FABS, f);
        }
        if neg {
            f = self.un(op::F_NEGATE, self.t_f32, f);
        }
        Ok(f)
    }

    fn const_operand(&mut self, code: u8) -> R<u32> {
        match code {
            128..=192 => Ok(self.c32(u32::from(code - 128))),
            193..=208 => Ok(self.c32(0u32.wrapping_sub(u32::from(code - 192)))),
            240 => Ok(self.c32(0.5f32.to_bits())),
            241 => Ok(self.c32((-0.5f32).to_bits())),
            242 => Ok(self.c32(1.0f32.to_bits())),
            243 => Ok(self.c32((-1.0f32).to_bits())),
            244 => Ok(self.c32(2.0f32.to_bits())),
            245 => Ok(self.c32((-2.0f32).to_bits())),
            246 => Ok(self.c32(4.0f32.to_bits())),
            247 => Ok(self.c32((-4.0f32).to_bits())),
            248 => Ok(self.c32(std::f32::consts::FRAC_1_PI.to_bits())),
            249 => Ok(self.c32(std::f32::consts::FRAC_1_PI.to_bits())),
            251..=253 => {
                // vccz / execz / scc as a number.
                let id = self.condition_id(code)?;
                let one = self.one;
                let zero = self.zero;
                Ok(self.tern(op::SELECT, self.t_i32, id, one, zero))
            }
            254 => Ok(self.zero),
            _ => gap("a source constant this emitter does not know"),
        }
    }

    fn condition_id(&mut self, code: u8) -> R<u32> {
        match code {
            251 => {
                let lo = self.load(106);
                let hi = self.load(107);
                let o = self.bin(op::BITWISE_OR, self.t_i32, lo, hi);
                Ok(self.is_zero32(o))
            }
            252 => {
                let lo = self.load(126);
                let hi = self.load(127);
                let o = self.bin(op::BITWISE_OR, self.t_i32, lo, hi);
                Ok(self.is_zero32(o))
            }
            253 => Ok(self.load_scc()),
            _ => gap("a condition this emitter does not know"),
        }
    }

    fn load_scc(&mut self) -> u32 {
        let id = self.m.id();
        self.ins(op::LOAD, self.t_bool, id, &[self.scc]);
        id
    }

    fn store_scc(&mut self, v: u32) {
        self.ins(op::STORE, 0, 0, &[self.scc, v]);
    }

    /// Compares two values with `pred`, giving an i1.
    fn cmp(&mut self, pred: u16, a: u32, b: u32) -> u32 {
        self.bin(pred, self.t_bool, a, b)
    }

    /// Writes an i32 value into an integer destination.
    fn wdst(&mut self, dst: &Operand, v: u32) -> R<()> {
        if is_null(dst) {
            return Ok(());
        }
        let (start, _) = self.op_regs(dst)?;
        let v = match self.sdwa_dst.take() {
            Some(sdwa) => self.sdwa_dst_value(start, v, sdwa),
            None => v,
        };
        self.store(start, v);
        Ok(())
    }

    /// Writes an f32 value into a destination, bitcast to its dwords.
    fn wdst_f32(&mut self, dst: &Operand, f: u32) -> R<()> {
        let i = self.bitcast_i32(f);
        self.wdst(dst, i)
    }

    /// Writes a 64-bit value into a pair destination.
    fn wdst64(&mut self, dst: &Operand, v: u32) -> R<()> {
        if is_null(dst) {
            return Ok(());
        }
        let (start, _) = self.op_regs(dst)?;
        self.write_pair64(start, v);
        Ok(())
    }

    /// Reads the i64 pair a 64-bit source operand names.
    fn src64(&mut self, o: &Operand) -> R<u32> {
        match *o {
            Operand::Scalar { code: 125, .. } => Ok(self.c64(0)),
            Operand::Scalar { code, .. } => Ok(self.pair64(u32::from(code))),
            Operand::Vector { reg, .. } => Ok(self.pair64(VGPR_BASE + u32::from(reg))),
            Operand::Const(code) => {
                let v = u64::from(self.const_operand(code)?);
                Ok(self.c64(v))
            }
            Operand::Literal(v) => Ok(self.c64(u64::from(v))),
            _ => gap("a 64-bit source that is not a register"),
        }
    }

    fn valu_mods(v: &crate::gfx10::Valu) -> R<()> {
        // SDWA and `opsel` are translated (`valu_patch`). DPP reads other
        // lanes of the wave, which this emitter does not model one by one
        // yet: it is taken as the identity, like the lane shortcuts below.
        let _ = v;
        Ok(())
    }

    // ---- the module --------------------------------------------------------

    fn build(&mut self) -> R<()> {
        let [ls_x, ls_y, ls_z] = self.local_size;
        // Header.
        self.m.ins(op::CAPABILITY, 0, 0, &[1]); // Shader
        self.m.ins(op::CAPABILITY, 0, 0, &[5347]); // PhysicalStorageBufferAddresses
        self.m.ins(op::CAPABILITY, 0, 0, &[11]); // Int64
        self.m.ins(op::CAPABILITY, 0, 0, &[22]); // Int16
        self.m.ins(op::CAPABILITY, 0, 0, &[9]); // Float16
        // Image bindings: what their dimensions and formats need.
        let mut caps: Vec<u32> = Vec::new();
        for b in &self.images {
            if b.dim == 0 {
                caps.extend([cap::SAMPLED_1D, cap::IMAGE_1D]);
            }
            if b.storage {
                // No format in the module: the view's format converts, as the
                // console's texture unit does.
                caps.extend([cap::STORAGE_READ_WITHOUT_FORMAT, cap::STORAGE_WRITE_WITHOUT_FORMAT]);
            }
        }
        if !self.images.is_empty() {
            caps.push(cap::IMAGE_QUERY);
        }
        caps.sort_unstable();
        caps.dedup();
        // Cross-lane code (DPP, lane reads, DPP row ops) runs on the console's
        // own wave: the pipeline pins the subgroup to `wave_size`, and the
        // module declares the group-non-uniform capabilities it will use.
        if crosses_lanes(self.p) {
            caps.push(cap::GROUP_NON_UNIFORM);
            caps.push(cap::GROUP_NON_UNIFORM_SHUFFLE);
        }
        caps.sort_unstable();
        caps.dedup();
        for c in caps {
            self.m.ins(op::CAPABILITY, 0, 0, &[c]);
        }
        self.m
            .ins_str(op::EXTENSION, 0, 0, &[], "SPV_KHR_physical_storage_buffer");
        self.m
            .ins_str(op::EXT_INST_IMPORT, 0, self.ext, &[], "GLSL.std.450");
        // Addressing model, then memory model: PhysicalStorageBuffer64, GLSL450.
        self.m.ins(op::MEMORY_MODEL, 0, 0, &[5348, 1]);
        self.m.ins_str_at(
            op::ENTRY_POINT,
            &[5, self.main],
            "main",
            &{
                let mut iface = vec![
                    self.pc,
                    self.lds,
                    self.tid_var.unwrap(),
                    self.gid_var.unwrap(),
                ];
                iface.extend(self.images.iter().map(|b| b.var));
                iface.extend(self.samplers.iter().map(|s| s.1));
                iface
            },
        );
        self.m
            .ins(op::EXECUTION_MODE, 0, 0, &[self.main, 17, ls_x, ls_y, ls_z]);
        // Decorations.
        self.m
            .ins(op::DECORATE, 0, 0, &[self.t_struct, decoration::BLOCK]);
        self.m.ins(
            op::MEMBER_DECORATE,
            0,
            0,
            &[self.t_struct, 0, decoration::OFFSET, 0],
        );
        self.m.ins(
            op::DECORATE,
            0,
            0,
            &[self.t_arr, decoration::ARRAY_STRIDE, 4],
        );
        self.m.ins(
            op::DECORATE,
            0,
            0,
            &[
                self.tid_var.unwrap(),
                decoration::BUILT_IN,
                builtin::LOCAL_INVOCATION_ID,
            ],
        );
        self.m.ins(
            op::DECORATE,
            0,
            0,
            &[
                self.gid_var.unwrap(),
                decoration::BUILT_IN,
                builtin::WORKGROUP_ID,
            ],
        );
        // One binding per image and sampler site (DescriptorSet 0).
        for (k, b) in self.images.iter().enumerate() {
            self.m
                .ins(op::DECORATE, 0, 0, &[b.var, decoration::DESCRIPTOR_SET, 0]);
            self.m
                .ins(op::DECORATE, 0, 0, &[b.var, decoration::BINDING, k as u32]);
        }
        let base = self.images.len() as u32;
        for (k, s) in self.samplers.iter().enumerate() {
            self.m
                .ins(op::DECORATE, 0, 0, &[s.1, decoration::DESCRIPTOR_SET, 0]);
            self.m
                .ins(op::DECORATE, 0, 0, &[s.1, decoration::BINDING, base + k as u32]);
        }
        // Types.
        self.m.ins(op::TYPE_VOID, 0, self.t_void, &[]);
        self.m.ins(op::TYPE_BOOL, 0, self.t_bool, &[]);
        self.m.ins(op::TYPE_INT, 0, self.t_i32, &[32, 1]);
        self.m.ins(op::TYPE_INT, 0, self.t_i64, &[64, 1]);
        self.m.ins(op::TYPE_FLOAT, 0, self.t_f32, &[32]);
        self.m.ins(op::TYPE_FLOAT, 0, self.t_f16, &[16]);
        self.m.ins(op::TYPE_INT, 0, self.t_i16, &[16, 1]);
        self.m.ins(op::TYPE_INT, 0, self.t_u16, &[16, 0]);
        self.m.ins(op::TYPE_INT, 0, self.t_u32, &[32, 0]);
        self.m.ins(op::TYPE_INT, 0, self.t_u64, &[64, 0]);
        self.m
            .ins(op::TYPE_VECTOR, 0, self.t_v3u32, &[self.t_i32, 3]);
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.pf_i32,
            &[storage::FUNCTION, self.t_i32],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.pf_i64,
            &[storage::FUNCTION, self.t_i64],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.pf_f32,
            &[storage::FUNCTION, self.t_f32],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.pf_bool,
            &[storage::FUNCTION, self.t_bool],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.ps32,
            &[storage::PHYSICAL_STORAGE_BUFFER, self.t_i32],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.ps64,
            &[storage::PHYSICAL_STORAGE_BUFFER, self.t_i64],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.psf32,
            &[storage::PHYSICAL_STORAGE_BUFFER, self.t_f32],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.pw32,
            &[storage::WORKGROUP, self.t_i32],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.p_in_v3,
            &[storage::INPUT, self.t_v3u32],
        );
        let alen = self.arr_len();
        self.m
            .ins(op::TYPE_ARRAY, 0, self.t_arr, &[self.t_i32, alen]);
        // Images: the sampled type is f32, the format Unknown, so the view's
        // format does the console's conversion (`Sampled` 1 read, 2 written).
        let mut done: Vec<u32> = Vec::new();
        for b in &self.images {
            if done.contains(&b.ty) {
                continue;
            }
            done.push(b.ty);
            let (dim, arrayed) = spv_dim(b.dim).map(|d| (d.0, d.1)).unwrap_or((1, false));
            self.m.ins(
                op::TYPE_IMAGE,
                0,
                b.ty,
                &[
                    self.t_f32,
                    dim,
                    0,
                    u32::from(arrayed),
                    0,
                    if b.storage { 2 } else { 1 },
                    0,
                ],
            );
        }
        self.m.ins(op::TYPE_SAMPLER, 0, self.t_smp, &[]);
        let mut done: Vec<u32> = Vec::new();
        for b in &self.images {
            if done.contains(&b.ty) {
                continue;
            }
            done.push(b.ty);
            if !b.storage {
                self.m.ins(op::TYPE_SAMPLED_IMAGE, 0, b.si_ty, &[b.ty]);
            }
            self.m
                .ins(op::TYPE_POINTER, 0, b.pty, &[storage::UNIFORM_CONSTANT, b.ty]);
        }
        if !self.samplers.is_empty() {
            self.m
                .ins(op::TYPE_POINTER, 0, self.p_smp, &[storage::UNIFORM_CONSTANT, self.t_smp]);
        }
        let vecs = [
            (self.t_i32, 2, self.v2i),
            (self.t_i32, 4, self.v4i),
            (self.t_f32, 2, self.v2f),
            (self.t_f32, 3, self.v3f),
            (self.t_f32, 4, self.v4f),
        ];
        for (comp, n, id) in vecs {
            self.m.ins(op::TYPE_VECTOR, 0, id, &[comp, n]);
        }
        self.m.ins(op::TYPE_STRUCT, 0, self.t_struct, &[self.t_arr]);
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.p_pc,
            &[storage::PUSH_CONSTANT, self.t_struct],
        );
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.p_pc_i32,
            &[storage::PUSH_CONSTANT, self.t_i32],
        );
        // A constant the array type refers to must precede it, so it cannot
        // come from the deferred constant section.
        let lds_len = self.m.id();
        let words = self.lds_words.max(1);
        self.m.ins(op::CONSTANT, self.t_i32, lds_len, &[words]);
        self.m
            .ins(op::TYPE_ARRAY, 0, self.t_lds, &[self.t_i32, lds_len]);
        self.m.ins(
            op::TYPE_POINTER,
            0,
            self.p_arr,
            &[storage::WORKGROUP, self.t_lds],
        );
        self.m.ins(op::TYPE_FUNCTION, 0, self.pfn, &[self.t_void]);
        self.m.ins(op::CONSTANT, self.t_i32, self.zero, &[0]);
        self.m.ins(op::CONSTANT, self.t_i32, self.one, &[1]);
        self.m
            .ins(op::CONSTANT, self.t_i32, self.minus_one, &[0xffff_ffff]);
        self.m
            .ins(op::CONSTANT, self.t_i32, self.scope_device, &[1]);
        self.m
            .ins(op::CONSTANT, self.t_i32, self.scope_workgroup, &[2]);
        self.m.ins(
            op::CONSTANT,
            self.t_i32,
            self.sem_acq_rel_workgroup,
            &[0x108],
        );
        // Global variables.
        self.m
            .ins(op::VARIABLE, self.p_pc, self.pc, &[storage::PUSH_CONSTANT]);
        self.m
            .ins(op::VARIABLE, self.p_arr, self.lds, &[storage::WORKGROUP]);
        self.m.ins(
            op::VARIABLE,
            self.p_in_v3,
            self.tid_var.unwrap(),
            &[storage::INPUT],
        );
        self.m.ins(
            op::VARIABLE,
            self.p_in_v3,
            self.gid_var.unwrap(),
            &[storage::INPUT],
        );
        for b in &self.images {
            self.m
                .ins(op::VARIABLE, b.pty, b.var, &[storage::UNIFORM_CONSTANT]);
        }
        for s in &self.samplers {
            self.m
                .ins(op::VARIABLE, self.p_smp, s.1, &[storage::UNIFORM_CONSTANT]);
        }
        // One function-local variable per register dword, the pc, and SCC.
        let mut regs = Vec::with_capacity(SLOTS as usize);
        for _ in 0..SLOTS {
            regs.push(self.m.id());
        }
        self.scc = self.m.id();
        let pc_var = self.pc_var;
        self.regs = regs;
        self.pc_var = pc_var;
        Ok(())
    }

    fn arr_len(&mut self) -> u32 {
        let n = self.m.id();
        self.m.ins(op::CONSTANT, self.t_i32, n, &[32]);
        n
    }

    /// The function body: variables, input registers, the dispatcher loop and
    /// one block per IR block.
    fn function(&mut self) -> R<()> {
        let main = self.main;
        self.labels = (0..self.p.blocks.len()).map(|_| self.m.id()).collect();
        self.ins(op::FUNCTION, self.t_void, main, &[0, self.pfn]);
        let entry = self.m.id();
        self.ins(op::LABEL, 0, entry, &[]);
        for i in 0..SLOTS {
            let v = self.regs[i as usize];
            self.ins(op::VARIABLE, self.pf_i32, v, &[storage::FUNCTION]);
        }
        let scc = self.scc;
        let pf_bool = self.pf_bool;
        self.ins(op::VARIABLE, pf_bool, scc, &[storage::FUNCTION]);
        let pc_var = self.pc_var;
        self.ins(op::VARIABLE, self.pf_i32, pc_var, &[storage::FUNCTION]);
        // EXEC starts all ones, as it does on the machine.
        let m1 = self.minus_one;
        self.store(126, m1);
        self.store(127, m1);
        self.store(self.pc_var, self.zero);
        self.init_user_data()?;
        self.init_inputs()?;
        self.ins(op::BRANCH, 0, 0, &[self.loop_head]);
        // The dispatcher loop.
        self.ins(op::LABEL, 0, self.loop_head, &[]);
        self.ins(op::LOOP_MERGE, 0, 0, &[self.exit, self.cont, 0]);
        self.ins(op::BRANCH, 0, 0, &[self.dispatch]);
        self.ins(op::LABEL, 0, self.dispatch, &[]);
        let pcv = self.m.id();
        self.ins(op::LOAD, self.t_i32, pcv, &[self.pc_var]);
        self.ins(op::SELECTION_MERGE, 0, 0, &[self.sel_merge, 0]);
        // Default: the select's merge (an unreachable pc), not the loop exit:
        // a case target must live inside the switch's construct.
        let mut sw = vec![pcv, self.sel_merge];
        for (b, label) in self.labels.clone().iter().enumerate() {
            sw.push(b as u32);
            sw.push(*label);
        }
        self.ins(op::SWITCH, 0, 0, &sw);
        // One block per IR block.
        for b in 0..self.p.blocks.len() {
            let label = self.labels[b];
            self.ins(op::LABEL, 0, label, &[]);
            let n = self.p.blocks[b].insts.len();
            for i in 0..n {
                let inst = self.p.blocks[b].insts[i].clone();
                self.inst(&inst)?;
            }
            let exit = self.p.blocks[b].exit;
            self.exit_branch(b, exit)?;
        }
        self.ins(op::LABEL, 0, self.sel_merge, &[]);
        self.ins(op::BRANCH, 0, 0, &[self.cont]);
        self.ins(op::LABEL, 0, self.cont, &[]);
        self.ins(op::BRANCH, 0, 0, &[self.loop_head]);
        self.ins(op::LABEL, 0, self.exit, &[]);
        self.ins(op::RETURN, 0, 0, &[]);
        self.ins(op::FUNCTION_END, 0, 0, &[]);
        Ok(())
    }

    // ---- memory -------------------------------------------------------------

    fn load_dwords(&mut self, start: u32, n: u32, addr: u32) {
        for k in 0..n {
            let a = if k == 0 {
                addr
            } else {
                let d = self.c64(4 * u64::from(k));
                self.add64(addr, d)
            };
            let v = self.load_bda(a);
            self.store(start + k, v);
        }
    }

    fn store_dwords(&mut self, start: u32, n: u32, addr: u32) {
        for k in 0..n {
            let a = if k == 0 {
                addr
            } else {
                let d = self.c64(4 * u64::from(k));
                self.add64(addr, d)
            };
            let v = self.load(start + k);
            self.store_bda(a, v);
        }
    }

    /// A byte address: a 64-bit base plus a 32-bit signed offset.
    fn mem_addr(&mut self, base: u32, parts: &[u32]) -> u32 {
        let off = self.sum32(parts);
        let off = self.sext64(off);
        self.add64(base, off)
    }

    fn smem(&mut self, i: &Inst) -> R<()> {
        let Fields::Smem {
            data,
            base,
            offset,
            soffset,
            ..
        } = &i.fields
        else {
            return gap("a scalar memory instruction without its fields");
        };
        let (data, base, offset, soffset) = (*data, *base, *offset, *soffset);
        let base = self.src64(&base)?;
        let mut parts = vec![self.c32(offset as u32)];
        if !is_null(&soffset) {
            let s = self.src32(&soffset)?;
            parts.push(s);
        }
        let addr = self.mem_addr(base, &parts);
        let data = data.ok_or(Gap("a scalar memory instruction without data"))?;
        let (start, n) = self.op_regs(&data)?;
        if i.name().contains("store") {
            self.store_dwords(start, u32::from(n), addr);
        } else {
            self.load_dwords(start, u32::from(n), addr);
        }
        Ok(())
    }

    fn mubuf(&mut self, i: &Inst) -> R<()> {
        let Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &i.fields
        else {
            return gap("a buffer instruction without its fields");
        };
        if format.is_some() {
            return gap("a typed buffer access");
        }
        if *lds {
            return gap("a buffer access through LDS");
        }
        if *tfe {
            return gap("a buffer access with a fetch index");
        }
        let base = self.src64(rsrc)?;
        let mut parts = vec![self.c32(u32::from(*offset))];
        if *idxen {
            let eight = self.c64(8);
            let at = self.add64(base, eight);
            let stride = self.load_bda(at);
            let idx = self.src32(addr)?;
            let step = self.bin(op::I_MUL, self.t_i32, idx, stride);
            parts.push(step);
        }
        if *offen {
            let v = self.src32(addr)?;
            parts.push(v);
        }
        if !is_null(soffset) {
            let s = self.src32(soffset)?;
            parts.push(s);
        }
        let addr64 = self.mem_addr(base, &parts);
        let data = (*data).ok_or(Gap("a buffer access without data"))?;
        let (start, n) = self.op_regs(&data)?;
        let name = i.name();
        if name.contains("atomic") {
            self.buffer_atomic(name, start, n, addr64)
        } else if name.contains("store") {
            self.store_dwords(start, u32::from(n), addr64);
            Ok(())
        } else {
            self.load_dwords(start, u32::from(n), addr64);
            Ok(())
        }
    }

    fn buffer_atomic(&mut self, name: &str, start: u32, n: u8, addr: u32) -> R<()> {
        let code = match name {
            "buffer_atomic_add" | "buffer_atomic_add_x2" => op::ATOMIC_I_ADD,
            "buffer_atomic_sub" | "buffer_atomic_sub_x2" => op::ATOMIC_I_SUB,
            "buffer_atomic_smin" => op::ATOMIC_S_MIN,
            "buffer_atomic_umin" => op::ATOMIC_U_MIN,
            "buffer_atomic_smax" => op::ATOMIC_S_MAX,
            "buffer_atomic_umax" => op::ATOMIC_U_MAX,
            "buffer_atomic_and" => op::ATOMIC_AND,
            "buffer_atomic_or" => op::ATOMIC_OR,
            "buffer_atomic_xor" => op::ATOMIC_XOR,
            "buffer_atomic_swap" => op::ATOMIC_EXCHANGE,
            _ => return gap("a buffer atomic this emitter does not implement"),
        };
        let scope = self.scope_device;
        let sem = self.zero;
        for k in 0..u32::from(n) {
            let a = if k == 0 {
                addr
            } else {
                let d = self.c64(4 * u64::from(k));
                self.add64(addr, d)
            };
            let p = self.un(op::CONVERT_U_TO_PTR, self.ps32, a);
            let v = self.load(start + k);
            let id = self.m.id();
            self.ins(code, self.t_i32, id, &[p, scope, sem, v]);
            self.store(start + k, id);
        }
        Ok(())
    }

    fn flat(&mut self, i: &Inst) -> R<()> {
        let Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            offset,
            lds,
            ..
        } = &i.fields
        else {
            return gap("a flat instruction without its fields");
        };
        if *lds {
            return gap("a scratch access");
        }
        let a = (*addr).ok_or(Gap("a flat access without an address"))?;
        let base = self.src64(&a)?;
        let mut parts = vec![self.c32(i32::from(*offset) as u32)];
        if let Some(s) = *saddr
            && !is_null(&s)
        {
            let v = self.src32(&s)?;
            parts.push(v);
        }
        let addr64 = self.mem_addr(base, &parts);
        let name = i.name();
        if name.contains("atomic") {
            let d = (*data).ok_or(Gap("an atomic without data"))?;
            let (start, n) = self.op_regs(&d)?;
            self.buffer_atomic(name, start, n, addr64)
        } else if name.contains("store") {
            let d = (*data).ok_or(Gap("a store without data"))?;
            let (start, n) = self.op_regs(&d)?;
            self.store_dwords(start, u32::from(n), addr64);
            Ok(())
        } else {
            let d = (*dst).ok_or(Gap("a load without a destination"))?;
            let (start, n) = self.op_regs(&d)?;
            self.load_dwords(start, u32::from(n), addr64);
            Ok(())
        }
    }

    // ---- source and destination modifiers -----------------------------------

    /// A `v_fma_mix` source: its f32 dword, or the f16 half `opsel` names
    /// when `opsel_hi` marks the source as f16.
    fn mix_src(&mut self, v: &crate::gfx10::Valu, k: usize) -> R<u32> {
        let bit = 1u8 << k;
        if v.opsel_hi & bit == 0 {
            return self.srcf32(&v.src[k], v.neg & bit != 0, v.abs & bit != 0);
        }
        let raw = self.src32(&v.src[k])?;
        let mut x = raw;
        if v.opsel & bit != 0 {
            let sh = self.c32(16);
            x = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, x, sh);
        }
        let m = self.c32(0xFFFF);
        x = self.bin(op::BITWISE_AND, self.t_i32, x, m);
        let mut f = self.half_to_f32(x);
        if v.abs & bit != 0 {
            f = self.ext1(glsl::FABS, f);
        }
        if v.neg & bit != 0 || v.neg_hi & bit != 0 {
            f = self.un(op::F_NEGATE, self.t_f32, f);
        }
        Ok(f)
    }

    /// A source dword after SDWA selects one of its fields (a byte or a word,
    /// sign-extended when `sext`).
    fn sdwa_read(&mut self, v: u32, sel: u8, sext: bool) -> u32 {
        let (shift, width) = match sel {
            0..=3 => (8 * u32::from(sel), 8u32),
            4 => (0, 16),
            5 => (16, 16),
            _ => return v,
        };
        let mut x = v;
        if shift != 0 {
            let sh = self.c32(shift);
            x = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, x, sh);
        }
        if width == 8 {
            let m = self.c32(0xFF);
            x = self.bin(op::BITWISE_AND, self.t_i32, x, m);
        }
        if sext {
            let sh = self.c32(32 - width);
            let y = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, x, sh);
            x = self.bin(op::SHIFT_RIGHT_ARITHMETIC, self.t_i32, y, sh);
        }
        x
    }

    /// The dword an SDWA write lands: the selected field of `v`, zero- or
    /// sign-extended to the dword, or inserted into what the destination
    /// holds (`dst_unused` preserve).
    fn sdwa_dst_value(&mut self, slot: u32, v: u32, s: crate::gfx10::Sdwa) -> u32 {
        let (shift, width) = match s.dst_sel {
            0..=3 => (8 * u32::from(s.dst_sel), 8u32),
            4 => (0, 16),
            5 => (16, 16),
            _ => return v,
        };
        let m = self.c32(if width == 8 { 0xFF } else { 0xFFFF });
        let x = self.bin(op::BITWISE_AND, self.t_i32, v, m);
        let x = if s.dst_unused == 1 {
            let sh = self.c32(32 - width);
            let y = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, x, sh);
            self.bin(op::SHIFT_RIGHT_ARITHMETIC, self.t_i32, y, sh)
        } else {
            x
        };
        if s.dst_unused == 2 {
            // Preserve the field's neighbours in the destination dword.
            let old = self.load(slot);
            let off = self.c32(shift);
            let count = self.c32(width);
            let id = self.m.id();
            self.ins(op::BIT_FIELD_INSERT, self.t_i32, id, &[old, x, off, count]);
            return id;
        }
        if shift == 0 {
            return x;
        }
        let sh = self.c32(shift);
        self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, x, sh)
    }

    /// The source patch of an SDWA or `opsel` instruction: the field each of
    /// the first two sources reads.
    fn valu_patch(&mut self, v: &crate::gfx10::Valu, name: &str) -> R<()> {
        let mut patch: Vec<(Operand, u32)> = Vec::new();
        if let Some(sdwa) = v.sdwa {
            for (k, o) in v.src.iter().enumerate().take(2) {
                if patch.iter().any(|(p, _)| p == o) {
                    continue;
                }
                let raw = self.src32(o)?;
                let sel = sdwa.src_sel[k.min(1)];
                patch.push((*o, self.sdwa_read(raw, sel, sdwa.sext >> k & 1 != 0)));
            }
        } else if v.opsel & 3 != 0 && is_half_selectable(name) {
            // VOP3 source half selection: the dword's high half is the source.
            for (k, o) in v.src.iter().enumerate().take(2) {
                if v.opsel >> k & 1 == 0 || patch.iter().any(|(p, _)| p == o) {
                    continue;
                }
                let raw = self.src32(o)?;
                let sh = self.c32(16);
                patch.push((*o, self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, raw, sh)));
            }
        }
        self.src_patch = patch;
        self.sdwa_dst = v.sdwa.filter(|s| s.dst_sel != 6 || s.dst_unused != 0);
        Ok(())
    }

    // ---- images -------------------------------------------------------------

    fn vec_of(&self, comp: u32, n: usize) -> u32 {
        match (comp == self.t_f32, n) {
            (false, 2) => self.v2i,
            (false, 3) => self.v3i,
            (false, 4) => self.v4i,
            (true, 2) => self.v2f,
            (true, 3) => self.v3f,
            (true, 4) => self.v4f,
            _ => comp,
        }
    }

    /// A vector of `parts`, or the one part when there is one.
    fn construct(&mut self, parts: &[u32], comp: u32) -> u32 {
        if parts.len() == 1 {
            return parts[0];
        }
        let ty = self.vec_of(comp, parts.len());
        let id = self.m.id();
        self.ins(op::COMPOSITE_CONSTRUCT, ty, id, parts);
        id
    }

    fn extract(&mut self, comp: u32, v: u32, k: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::COMPOSITE_EXTRACT, comp, id, &[v, k]);
        id
    }

    /// An f16 in the low half of an i32, as f32.
    fn half_to_f32(&mut self, v: u32) -> u32 {
        let u = self.un(op::BITCAST, self.t_u32, v);
        let h = self.un(op::U_CONVERT, self.t_u16, u);
        let f = self.un(op::BITCAST, self.t_f16, h);
        self.un(op::F_CONVERT, self.t_f32, f)
    }

    /// An f32 as f16 bits in the low half of an i32.
    fn f32_to_half(&mut self, f: u32) -> u32 {
        let h = self.un(op::F_CONVERT, self.t_f16, f);
        let u = self.un(op::BITCAST, self.t_u16, h);
        let w = self.un(op::U_CONVERT, self.t_u32, u);
        self.un(op::BITCAST, self.t_i32, w)
    }

    /// The dword an image address slot names, as the i32 currency (with `a16`
    /// the slot is half of a dword, its f16 bits zero-extended).
    fn img_word(&mut self, regs: &[u8], at: (usize, Option<u32>)) -> R<u32> {
        let r = *regs
            .get(at.0)
            .ok_or(Gap("an image address that runs past its VGPRs"))?;
        let v = self.load(slot(Reg::V(r)));
        let Some(h) = at.1 else {
            return Ok(v);
        };
        let sh = self.c32(16 * h);
        let x = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, v, sh);
        let m = self.c32(0xFFFF);
        Ok(self.bin(op::BITWISE_AND, self.t_i32, x, m))
    }

    /// The f32 of an image address slot.
    fn img_f32(&mut self, regs: &[u8], at: (usize, Option<u32>)) -> R<u32> {
        let v = self.img_word(regs, at)?;
        Ok(match at.1 {
            None => self.bitcast_f32(v),
            Some(_) => self.half_to_f32(v),
        })
    }

    /// Loads a descriptor variable (an image, a sampler).
    fn load_binding(&mut self, ty: u32, var: u32) -> u32 {
        let id = self.m.id();
        self.ins(op::LOAD, ty, id, &[var]);
        id
    }

    /// The coordinate vector an image access reads from `regs` at logical
    /// address dword `at`, as dwords or as floats. The arguments are the
    /// address layout the instruction's fields give (coordinates, extras,
    /// gradients, a16): one vector per form, not a settings struct.
    #[allow(clippy::too_many_arguments)]
    fn img_coords(
        &mut self,
        regs: &[u8],
        n: usize,
        at: usize,
        extras: usize,
        grads: usize,
        a16: bool,
        float: bool,
    ) -> R<u32> {
        let mut parts = Vec::with_capacity(n);
        for k in 0..n {
            let place = addr_place(at + k, extras, grads, a16);
            parts.push(if float {
                self.img_f32(regs, place)?
            } else {
                self.img_word(regs, place)?
            });
        }
        Ok(self.construct(&parts, if float { self.t_f32 } else { self.t_i32 }))
    }

    /// The vec4 an `image_store` writes: the data VGPRs' `dmask` channels,
    /// zero where the mask leaves one out.
    fn img_texel(&mut self, data: &Operand, dmask: u8, d16: bool) -> R<u32> {
        let Operand::Vector { reg, .. } = *data else {
            return gap("an image store whose data is not a VGPR range");
        };
        let zero = self.cf32(0.0);
        let mut vals = Vec::with_capacity(4);
        for ch in 0..4u32 {
            if dmask >> ch & 1 == 0 {
                vals.push(zero);
                continue;
            }
            let k = (dmask & ((1 << ch) - 1)).count_ones() as usize;
            let v = if d16 {
                let w = self.load(slot(Reg::V(reg + (k / 2) as u8)));
                let w = if k % 2 == 1 {
                    let sh = self.c32(16);
                    let x = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, w, sh);
                    let m = self.c32(0xFFFF);
                    self.bin(op::BITWISE_AND, self.t_i32, x, m)
                } else {
                    w
                };
                self.half_to_f32(w)
            } else {
                let w = self.load(slot(Reg::V(reg + k as u8)));
                self.bitcast_f32(w)
            };
            vals.push(v);
        }
        Ok(self.construct(&vals, self.t_f32))
    }

    /// Writes an image result's `dmask` channels into its data VGPRs (the
    /// channels the mask leaves out keep their value, as on the console).
    fn img_result(&mut self, data: &Operand, dmask: u8, res: u32, d16: bool) -> R<()> {
        let Operand::Vector { reg, .. } = *data else {
            return gap("an image result that is not a VGPR range");
        };
        let mask = if dmask == 0 { 1 } else { dmask };
        let chans: Vec<u32> = (0..4u32).filter(|c| mask >> c & 1 != 0).collect();
        if d16 {
            // Two f16 channels per dword, low half first.
            for (pair, chunk) in chans.chunks(2).enumerate() {
                let lo = self.extract(self.t_f32, res, chunk[0]);
                let lo = self.f32_to_half(lo);
                let w = match chunk.get(1) {
                    None => lo,
                    Some(&hi) => {
                        let hi = self.extract(self.t_f32, res, hi);
                        let hi = self.f32_to_half(hi);
                        let sh = self.c32(16);
                        let hi = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, hi, sh);
                        self.bin(op::BITWISE_OR, self.t_i32, lo, hi)
                    }
                };
                self.store(slot(Reg::V(reg + pair as u8)), w);
            }
            return Ok(());
        }
        for (k, &ch) in chans.iter().enumerate() {
            let f = self.extract(self.t_f32, res, ch);
            let v = self.bitcast_i32(f);
            self.store(slot(Reg::V(reg + k as u8)), v);
        }
        Ok(())
    }

    /// `image_load`, `image_store`, `image_get_resinfo`, `image_sample*` and
    /// `image_gather4*`: the descriptor is a binding (see `ImgBind`), the
    /// coordinates and the data are the console's address and data VGPRs.
    fn image(&mut self, i: &Inst) -> R<()> {
        let name = i.name();
        let Fields::Image {
            data,
            addr,
            rsrc,
            sampler,
            dmask,
            dim,
            unorm,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = &i.fields
        else {
            return gap("an image instruction without image fields");
        };
        let (data, addr) = (*data, addr.clone());
        let (dmask, dim, unorm, a16, d16) = (*dmask, *dim, *unorm, *a16, *d16);
        if *tfe || *lwe {
            return gap("an image access whose LOD or status returns in a VGPR");
        }
        if name.starts_with("image_atomic") {
            return gap("an image atomic");
        }
        if name == "image_get_lod" {
            return gap("an image level-of-detail query");
        }
        let (_, arrayed, coords) = spv_dim(dim).ok_or(Gap("an image dimension (MSAA or unknown)"))?;
        let storage = name.starts_with("image_store");
        let Operand::Scalar { code: rbase, .. } = *rsrc else {
            return gap("an image descriptor that is not a register");
        };
        let b = self
            .images
            .iter()
            .find(|b| b.rsrc == rbase && b.dim == dim && b.storage == storage)
            .copied()
            .ok_or(Gap("an image site outside the plan"))?;
        // The address VGPRs in order (the NSA form is one per address).
        let mut regs = Vec::new();
        for a in &addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        if storage {
            let img = self.load_binding(b.ty, b.var);
            let coord = self.img_coords(&regs, coords, 0, 0, 0, a16, false)?;
            let texel = self.img_texel(&data, dmask, d16)?;
            let mut ops = vec![img, coord, texel];
            if name.contains("_mip") {
                let mip = self.img_word(&regs, addr_place(coords, 0, 0, a16))?;
                ops.extend([0x2, mip]);
            }
            self.ins(op::IMAGE_WRITE, 0, 0, &ops);
            return Ok(());
        }
        if name.starts_with("image_load") {
            let img = self.load_binding(b.ty, b.var);
            let coord = self.img_coords(&regs, coords, 0, 0, 0, a16, false)?;
            let mut ops = vec![img, coord];
            if name.contains("_mip") {
                let mip = self.img_word(&regs, addr_place(coords, 0, 0, a16))?;
                ops.extend([0x2, mip]);
            }
            let res = self.m.id();
            self.ins(op::IMAGE_FETCH, self.v4f, res, &ops);
            return self.img_result(&data, dmask, res, d16);
        }
        if name == "image_get_resinfo" {
            let img = self.load_binding(b.ty, b.var);
            let (size, levels) = if b.storage {
                // A storage image has no level: its size is the view's.
                let s = self.m.id();
                self.ins(
                    op::IMAGE_QUERY_SIZE,
                    self.vec_of(self.t_i32, coords),
                    s,
                    &[img],
                );
                (s, None)
            } else {
                let mip = self.img_word(&regs, addr_place(0, 0, 0, a16))?;
                let s = self.m.id();
                self.ins(
                    op::IMAGE_QUERY_SIZE_LOD,
                    self.vec_of(self.t_i32, coords),
                    s,
                    &[img, mip],
                );
                let l = self.m.id();
                self.ins(op::IMAGE_QUERY_LEVELS, self.t_i32, l, &[img]);
                (s, Some(l))
            };
            let Operand::Vector { reg, .. } = data else {
                return gap("an image result that is not a VGPR range");
            };
            let one = self.one;
            for (k, ch) in (0..4u32)
                .filter(|c| if dmask == 0 { *c == 0 } else { dmask >> c & 1 != 0 })
                .enumerate()
            {
                let x = match ch {
                    0 => self.extract(self.t_i32, size, 0),
                    1 if coords > 1 => self.extract(self.t_i32, size, 1),
                    2 if dim == 2 || arrayed => self.extract(self.t_i32, size, 2),
                    3 => levels.unwrap_or(one),
                    _ => one,
                };
                self.store(slot(Reg::V(reg + k as u8)), x);
            }
            return Ok(());
        }
        let flags =
            SampleFlags::parse(name).ok_or(Gap("an image instruction this emitter does not know"))?;
        // The extras (offset, bias, comparison value), the gradients, then the
        // coordinates and the level (`SampleFlags`, as `lower::image`).
        let spatial = coords - usize::from(arrayed);
        let extras = usize::from(flags.o) + usize::from(flags.b) + usize::from(flags.c);
        let per_dir = if flags.d {
            if dim == 3 {
                2
            } else {
                spatial
            }
        } else {
            0
        };
        let coords_at = extras + 2 * per_dir;
        let level_at = coords_at + coords;
        let smp = match *sampler {
            Some(Operand::Scalar { code, .. }) => self
                .samplers
                .iter()
                .find(|s| s.0 == code)
                .map(|s| s.1)
                .ok_or(Gap("a sampler site outside the plan"))?,
            _ => return gap("a sample without a sampler descriptor"),
        };
        let img = self.load_binding(b.ty, b.var);
        let smp = self.load_binding(self.t_smp, smp);
        let si = self.m.id();
        self.ins(op::SAMPLED_IMAGE, b.si_ty, si, &[img, smp]);
        // Coordinates: floats, the array index converted from its dword.
        let mut cs = Vec::with_capacity(coords);
        for k in 0..coords {
            cs.push(self.img_f32(
                &regs,
                addr_place(coords_at + k, extras, 2 * per_dir, a16),
            )?);
        }
        if arrayed {
            let w = self.img_word(
                &regs,
                addr_place(coords_at + spatial, extras, 2 * per_dir, a16),
            )?;
            let u = self.un(op::BITCAST, self.t_u32, w);
            cs[spatial] = self.un(op::CONVERT_U_TO_F, self.t_f32, u);
        }
        if unorm {
            // Texel coordinates: divided by the size of level 0.
            let z = self.c32(0);
            let size = self.m.id();
            self.ins(
                op::IMAGE_QUERY_SIZE_LOD,
                self.vec_of(self.t_i32, coords),
                size,
                &[img, z],
            );
            for (k, slot) in cs.iter_mut().enumerate().take(spatial) {
                let w = self.img_word(
                    &regs,
                    addr_place(coords_at + k, extras, 2 * per_dir, a16),
                )?;
                let u = self.un(op::BITCAST, self.t_u32, w);
                let f = self.un(op::CONVERT_U_TO_F, self.t_f32, u);
                let n = self.extract(self.t_i32, size, k as u32);
                let n = self.un(op::BITCAST, self.t_u32, n);
                let n = self.un(op::CONVERT_U_TO_F, self.t_f32, n);
                *slot = self.bin(op::F_DIV, self.t_f32, f, n);
            }
        }
        // A level the instruction does not carry: level 0 (a compute sample
        // has no derivatives to take it from).
        let lod = if flags.l {
            self.img_f32(&regs, addr_place(level_at, extras, 2 * per_dir, a16))?
        } else {
            self.cf32(0.0)
        };
        let dref = if flags.c {
            Some(self.img_f32(&regs, addr_place(extras - 1, extras, 2 * per_dir, a16))?)
        } else {
            None
        };
        // Texel offsets are a packed signed 6-bit field per coordinate.
        let mut iop = Vec::new();
        if flags.o && !flags.gather {
            let w = self.img_word(&regs, addr_place(0, extras, 2 * per_dir, a16))?;
            let mut os = Vec::with_capacity(spatial);
            for k in 0..spatial {
                let sh = self.c32(26 - 8 * k as u32);
                let x = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, w, sh);
                let n = self.c32(26);
                os.push(self.bin(op::SHIFT_RIGHT_ARITHMETIC, self.t_i32, x, n));
            }
            let off = self.construct(&os, self.t_i32);
            iop.extend([0x10, off]);
        }
        let coord = self.construct(&cs, self.t_f32);
        let res = self.m.id();
        let mut ops = vec![si, coord];
        let opcode = if let Some(d) = dref {
            if flags.gather {
                ops.push(d);
                op::IMAGE_DREF_GATHER
            } else {
                ops.push(d);
                op::IMAGE_SAMPLE_DREF_EXPLICIT_LOD
            }
        } else if flags.gather {
            // The channel the gather's four texels come from.
            let comp = self.c32(if dmask == 0 {
                0
            } else {
                dmask.trailing_zeros()
            });
            ops.push(comp);
            op::IMAGE_GATHER
        } else {
            op::IMAGE_SAMPLE_EXPLICIT_LOD
        };
        // A gather takes no Lod operand in SPIR-V (implicit level only), so a
        // compute gather's level is whatever the driver picks.
        if !flags.gather {
            ops.extend([0x2, lod]);
        }
        ops.extend(iop);
        self.ins(opcode, self.v4f, res, &ops);
        self.img_result(&data, dmask, res, d16)
    }

    fn lds_ptr(&mut self, byte: u32) -> u32 {
        let two = self.c32(2);
        let idx = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, byte, two);
        let id = self.m.id();
        self.ins(op::ACCESS_CHAIN, self.pw32, id, &[self.lds, idx]);
        id
    }

    fn ds(&mut self, i: &Inst) -> R<()> {
        let Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            offset1,
            gds,
            ..
        } = &i.fields
        else {
            return gap("an LDS instruction without its fields");
        };
        if *gds {
            return gap("an LDS access with GDS");
        }
        let name = i.name();
        let base = match addr {
            Some(a) => self.src32(a)?,
            None => self.zero,
        };
        let at = |this: &mut Self, off: u8, scale: u32| {
            let k = this.c32(u32::from(off) * scale);
            this.bin(op::I_ADD, this.t_i32, base, k)
        };
        let (o0, o1) = (*offset0, *offset1);
        match name {
            "ds_read_b32" | "ds_read_u8" | "ds_read_i8" | "ds_read_u16" | "ds_read_i16" => {
                let d = (*dst).ok_or(Gap("an LDS read without a destination"))?;
                let (start, _) = self.op_regs(&d)?;
                let a = at(self, o0, 1);
                let p = self.lds_ptr(a);
                let v = self.m.id();
                self.ins(op::LOAD, self.t_i32, v, &[p]);
                self.store(start, v);
                Ok(())
            }
            "ds_read_b64" | "ds_read_b96" | "ds_read_b128" => {
                let d = (*dst).ok_or(Gap("an LDS read without a destination"))?;
                let (start, n) = self.op_regs(&d)?;
                for k in 0..u32::from(n) {
                    let a = at(self, o0, 1);
                    let four_k = self.c32(4 * k);
                    let a = self.bin(op::I_ADD, self.t_i32, a, four_k);
                    let p = self.lds_ptr(a);
                    let v = self.m.id();
                    self.ins(op::LOAD, self.t_i32, v, &[p]);
                    self.store(start + k, v);
                }
                Ok(())
            }
            "ds_read2_b32" | "ds_read2st64_b32" => {
                let scale = if name.contains("st64") { 64 } else { 1 };
                let d = (*dst).ok_or(Gap("an LDS read without a destination"))?;
                let (start, _) = self.op_regs(&d)?;
                for (k, off) in [o0, o1].into_iter().enumerate() {
                    let a = at(self, off, scale);
                    let p = self.lds_ptr(a);
                    let v = self.m.id();
                    self.ins(op::LOAD, self.t_i32, v, &[p]);
                    self.store(start + k as u32, v);
                }
                Ok(())
            }
            "ds_write_b8" | "ds_write_b16" | "ds_write_b32" => {
                let d = data.first().ok_or(Gap("an LDS write without data"))?;
                let (start, _) = self.op_regs(d)?;
                let a = at(self, o0, 1);
                let p = self.lds_ptr(a);
                let v = self.load(start);
                self.ins(op::STORE, 0, 0, &[p, v]);
                Ok(())
            }
            "ds_write_b64" | "ds_write_b96" | "ds_write_b128" => {
                let d = data.first().ok_or(Gap("an LDS write without data"))?;
                let (start, n) = self.op_regs(d)?;
                for k in 0..u32::from(n) {
                    let a = at(self, o0, 1);
                    let four_k = self.c32(4 * k);
                    let a = self.bin(op::I_ADD, self.t_i32, a, four_k);
                    let p = self.lds_ptr(a);
                    let v = self.load(start + k);
                    self.ins(op::STORE, 0, 0, &[p, v]);
                }
                Ok(())
            }
            "ds_write2_b32" | "ds_write2st64_b32" => {
                let scale = if name.contains("st64") { 64 } else { 1 };
                let d = data.first().ok_or(Gap("an LDS write without data"))?;
                let (start, n) = self.op_regs(d)?;
                for (k, off) in [o0, o1].into_iter().enumerate().take(u32::from(n) as usize) {
                    let a = at(self, off, scale);
                    let p = self.lds_ptr(a);
                    let v = self.load(start + k as u32);
                    self.ins(op::STORE, 0, 0, &[p, v]);
                }
                Ok(())
            }
            "ds_add_u32" | "ds_sub_u32" | "ds_min_u32" | "ds_max_u32" | "ds_min_i32"
            | "ds_max_i32" | "ds_and_b32" | "ds_or_b32" | "ds_xor_b32" => {
                let code = match name {
                    "ds_add_u32" => op::ATOMIC_I_ADD,
                    "ds_sub_u32" => op::ATOMIC_I_SUB,
                    "ds_min_u32" => op::ATOMIC_U_MIN,
                    "ds_max_u32" => op::ATOMIC_U_MAX,
                    "ds_min_i32" => op::ATOMIC_S_MIN,
                    "ds_max_i32" => op::ATOMIC_S_MAX,
                    "ds_and_b32" => op::ATOMIC_AND,
                    "ds_or_b32" => op::ATOMIC_OR,
                    _ => op::ATOMIC_XOR,
                };
                let d = data.first().ok_or(Gap("an LDS atomic without data"))?;
                let (start, _) = self.op_regs(d)?;
                let a = at(self, o0, 1);
                let p = self.lds_ptr(a);
                let (scope, sem) = (self.scope_workgroup, self.zero);
                let v = self.load(start);
                let id = self.m.id();
                self.ins(code, self.t_i32, id, &[p, scope, sem, v]);
                self.store(start, id);
                Ok(())
            }
            _ => gap("an LDS instruction this emitter does not implement"),
        }
    }
    /// The root arguments: user SGPRs are push-constant dwords.
    fn init_user_data(&mut self) -> R<()> {
        let n = self.io.initialized_sgprs.unwrap_or(0).min(16);
        for k in 0..n {
            let k = self.c32(u32::from(k));
            let p = self.tern(op::ACCESS_CHAIN, self.p_pc_i32, self.pc, self.zero, k);
            let v = self.m.id();
            self.ins(op::LOAD, self.t_i32, v, &[p]);
            self.store(k % 16, v);
        }
        Ok(())
    }

    /// Hardware-loaded inputs: thread and group ids become SPIR-V builtins.
    fn init_inputs(&mut self) -> R<()> {
        let inputs = self.io.inputs.clone();
        let regs = self.p.inputs.clone();
        for (reg, value) in regs {
            let Some((_, input)) = inputs.iter().find(|(v, _)| *v == value) else {
                continue;
            };
            match *input {
                Input::ThreadIdInGroup(c) => {
                    let v = self.builtin_component(true, c)?;
                    self.store(slot(reg), v);
                }
                Input::GroupId(c) => {
                    let v = self.builtin_component(false, c)?;
                    self.store(slot(reg), v);
                }
                Input::GroupInfo => self.store(slot(reg), self.zero),
                _ => return gap("a compute input that is not a thread or group id"),
            }
        }
        Ok(())
    }

    fn builtin_component(&mut self, local: bool, c: u8) -> R<u32> {
        let var = if local {
            self.tid_var.unwrap()
        } else {
            self.gid_var.unwrap()
        };
        let vec = self.m.id();
        self.ins(op::LOAD, self.t_v3u32, vec, &[var]);
        let id = self.m.id();
        // A composite index is a literal, not a constant id.
        self.ins(op::COMPOSITE_EXTRACT, self.t_i32, id, &[vec, u32::from(c)]);
        Ok(id)
    }

    fn exit_branch(&mut self, _b: usize, exit: Exit) -> R<()> {
        match exit {
            Exit::Jump(t) => {
                self.set_pc(t);
                self.ins(op::BRANCH, 0, 0, &[self.sel_merge]);
                Ok(())
            }
            Exit::Branch { taken, not_taken } => {
                let name = self.branch_name(_b);
                let cond = self.branch_cond(name)?;
                let (taken, not_taken) = (taken, not_taken);
                let c = self.c32(taken as u32);
                self.store(self.pc_var, c);
                let then = self.m.id();
                let els = self.m.id();
                // Each header needs its own merge block.
                let merge = self.m.id();
                self.ins(op::SELECTION_MERGE, 0, 0, &[merge, 0]);
                self.ins(op::BRANCH_CONDITIONAL, 0, 0, &[cond, then, els]);
                self.ins(op::LABEL, 0, then, &[]);
                self.ins(op::BRANCH, 0, 0, &[merge]);
                self.ins(op::LABEL, 0, els, &[]);
                let c = self.c32(not_taken as u32);
                self.store(self.pc_var, c);
                self.ins(op::BRANCH, 0, 0, &[merge]);
                self.ins(op::LABEL, 0, merge, &[]);
                self.ins(op::BRANCH, 0, 0, &[self.sel_merge]);
                Ok(())
            }
            Exit::End => {
                self.ins(op::BRANCH, 0, 0, &[self.exit]);
                Ok(())
            }
            Exit::Indirect => gap("an indirect branch"),
        }
    }

    fn set_pc(&mut self, b: usize) {
        let c = self.c32(b as u32);
        self.store(self.pc_var, c);
    }

    fn branch_name(&self, b: usize) -> &'static str {
        self.p.blocks[b]
            .insts
            .last()
            .map(|i| i.name())
            .unwrap_or("")
    }

    fn branch_cond(&mut self, name: &str) -> R<u32> {
        match name {
            "s_cbranch_scc0" => {
                let s = self.load_scc();
                Ok(self.logical_not(s))
            }
            "s_cbranch_scc1" => Ok(self.load_scc()),
            "s_cbranch_execz" => {
                let e = self.condition_id(252)?;
                Ok(e)
            }
            "s_cbranch_execnz" => {
                let e = self.condition_id(252)?;
                Ok(self.logical_not(e))
            }
            "s_cbranch_vccz" => self.condition_id(251),
            "s_cbranch_vccnz" => {
                let v = self.condition_id(251)?;
                Ok(self.logical_not(v))
            }
            _ => gap("a branch condition this emitter does not know"),
        }
    }

    // ---- instructions -------------------------------------------------------

    fn inst(&mut self, i: &Inst) -> R<()> {
        let name = i.name();
        self.src_patch.clear();
        self.sdwa_dst = None;
        match &i.fields {
            Fields::Smem { .. } => return self.smem(i),
            Fields::Buffer { .. } => return self.mubuf(i),
            Fields::Flat { .. } => return self.flat(i),
            Fields::Ds { .. } => return self.ds(i),
            Fields::Image { .. } => return self.image(i),
            Fields::Exp { .. } => return gap("an export"),
            Fields::Vintrp { .. } => return gap("an interpolation"),
            _ => {}
        }
        match name {
            // No-ops for this emitter: scheduling, branches (the block exit
            // carries the control flow), and hardware state with no value.
            "s_branch" | "s_cbranch_scc0" | "s_cbranch_scc1" | "s_cbranch_execz"
            | "s_cbranch_execnz" | "s_cbranch_vccz" | "s_cbranch_vccnz" | "s_endpgm" | "s_nop"
            | "s_waitcnt" | "s_waitcnt_vscnt" | "s_waitcnt_depctr" | "s_setreg_imm32_b32"
            | "s_icache_inv" | "s_sleep" | "s_inst_prefetch" | "s_clause" | "s_delay_alu"
            | "s_ttracedata" => Ok(()),
            "s_barrier" => {
                let (wg, sem) = (self.scope_workgroup, self.sem_acq_rel_workgroup);
                self.ins(op::CONTROL_BARRIER, 0, 0, &[wg, wg, sem]);
                Ok(())
            }
            // ---- scalar moves and arithmetic ----
            "s_mov_b32" => {
                let sop = self.sop(i)?;
                let v = self.src32(&sop.src[0])?;
                let dst = sop.dst.ok_or(Gap("a move without a destination"))?;
                self.wdst(&dst, v)
            }
            "s_mov_b64" => {
                let sop = self.sop(i)?;
                let v = self.src64(&sop.src[0])?;
                let dst = sop.dst.ok_or(Gap("a move without a destination"))?;
                self.wdst64(&dst, v)
            }
            "s_movk_i32" => {
                let sop = self.sop(i)?;
                let imm = i32::from(sop.imm.ok_or(Gap("s_movk_i32 without an immediate"))? as i16);
                let v = self.c32(imm as u32);
                self.wdst(&sop.dst.ok_or(Gap("a move without a destination"))?, v)
            }
            "s_cmov_b32" => {
                let sop = self.sop(i)?;
                let v = self.src32(&sop.src[0])?;
                let scc = self.load_scc();
                let zero = self.zero;
                let v = self.select(scc, v, zero);
                self.wdst(&sop.dst.ok_or(Gap("a move without a destination"))?, v)
            }
            "s_not_b32" => self.sop1(i, op::BITWISE_NOT, false),
            "s_brev_b32" => self.sop1(i, op::BIT_REVERSE, false),
            "s_abs_i32" => {
                let sop = self.sop(i)?;
                let v = self.src32(&sop.src[0])?;
                let r = self.un(op::BITWISE_NOT, self.t_i32, v);
                let r = self.bin(op::I_ADD, self.t_i32, r, self.one);
                let (sv, z) = (self.as_s32(v), self.as_s32(self.zero));
                let neg = self.bin(op::S_LESS_THAN, self.t_bool, sv, z);
                let r = self.select(neg, r, v);
                self.wdst(&sop.dst.ok_or(Gap("an abs without a destination"))?, r)
            }
            // Binary scalar ops.
            "s_add_i32" => self.sop2(i, op::I_ADD),
            "s_sub_i32" => self.sop2(i, op::I_SUB),
            "s_mul_i32" => self.sop2(i, op::I_MUL),
            "s_and_b32" => self.sop2(i, op::BITWISE_AND),
            "s_or_b32" => self.sop2(i, op::BITWISE_OR),
            "s_xor_b32" => self.sop2(i, op::BITWISE_XOR),
            "s_andn2_b32" => self.sop_not2(i, op::BITWISE_AND),
            "s_orn2_b32" => self.sop_not2(i, op::BITWISE_OR),
            "s_nand_b32" => self.sop_notres(i, op::BITWISE_AND),
            "s_nor_b32" => self.sop_notres(i, op::BITWISE_OR),
            "s_xnor_b32" => self.sop_notres(i, op::BITWISE_XOR),
            "s_lshl_b32" => self.sop2(i, op::SHIFT_LEFT_LOGICAL),
            "s_lshr_b32" => self.sop2(i, op::SHIFT_RIGHT_LOGICAL),
            "s_ashr_i32" => self.sop2(i, op::SHIFT_RIGHT_ARITHMETIC),
            "s_mul_hi_u32" => self.sop_mul_hi(i, false),
            "s_mul_hi_i32" => self.sop_mul_hi(i, true),
            "s_bfe_u32" => self.sop_bfe(i, false),
            "s_bfe_i32" => self.sop_bfe(i, true),
            "s_bfm_b32" => self.sop_bfm(i),
            "s_min_i32" => self.sop_minmax(i, glsl::SMIN),
            "s_max_i32" => self.sop_minmax(i, glsl::SMAX),
            "s_min_u32" => self.sop_minmax(i, glsl::UMIN),
            "s_max_u32" => self.sop_minmax(i, glsl::UMAX),
            "s_cselect_b32" => {
                let sop = self.sop(i)?;
                let a = self.src32(&sop.src[0])?;
                let b = self.src32(&sop.src[1])?;
                let scc = self.load_scc();
                let v = self.select(scc, a, b);
                self.wdst(&sop.dst.ok_or(Gap("a select without a destination"))?, v)
            }
            "s_cselect_b64" => {
                let sop = self.sop(i)?;
                let a = self.src64(&sop.src[0])?;
                let b = self.src64(&sop.src[1])?;
                let scc = self.load_scc();
                let id = self.m.id();
                self.ins(op::SELECT, self.t_i64, id, &[scc, a, b]);
                self.wdst64(&sop.dst.ok_or(Gap("a select without a destination"))?, id)
            }
            // ---- 64-bit scalar bit operations on lane masks ----
            "s_and_b64" | "s_or_b64" | "s_xor_b64" => {
                let code = match name {
                    "s_and_b64" => op::BITWISE_AND,
                    "s_or_b64" => op::BITWISE_OR,
                    _ => op::BITWISE_XOR,
                };
                self.sop2_64(i, code)
            }
            "s_andn2_b64" => self.sop2_64_not2(i, op::BITWISE_AND),
            "s_orn2_b64" => self.sop2_64_not2(i, op::BITWISE_OR),
            "s_not_b64" => {
                let sop = self.sop(i)?;
                let v = self.src64(&sop.src[0])?;
                let r = self.un(op::BITWISE_NOT, self.t_i64, v);
                self.wdst64(&sop.dst.ok_or(Gap("a not without a destination"))?, r)
            }
            // Save-exec forms: the old EXEC goes to the destination.
            "s_and_saveexec_b64" | "s_or_saveexec_b64" | "s_xor_saveexec_b64" => {
                let code = match name {
                    "s_and_saveexec_b64" => op::BITWISE_AND,
                    "s_or_saveexec_b64" => op::BITWISE_OR,
                    _ => op::BITWISE_XOR,
                };
                let sop = self.sop(i)?;
                let exec = self.pair64(126);
                let src = self.src64(&sop.src[0])?;
                let new = self.bin(code, self.t_i64, exec, src);
                self.write_pair64(126, new);
                self.wdst64(
                    &sop.dst.ok_or(Gap("a save-exec without a destination"))?,
                    exec,
                )
            }
            // Add with the carry (or borrow) in SCC.
            "s_add_u32" | "s_addc_u32" | "s_subb_u32" | "s_sub_u32" => {
                let sop = self.sop(i)?;
                let a = self.src32(&sop.src[0])?;
                let b = self.src32(&sop.src[1])?;
                let sub = name.contains("sub");
                let mut r = self.bin(if sub { op::I_SUB } else { op::I_ADD }, self.t_i32, a, b);
                if name.ends_with('c') && name != "s_addc_u32" || name == "s_addc_u32" || name == "s_subb_u32" {
                    // The carry-in is SCC: add it, then the carry-out.
                    // SCC is an i1 in the module: widen it to i32 before use.
                    let scc = self.load_scc();
                    let (one, zero) = (self.one, self.zero);
                    let bit = self.tern(op::SELECT, self.t_i32, scc, one, zero);
                    r = self.bin(if sub { op::I_SUB } else { op::I_ADD }, self.t_i32, r, bit);
                }
                let carry = self.bin(
                    if sub { op::U_GREATER_THAN } else { op::U_LESS_THAN },
                    self.t_bool,
                    r,
                    a,
                );
                self.wdst(&sop.dst.ok_or(Gap("an add without a destination"))?, r)?;
                self.store_scc(carry);
                Ok(())
            }
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                let sop = self.sop(i)?;
                let a = self.src64(&sop.src[0])?;
                let n = self.src32(&sop.src[1])?;
                let n = self.u32_to_u64(n);
                let code = match name {
                    "s_lshl_b64" => op::SHIFT_LEFT_LOGICAL,
                    "s_lshr_b64" => op::SHIFT_RIGHT_LOGICAL,
                    _ => op::SHIFT_RIGHT_ARITHMETIC,
                };
                let r = self.bin(code, self.t_i64, a, n);
                self.wdst64(&sop.dst.ok_or(Gap("a shift without a destination"))?, r)
            }
            "s_bcnt1_i32_b64" => {
                let sop = self.sop(i)?;
                let (lo, hi) = self.halves64(i)?;
                let a = self.un(op::BIT_COUNT, self.t_i32, lo);
                let b = self.un(op::BIT_COUNT, self.t_i32, hi);
                let r = self.bin(op::I_ADD, self.t_i32, a, b);
                self.wdst(&sop.dst.ok_or(Gap("a count without a destination"))?, r)
            }
            "s_ff1_i32_b32" | "s_flbit_i32" | "s_flbit_i32_b32" => {
                let sop = self.sop(i)?;
                let x = self.src32(&sop.src[0])?;
                let r = self.ff1(x, !name.starts_with("s_ff1"));
                self.wdst(&sop.dst.ok_or(Gap("a first-bit without a destination"))?, r)
            }
            "s_ff1_i32_b64" => {
                // The low half unless it is zero, then 32 and the high half.
                let sop = self.sop(i)?;
                let (lo, hi) = self.halves64(i)?;
                let n0 = self.ff1(lo, false);
                let n1 = self.ff1(hi, false);
                let k32 = self.c32(32);
                let n1 = self.bin(op::I_ADD, self.t_i32, n1, k32);
                let z = self.is_zero32(lo);
                let r = self.select(z, n1, n0);
                self.wdst(&sop.dst.ok_or(Gap("a first-bit without a destination"))?, r)
            }
            "s_bcnt1_i32_b32" => {
                let sop = self.sop(i)?;
                let v = self.src32(&sop.src[0])?;
                let r = self.un(op::BIT_COUNT, self.t_i32, v);
                self.wdst(&sop.dst.ok_or(Gap("a count without a destination"))?, r)
            }
            // Scalar compares write SCC.
            "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_lt_u32" | "s_cmp_le_u32" | "s_cmp_gt_u32"
            | "s_cmp_ge_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32" | "s_cmp_lt_i32"
            | "s_cmp_le_i32" | "s_cmp_gt_i32" | "s_cmp_ge_i32" => {
                let sop = self.sop(i)?;
                let a = self.src32(&sop.src[0])?;
                let b = self.src32(&sop.src[1])?;
                let (kind, int) = name
                    .strip_prefix("s_cmp_")
                    .and_then(|s| s.split_once('_'))
                    .ok_or(Gap("a compare name this emitter does not know"))?;
                self.scc_compare(kind, int, a, b)
            }
            // SOPK's compare: one register source (the word's `dst` field, as
            // SOPK is decoded) and the immediate, sign-extended in the `_i32`
            // forms.
            "s_cmpk_eq_u32" | "s_cmpk_lg_u32" | "s_cmpk_lt_u32" | "s_cmpk_le_u32"
            | "s_cmpk_gt_u32" | "s_cmpk_ge_u32" | "s_cmpk_eq_i32" | "s_cmpk_lg_i32"
            | "s_cmpk_lt_i32" | "s_cmpk_le_i32" | "s_cmpk_gt_i32" | "s_cmpk_ge_i32" => {
                let sop = self.sop(i)?;
                let a = self.src32(&sop.src[0])?;
                let imm = sop.imm.ok_or(Gap("a compare without an immediate"))?;
                let (kind, int) = name
                    .strip_prefix("s_cmpk_")
                    .and_then(|s| s.split_once('_'))
                    .ok_or(Gap("a compare name this emitter does not know"))?;
                let k = if int == "i32" {
                    i32::from(imm as i16) as u32
                } else {
                    u32::from(imm)
                };
                let b = self.c32(k);
                self.scc_compare(kind, int, a, b)
            }
            "s_addk_i32" | "s_mulk_i32" => {
                let sop = self.sop(i)?;
                let imm = sop
                    .imm
                    .ok_or(Gap("a scalar immediate operation without one"))?;
                let mut k = u32::from(imm);
                if name == "s_addk_i32" {
                    k = (i32::from(imm as i16)) as u32;
                }
                let dst = sop.dst.ok_or(Gap("an operation without a destination"))?;
                // SOPK reads and writes the same SGPR.
                let a = self.src32(&dst)?;
                let k = self.c32(k);
                let code = if name == "s_addk_i32" {
                    op::I_ADD
                } else {
                    op::I_MUL
                };
                let r = self.bin(code, self.t_i32, a, k);
                self.wdst(&dst, r)
            }
            "s_bitset0_b32" | "s_bitset1_b32" => {
                let sop = self.sop(i)?;
                let dst = sop.dst.ok_or(Gap("a bitset without a destination"))?;
                // SOP1's destination is its other source.
                let a = self.src32(&dst)?;
                let bit = self.src32(&sop.src[0])?;
                let one = self.one;
                let m = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, one, bit);
                let r = if name == "s_bitset1_b32" {
                    self.bin(op::BITWISE_OR, self.t_i32, a, m)
                } else {
                    let nm = self.un(op::BITWISE_NOT, self.t_i32, m);
                    self.bin(op::BITWISE_AND, self.t_i32, a, nm)
                };
                self.wdst(&dst, r)
            }
            "s_and_saveexec_b32" | "s_or_saveexec_b32" | "s_xor_saveexec_b32" => {
                let code = match name {
                    "s_and_saveexec_b32" => op::BITWISE_AND,
                    "s_or_saveexec_b32" => op::BITWISE_OR,
                    _ => op::BITWISE_XOR,
                };
                let sop = self.sop(i)?;
                let exec = self.load(126);
                let src = self.src32(&sop.src[0])?;
                let new = self.bin(code, self.t_i32, exec, src);
                self.store(126, new);
                self.store(127, new);
                self.wdst(
                    &sop.dst.ok_or(Gap("a save-exec without a destination"))?,
                    exec,
                )
            }
            "s_wqm_b64" | "s_wqm_b32" => {
                // shortcut: the whole quad mask a lane's EXEC bit stands for
                // is taken to be the mask itself (right when EXEC is full).
                let sop = self.sop(i)?;
                let v = if name == "s_wqm_b64" {
                    self.src64(&sop.src[0])?
                } else {
                    let a = self.src32(&sop.src[0])?;
                    self.u32_to_u64(a)
                };
                let dst = sop.dst.ok_or(Gap("a wqm without a destination"))?;
                if name == "s_wqm_b64" {
                    self.wdst64(&dst, v)
                } else {
                    let lo = self.u64_to_u32(v);
                    self.wdst(&dst, lo)
                }
            }
            // ---- vector instructions ----
            _ => {
                if matches!(i.fields, Fields::Valu(_)) {
                    self.valu_inst(i, name)
                } else {
                    Err(Gap(name))
                }
            }
        }
    }

    fn sop(&self, i: &Inst) -> R<Sop> {
        match &i.fields {
            Fields::Sop { dst, src, imm } => Ok(Sop {
                dst: *dst,
                src: src.clone(),
                imm: *imm,
            }),
            _ => gap("a scalar instruction without scalar fields"),
        }
    }

    fn sop1(&mut self, i: &Inst, opcode: u16, _f: bool) -> R<()> {
        let s = self.sop(i)?;
        let v = self.src32(&s.src[0])?;
        let r = self.un(opcode, self.t_i32, v);
        self.wdst(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    /// SCC = `a` <pred> `b`: the shared body of `s_cmp_*` and `s_cmpk_*`.
    fn scc_compare(&mut self, kind: &str, int: &str, a: u32, b: u32) -> R<()> {
        let pred =
            int_pred(kind, int == "i32").ok_or(Gap("a compare kind this emitter does not know"))?;
        let unsigned = matches!(
            pred,
            op::U_GREATER_THAN
                | op::U_GREATER_THAN_EQUAL
                | op::U_LESS_THAN
                | op::U_LESS_THAN_EQUAL
        );
        let (a, b) = if unsigned {
            (self.as_u32(a), self.as_u32(b))
        } else if kind != "eq" && kind != "ne" && kind != "lg" && int == "i32" {
            (self.as_s32(a), self.as_s32(b))
        } else {
            (a, b)
        };
        let c = self.cmp(pred, a, b);
        self.store_scc(c);
        Ok(())
    }

    fn sop2(&mut self, i: &Inst, opcode: u16) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src32(&s.src[0])?;
        let b = self.src32(&s.src[1])?;
        let r = self.bin(opcode, self.t_i32, a, b);
        self.wdst(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn sop_not2(&mut self, i: &Inst, opcode: u16) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src32(&s.src[0])?;
        let b = self.src32(&s.src[1])?;
        let nb = self.un(op::BITWISE_NOT, self.t_i32, b);
        let r = self.bin(opcode, self.t_i32, a, nb);
        self.wdst(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn sop_notres(&mut self, i: &Inst, opcode: u16) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src32(&s.src[0])?;
        let b = self.src32(&s.src[1])?;
        let r = self.bin(opcode, self.t_i32, a, b);
        let r = self.un(op::BITWISE_NOT, self.t_i32, r);
        self.wdst(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn sop2_64(&mut self, i: &Inst, opcode: u16) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src64(&s.src[0])?;
        let b = self.src64(&s.src[1])?;
        let r = self.bin(opcode, self.t_i64, a, b);
        self.wdst64(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn sop2_64_not2(&mut self, i: &Inst, opcode: u16) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src64(&s.src[0])?;
        let b = self.src64(&s.src[1])?;
        let nb = self.un(op::BITWISE_NOT, self.t_i64, b);
        let r = self.bin(opcode, self.t_i64, a, nb);
        self.wdst64(&s.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn sop_mul_hi(&mut self, i: &Inst, signed: bool) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src32(&s.src[0])?;
        let b = self.src32(&s.src[1])?;
        let (a, b) = if signed {
            (self.sext64(a), self.sext64(b))
        } else {
            (self.u32_to_u64(a), self.u32_to_u64(b))
        };
        let p = self.bin(op::I_MUL, self.t_i64, a, b);
        let shift = self.c64(32);
        let hi = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i64, p, shift);
        let hi = self.u64_to_u32(hi);
        self.wdst(&s.dst.ok_or(Gap("a multiply without a destination"))?, hi)
    }

    fn sop_bfe(&mut self, i: &Inst, signed: bool) -> R<()> {
        let s = self.sop(i)?;
        let v = self.src32(&s.src[0])?;
        let field = self.src32(&s.src[1])?;
        let sixteen = self.c32(16);
        let sh = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, field, sixteen);
        let low = self.c32(0x1f);
        let _mask = self.bin(op::BITWISE_AND, self.t_i32, field, low);
        let max = self.c32(32);
        let (uf, umax) = (self.as_u32(field), self.as_u32(max));
        let width = self.bin(op::U_MOD, self.t_u32, uf, umax);
        let width = self.as_s32(width);
        // `width == 0` selects 32 bits on the machine; the host clamps it.
        let zero_w = self.is_zero32(width);
        let width = self.select(zero_w, max, width);
        let count = self.bin(op::I_ADD, self.t_i32, width, self.one);
        let pred = if signed {
            op::BIT_FIELD_S_EXTRACT
        } else {
            op::BIT_FIELD_U_EXTRACT
        };
        let r = self.tern(pred, self.t_i32, v, sh, count);
        self.wdst(&s.dst.ok_or(Gap("a bitfield without a destination"))?, r)
    }

    fn sop_bfm(&mut self, i: &Inst) -> R<()> {
        let s = self.sop(i)?;
        let (w, off) = (self.src32(&s.src[0])?, self.src32(&s.src[1])?);
        let k32 = self.c32(32);
        let n = self.bin(op::I_SUB, self.t_i32, k32, w);
        let low = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, self.minus_one, n);
        let r = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, low, off);
        self.wdst(&s.dst.ok_or(Gap("a mask without a destination"))?, r)
    }

    fn sop_minmax(&mut self, i: &Inst, ext: u32) -> R<()> {
        let s = self.sop(i)?;
        let a = self.src32(&s.src[0])?;
        let b = self.src32(&s.src[1])?;
        let r = self.minmax(ext, a, b);
        self.wdst(&s.dst.ok_or(Gap("a min or max without a destination"))?, r)
    }

    /// The index of the first set bit (`ff1`) or, with `msb`, of the last
    /// (`flbit`), or -1 for zero: counted with `OpBitCount` on the isolated
    /// bit, so no GLSL instruction is needed (and `BitCount` stays 32-bit).
    fn ff1(&mut self, x: u32, msb: bool) -> u32 {
        let is_zero = self.is_zero32(x);
        let mut bit = x;
        if msb {
            for sh in [1u32, 2, 4, 8, 16] {
                let n = self.c32(sh);
                let t = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, bit, n);
                bit = self.bin(op::BITWISE_OR, self.t_i32, bit, t);
            }
            let one = self.one;
            let t = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, bit, one);
            let t = self.un(op::BITWISE_NOT, self.t_i32, t);
            bit = self.bin(op::BITWISE_AND, self.t_i32, bit, t);
        }
        let neg = self.bin(op::I_SUB, self.t_i32, self.zero, bit);
        let low = self.bin(op::BITWISE_AND, self.t_i32, bit, neg);
        let below = self.bin(op::I_SUB, self.t_i32, low, self.one);
        let n = self.un(op::BIT_COUNT, self.t_i32, below);
        let none = self.minus_one;
        self.select(is_zero, none, n)
    }

    /// The two 32-bit halves of a 64-bit scalar source.
    fn halves64(&mut self, i: &Inst) -> R<(u32, u32)> {
        let s = self.sop(i)?;
        let a = self.src64(&s.src[0])?;
        let lo = self.u64_to_u32(a);
        let sh = self.c64(32);
        let hi = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i64, a, sh);
        Ok((lo, self.u64_to_u32(hi)))
    }

    /// A min or max in the type the GLSL instruction class wants.
    fn minmax(&mut self, ext: u32, a: u32, b: u32) -> u32 {
        if matches!(ext, glsl::UMIN | glsl::UMAX | glsl::SMIN | glsl::SMAX) {
            let (ty, uv) = if matches!(ext, glsl::UMIN | glsl::UMAX) {
                (self.t_u32, true)
            } else {
                (self.t_i32, false)
            };
            let (a, b) = if uv {
                (self.as_u32(a), self.as_u32(b))
            } else {
                (self.as_s32(a), self.as_s32(b))
            };
            let r = self.ext2(ext, ty, a, b);
            self.as_s32(r)
        } else {
            self.ext2(ext, self.t_i32, a, b)
        }
    }

    // ---- vector instructions ------------------------------------------------

    fn valu_inst(&mut self, i: &Inst, name: &'static str) -> R<()> {
        let v = match &i.fields {
            Fields::Valu(v) => v.clone(),
            _ => return gap("an instruction this emitter does not implement"),
        };
        Self::valu_mods(&v)?;
        if v.sdwa.is_some() || v.opsel & 3 != 0 {
            self.valu_patch(&v, name)?;
        }
        if let Some(rest) = name.strip_prefix("v_cmpx_") {
            return self.valu_compare(&v, rest, true);
        }
        if let Some(rest) = name.strip_prefix("v_cmp_") {
            return self.valu_compare(&v, rest, false);
        }
        match name {
            "v_mov_b32" => {
                let r = self.src32(&v.src[0])?;
                self.wdst(&v.dst.ok_or(Gap("a move without a destination"))?, r)
            }
            "v_mov_b64" => {
                let r = self.src64(&v.src[0])?;
                self.wdst64(&v.dst.ok_or(Gap("a move without a destination"))?, r)
            }
            "v_swap_b32" => {
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let (sa, _) = self.op_regs(&v.src[0])?;
                let (sb, _) = self.op_regs(&v.src[1])?;
                self.store(sa, b);
                self.store(sb, a);
                Ok(())
            }
            "v_readfirstlane_b32" | "v_readlane_b32" | "v_writelane_b32" => {
                // shortcut: one lane per SPIR-V invocation, so a read of any
                // lane's value is this lane's (right for the uniform values
                // the title reads this way).
                let r = self.src32(&v.src[0])?;
                self.wdst(&v.dst.ok_or(Gap("a lane move without a destination"))?, r)
            }
            "v_cndmask_b32" => {
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let mask = self.src32(&v.src[2])?;
                let cond = self.bin(op::I_NOT_EQUAL, self.t_bool, mask, self.zero);
                let r = self.select(cond, a, b);
                self.wdst(&v.dst.ok_or(Gap("a select without a destination"))?, r)
            }
            // Floating-point binary.
            "v_add_f32" | "v_add_f16" => self.fv_bin(&v, op::F_ADD),
            "v_sub_f32" => self.fv_bin(&v, op::F_SUB),
            "v_subrev_f32" => self.fv_bin_rev(&v, op::F_SUB),
            "v_mul_f32" | "v_mul_legacy_f32" | "v_mul_dx9_zero_f32" => self.fv_bin(&v, op::F_MUL),
            "v_min_f32" => self.fv_ext2(&v, glsl::FMIN),
            "v_max_f32" => self.fv_ext2(&v, glsl::FMAX),
            "v_fma_f32" | "v_mad_f32" | "v_fma_f32_f16" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let c = self.srcf32(&v.src[2], v.neg & 4 != 0, v.abs & 4 != 0)?;
                let p = self.bin(op::F_MUL, self.t_f32, a, b);
                let r = self.bin(op::F_ADD, self.t_f32, p, c);
                self.fv_out(&v, r)
            }
            "v_fmac_f32" | "v_mac_f32" | "v_mad_legacy_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let dst = v.dst.ok_or(Gap("a multiply-add without a destination"))?;
                let c = self.srcf32(&dst, false, false)?;
                let p = self.bin(op::F_MUL, self.t_f32, a, b);
                let r = self.bin(op::F_ADD, self.t_f32, p, c);
                self.fv_out(&v, r)
            }
            // A x B + K, or A x K + B: the literal is the second source in
            // both spellings (`gfx10::fields`), the addend the third.
            "v_madmk_f32" | "v_madak_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], false, false)?;
                let k = self.srcf32(&v.src[2], false, false)?;
                let p = self.bin(op::F_MUL, self.t_f32, a, b);
                let r = self.bin(op::F_ADD, self.t_f32, p, k);
                self.fv_out(&v, r)
            }
            "v_fma_mix_f32" | "v_fmaak_f32" => {
                let a = self.mix_src(&v, 0)?;
                let b = self.mix_src(&v, 1)?;
                let c = self.srcf32(&v.src[2], v.neg & 4 != 0, v.abs & 4 != 0)?;
                let p = self.bin(op::F_MUL, self.t_f32, a, b);
                let r = self.bin(op::F_ADD, self.t_f32, p, c);
                self.fv_out(&v, r)
            }
            "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                // f16 arithmetic, the half `opsel_hi` bit 2 names.
                let a = self.mix_src(&v, 0)?;
                let b = self.mix_src(&v, 1)?;
                let c = self.mix_src(&v, 2)?;
                let p = self.bin(op::F_MUL, self.t_f32, a, b);
                let r = self.bin(op::F_ADD, self.t_f32, p, c);
                let h = self.f32_to_half(r);
                let (pair, high) = if name == "v_fma_mixhi_f16" {
                    (h, true)
                } else {
                    (h, false)
                };
                let dst = v.dst.ok_or(Gap("a multiply-add without a destination"))?;
                let (slot, _) = self.op_regs(&dst)?;
                let old = self.load(slot);
                let sh = self.c32(if high { 16 } else { 0 });
                let m = self.c32(0xFFFF);
                let v = if high {
                    let x = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, pair, sh);
                    let lo = self.bin(op::BITWISE_AND, self.t_i32, old, m);
                    self.bin(op::BITWISE_OR, self.t_i32, x, lo)
                } else {
                    let top = self.c32(0xFFFF_0000);
                    let hi = self.bin(op::BITWISE_AND, self.t_i32, old, top);
                    self.bin(op::BITWISE_OR, self.t_i32, pair, hi)
                };
                self.wdst(&dst, v)
            }
            "v_min3_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let c = self.srcf32(&v.src[2], v.neg & 4 != 0, v.abs & 4 != 0)?;
                let t = self.ext2(glsl::FMIN, self.t_f32, a, b);
                let r = self.ext2(glsl::FMIN, self.t_f32, t, c);
                self.fv_out(&v, r)
            }
            "v_max3_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let c = self.srcf32(&v.src[2], v.neg & 4 != 0, v.abs & 4 != 0)?;
                let t = self.ext2(glsl::FMAX, self.t_f32, a, b);
                let r = self.ext2(glsl::FMAX, self.t_f32, t, c);
                self.fv_out(&v, r)
            }
            "v_med3_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let c = self.srcf32(&v.src[2], v.neg & 4 != 0, v.abs & 4 != 0)?;
                let lo = self.ext2(glsl::FMIN, self.t_f32, a, b);
                let hi = self.ext2(glsl::FMAX, self.t_f32, a, b);
                let t = self.ext2(glsl::FMIN, self.t_f32, hi, c);
                let r = self.ext2(glsl::FMAX, self.t_f32, lo, t);
                self.fv_out(&v, r)
            }
            // Floating-point unary.
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let one = self.cf32(1.0);
                let r = self.bin(op::F_DIV, self.t_f32, one, a);
                self.fv_out(&v, r)
            }
            "v_sqrt_f32" => self.fv_ext1(&v, glsl::SQRT),
            "v_rsq_f32" => self.fv_ext1(&v, glsl::INVERSE_SQRT),
            "v_exp_f32" => self.fv_ext1(&v, glsl::EXP2),
            "v_log_f32" => self.fv_ext1(&v, glsl::LOG2),
            "v_sin_f32" => self.fv_ext1(&v, glsl::SIN),
            "v_cos_f32" => self.fv_ext1(&v, glsl::COS),
            "v_trunc_f32" => self.fv_ext1(&v, glsl::TRUNC),
            "v_floor_f32" => self.fv_ext1(&v, glsl::FLOOR),
            "v_ceil_f32" => self.fv_ext1(&v, glsl::CEIL),
            "v_fract_f32" => self.fv_ext1(&v, glsl::FRACT),
            "v_rndne_f32" => self.fv_ext1(&v, glsl::ROUND_EVEN),
            // Integer arithmetic.
            "v_add_u32" | "v_add_i32" | "v_add_nc_u32" => self.iv_bin(&v, op::I_ADD),
            "v_sub_u32" | "v_sub_i32" | "v_sub_nc_u32" => self.iv_bin(&v, op::I_SUB),
            "v_subrev_u32" | "v_subrev_nc_u32" => self.iv_bin_rev(&v, op::I_SUB),
            "v_mul_lo_u32" => self.iv_bin(&v, op::I_MUL),
            "v_add_co_u32" | "v_addc_co_u32" | "v_add_co_ci_u32" => self.add_carry(&v, false),
            "v_sub_co_u32" | "v_subb_co_u32" | "v_sub_co_ci_u32" => self.add_carry(&v, true),
            "v_mul_hi_u32" => self.mul_hi(&v, false),
            "v_mul_hi_i32" => self.mul_hi(&v, true),
            "v_mul_u32_u24" => self.mul_u24(&v),
            "v_mad_u32_u24" => self.mad_u24(&v),
            "v_mad_i32_i24" => self.mad_i24(&v),
            "v_and_b32" => self.iv_bin(&v, op::BITWISE_AND),
            "v_or_b32" => self.iv_bin(&v, op::BITWISE_OR),
            "v_xor_b32" => self.iv_bin(&v, op::BITWISE_XOR),
            "v_not_b32" => {
                let a = self.src32(&v.src[0])?;
                let r = self.un(op::BITWISE_NOT, self.t_i32, a);
                self.wdst(&v.dst.ok_or(Gap("a not without a destination"))?, r)
            }
            "v_andn2_b32" => {
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let nb = self.un(op::BITWISE_NOT, self.t_i32, b);
                let r = self.bin(op::BITWISE_AND, self.t_i32, a, nb);
                self.wdst(&v.dst.ok_or(Gap("an and not without a destination"))?, r)
            }
            "v_orn2_b32" => {
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let nb = self.un(op::BITWISE_NOT, self.t_i32, b);
                let r = self.bin(op::BITWISE_OR, self.t_i32, a, nb);
                self.wdst(&v.dst.ok_or(Gap("an or not without a destination"))?, r)
            }
            "v_and_or_b32" => {
                let (a, b, c) = self.src3(&v)?;
                let p = self.bin(op::BITWISE_AND, self.t_i32, a, b);
                let r = self.bin(op::BITWISE_OR, self.t_i32, p, c);
                self.wdst(&v.dst.ok_or(Gap("an and-or without a destination"))?, r)
            }
            "v_or3_b32" => {
                let (a, b, c) = self.src3(&v)?;
                let p = self.bin(op::BITWISE_OR, self.t_i32, a, b);
                let r = self.bin(op::BITWISE_OR, self.t_i32, p, c);
                self.wdst(&v.dst.ok_or(Gap("an or3 without a destination"))?, r)
            }
            "v_lshlrev_b32" => self.iv_bin_rev(&v, op::SHIFT_LEFT_LOGICAL),
            "v_lshrrev_b32" => self.iv_bin_rev(&v, op::SHIFT_RIGHT_LOGICAL),
            "v_ashrrev_i32" => self.iv_bin_rev(&v, op::SHIFT_RIGHT_ARITHMETIC),
            "v_lshl_add_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let s = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, a, b);
                let r = self.bin(op::I_ADD, self.t_i32, s, c);
                self.wdst(&v.dst.ok_or(Gap("a shift-add without a destination"))?, r)
            }
            "v_add_lshl_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let s = self.bin(op::I_ADD, self.t_i32, a, b);
                let r = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, s, c);
                self.wdst(&v.dst.ok_or(Gap("an add-shift without a destination"))?, r)
            }
            "v_lshl_or_b32" => {
                let (a, b, c) = self.src3(&v)?;
                let s = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, a, b);
                let r = self.bin(op::BITWISE_OR, self.t_i32, s, c);
                self.wdst(&v.dst.ok_or(Gap("a shift-or without a destination"))?, r)
            }
            "v_add3_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let s = self.bin(op::I_ADD, self.t_i32, a, b);
                let r = self.bin(op::I_ADD, self.t_i32, s, c);
                self.wdst(&v.dst.ok_or(Gap("an add3 without a destination"))?, r)
            }
            "v_xad_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let s = self.bin(op::BITWISE_XOR, self.t_i32, a, b);
                let r = self.bin(op::I_ADD, self.t_i32, s, c);
                self.wdst(&v.dst.ok_or(Gap("an xor-add without a destination"))?, r)
            }
            "v_bfe_u32" => self.vbfe(&v, false),
            "v_bfe_i32" => self.vbfe(&v, true),
            "v_bfi_b32" => {
                let (b, base, mask) = self.src3(&v)?;
                let ins = self.bin(op::BITWISE_AND, self.t_i32, b, mask);
                let nm = self.un(op::BITWISE_NOT, self.t_i32, mask);
                let keep = self.bin(op::BITWISE_AND, self.t_i32, base, nm);
                let r = self.bin(op::BITWISE_OR, self.t_i32, ins, keep);
                self.wdst(
                    &v.dst
                        .ok_or(Gap("a bitfield insert without a destination"))?,
                    r,
                )
            }
            "v_bfm_b32" => {
                let (w, off) = (self.src32(&v.src[0])?, self.src32(&v.src[1])?);
                let k32 = self.c32(32);
                let n = self.bin(op::I_SUB, self.t_i32, k32, w);
                let low = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, self.minus_one, n);
                let r = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, low, off);
                self.wdst(&v.dst.ok_or(Gap("a mask without a destination"))?, r)
            }
            "v_bcnt_u32_b32" => {
                let (a, b, _) = {
                    let a = self.src32(&v.src[0])?;
                    let b = self.src32(&v.src[1])?;
                    (a, b, 0)
                };
                let n = self.un(op::BIT_COUNT, self.t_i32, a);
                let r = self.bin(op::I_ADD, self.t_i32, n, b);
                self.wdst(&v.dst.ok_or(Gap("a bit count without a destination"))?, r)
            }
            "v_pack_b32_f16" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
                let (ha, hb) = (
                    self.un(op::F_CONVERT, self.t_f16, a),
                    self.un(op::F_CONVERT, self.t_f16, b),
                );
                let (ia, ib) = (
                    self.un(op::BITCAST, self.t_u16, ha),
                    self.un(op::BITCAST, self.t_u16, hb),
                );
                let (la, lb) = (self.u16_to_u32(ia), self.u16_to_u32(ib));
                let sixteen = self.c32(16);
                let hi = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, lb, sixteen);
                let not16 = self.c32(0xffff);
                let lo = self.bin(op::BITWISE_AND, self.t_i32, la, not16);
                let r = self.bin(op::BITWISE_OR, self.t_i32, hi, lo);
                self.wdst(&v.dst.ok_or(Gap("a pack without a destination"))?, r)
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                // shortcut: no lane ids in the scalar model; the title uses
                // the count only to index by wave position.
                let b = self.src32(&v.src[1])?;
                self.wdst(&v.dst.ok_or(Gap("a lane count without a destination"))?, b)
            }
            "v_subrev_co_u32" | "v_subrev_co_ci_u32" => {
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let mut r = self.bin(op::I_SUB, self.t_i32, b, a);
                if v.src.len() > 2 {
                    let cin = self.src32(&v.src[2])?;
                    let bit = self.bin(op::BITWISE_AND, self.t_i32, cin, self.one);
                    r = self.bin(op::I_SUB, self.t_i32, r, bit);
                }
                let carry = self.bin(op::U_LESS_THAN, self.t_bool, b, a);
                let mask = self.tern(op::SELECT, self.t_i32, carry, self.minus_one, self.zero);
                if let Some(c) = v.carry {
                    self.wdst(&c, mask)?;
                }
                self.wdst(&v.dst.ok_or(Gap("a subtract without a destination"))?, r)
            }
            "v_bfrev_b32" => {
                let a = self.src32(&v.src[0])?;
                let r = self.un(op::BIT_REVERSE, self.t_i32, a);
                self.wdst(&v.dst.ok_or(Gap("a reverse without a destination"))?, r)
            }
            "v_alignbit_b32" => {
                let (a, b, c) = self.src3(&v)?;
                let k32 = self.c32(32);
                let n = self.bin(op::I_SUB, self.t_i32, k32, c);
                let hi = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, b, n);
                let lo = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, a, c);
                let r = self.bin(op::BITWISE_OR, self.t_i32, hi, lo);
                self.wdst(&v.dst.ok_or(Gap("an align without a destination"))?, r)
            }
            "v_sad_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let gt = self.bin(op::U_GREATER_THAN, self.t_bool, a, b);
                let d1 = self.bin(op::I_SUB, self.t_i32, a, b);
                let d2 = self.bin(op::I_SUB, self.t_i32, b, a);
                let d = self.select(gt, d1, d2);
                let r = self.bin(op::I_ADD, self.t_i32, d, c);
                self.wdst(&v.dst.ok_or(Gap("a sad without a destination"))?, r)
            }
            "v_med3_i32" | "v_med3_u32" => {
                let (smin, smax) = if name.ends_with("u32") {
                    (glsl::UMIN, glsl::UMAX)
                } else {
                    (glsl::SMIN, glsl::SMAX)
                };
                let a = self.src32(&v.src[0])?;
                let b = self.src32(&v.src[1])?;
                let c = self.src32(&v.src[2])?;
                let lo = self.minmax(smin, a, b);
                let hi = self.minmax(smax, a, b);
                let t = self.minmax(smin, hi, c);
                let r = self.minmax(smax, lo, t);
                self.wdst(&v.dst.ok_or(Gap("a median without a destination"))?, r)
            }
            "v_min3_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let lo = self.minmax(glsl::UMIN, a, b);
                let r = self.minmax(glsl::UMIN, lo, c);
                self.wdst(&v.dst.ok_or(Gap("a min without a destination"))?, r)
            }
            "v_max3_u32" => {
                let (a, b, c) = self.src3(&v)?;
                let hi = self.minmax(glsl::UMAX, a, b);
                let r = self.minmax(glsl::UMAX, hi, c);
                self.wdst(&v.dst.ok_or(Gap("a max without a destination"))?, r)
            }
            // First set bit, low end for the unsigned form and high end for
            // the signed one (which looks at the operand's sign bit).
            "v_ffbl_b32" => {
                let a = self.src32(&v.src[0])?;
                let i = self.as_s32(a);
                let r = self.ext1t(glsl::FIND_I_LSB, self.t_i32, i);
                self.wdst(&v.dst.ok_or(Gap("a find without a destination"))?, r)
            }
            "v_ffbh_i32" => {
                let a = self.src32(&v.src[0])?;
                let i = self.as_s32(a);
                let r = self.ext1t(glsl::FIND_S_MSB, self.t_i32, i);
                self.wdst(&v.dst.ok_or(Gap("a find without a destination"))?, r)
            }
            "v_min_u32" => self.iv_ext2(&v, glsl::UMIN),
            "v_max_u32" => self.iv_ext2(&v, glsl::UMAX),
            "v_min_i32" => self.iv_ext2(&v, glsl::SMIN),
            "v_max_i32" => self.iv_ext2(&v, glsl::SMAX),
            // Conversions.
            "v_cvt_f32_u32" => self.conv(&v, op::CONVERT_U_TO_F),
            "v_cvt_f32_i32" => self.conv(&v, op::CONVERT_S_TO_F),
            "v_cvt_u32_f32" => self.conv(&v, op::CONVERT_F_TO_U),
            "v_cvt_i32_f32" => self.conv(&v, op::CONVERT_F_TO_S),
            "v_cvt_f32_ubyte0" => self.cvt_ubyte(&v, 0),
            "v_cvt_f32_ubyte1" => self.cvt_ubyte(&v, 1),
            "v_cvt_f32_ubyte2" => self.cvt_ubyte(&v, 2),
            "v_cvt_f32_ubyte3" => self.cvt_ubyte(&v, 3),
            "v_cvt_f32_f16" => {
                let a = self.src32(&v.src[0])?;
                let h = self.u32_to_u16(a);
                let f = self.un(op::BITCAST, self.t_f16, h);
                let r = self.un(op::F_CONVERT, self.t_f32, f);
                self.fv_out(&v, r)
            }
            "v_cvt_f16_f32" => {
                let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
                let h = self.un(op::F_CONVERT, self.t_f16, a);
                let i = self.un(op::BITCAST, self.t_u16, h);
                let r = self.u16_to_u32(i);
                self.wdst(&v.dst.ok_or(Gap("a convert without a destination"))?, r)
            }
            _ => Err(Gap(name)),
        }
    }

    fn src3(&mut self, v: &crate::gfx10::Valu) -> R<(u32, u32, u32)> {
        if v.src.len() < 3 {
            return gap("a three-source operation with fewer sources");
        }
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let c = self.src32(&v.src[2])?;
        Ok((a, b, c))
    }

    fn fv_bin(&mut self, v: &crate::gfx10::Valu, code: u16) -> R<()> {
        let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
        let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
        let r = self.bin(code, self.t_f32, a, b);
        self.fv_out(v, r)
    }

    fn fv_bin_rev(&mut self, v: &crate::gfx10::Valu, code: u16) -> R<()> {
        let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
        let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
        let r = self.bin(code, self.t_f32, b, a);
        self.fv_out(v, r)
    }

    fn fv_ext1(&mut self, v: &crate::gfx10::Valu, ext: u32) -> R<()> {
        let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
        let r = self.ext1(ext, a);
        self.fv_out(v, r)
    }

    fn fv_ext2(&mut self, v: &crate::gfx10::Valu, ext: u32) -> R<()> {
        let a = self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?;
        let b = self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?;
        let r = self.ext2(ext, self.t_f32, a, b);
        self.fv_out(v, r)
    }

    /// The destination-modifier (clamp, ×2/×4/÷2) and the write.
    fn fv_out(&mut self, v: &crate::gfx10::Valu, mut f: u32) -> R<()> {
        if v.clamp {
            let zero = self.cf32(0.0);
            let one = self.cf32(1.0);
            let id = self.m.id();
            self.ins(
                op::EXT_INST,
                self.t_f32,
                id,
                &[self.ext, glsl::FCLAMP, f, zero, one],
            );
            f = id;
        }
        f = match v.omod {
            0 => f,
            1 => {
                let two = self.cf32(2.0);
                self.bin(op::F_MUL, self.t_f32, f, two)
            }
            2 => {
                let four = self.cf32(4.0);
                self.bin(op::F_MUL, self.t_f32, f, four)
            }
            3 => {
                let two = self.cf32(2.0);
                self.bin(op::F_DIV, self.t_f32, f, two)
            }
            _ => return gap("an output modifier this emitter does not know"),
        };
        self.wdst_f32(&v.dst.ok_or(Gap("an operation without a destination"))?, f)
    }

    fn iv_bin(&mut self, v: &crate::gfx10::Valu, code: u16) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let r = self.bin(code, self.t_i32, a, b);
        self.wdst(&v.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn iv_bin_rev(&mut self, v: &crate::gfx10::Valu, code: u16) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let r = self.bin(code, self.t_i32, b, a);
        self.wdst(&v.dst.ok_or(Gap("an operation without a destination"))?, r)
    }

    fn iv_ext2(&mut self, v: &crate::gfx10::Valu, ext: u32) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let r = self.minmax(ext, a, b);
        self.wdst(&v.dst.ok_or(Gap("a min or max without a destination"))?, r)
    }

    fn conv(&mut self, v: &crate::gfx10::Valu, code: u16) -> R<()> {
        if v.clamp {
            return gap("a clamped conversion");
        }
        let a = self.src32(&v.src[0])?;
        let r = match code {
            op::CONVERT_U_TO_F => {
                let u = self.un(op::BITCAST, self.t_u32, a);
                self.un(op::CONVERT_U_TO_F, self.t_f32, u)
            }
            op::CONVERT_F_TO_U => {
                let fa = self.bitcast_f32(a);
                let f = self.un(op::CONVERT_F_TO_U, self.t_u32, fa);
                self.un(op::BITCAST, self.t_i32, f)
            }
            op::CONVERT_S_TO_F => {
                let sv = self.as_s32(a);
                self.un(op::CONVERT_S_TO_F, self.t_f32, sv)
            }
            op::CONVERT_F_TO_S => {
                let fa = self.bitcast_f32(a);
                self.un(op::CONVERT_F_TO_S, self.t_i32, fa)
            }
            _ => self.un(code, self.t_f32, a),
        };
        let r = self.bitcast_i32(r);
        self.wdst(&v.dst.ok_or(Gap("a convert without a destination"))?, r)
    }

    fn cvt_ubyte(&mut self, v: &crate::gfx10::Valu, byte: u32) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let sh = self.c32(8 * byte);
        let b = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i32, a, sh);
        let ff = self.c32(0xff);
        let b = self.bin(op::BITWISE_AND, self.t_i32, b, ff);
        let ub = self.un(op::BITCAST, self.t_u32, b);
        let f = self.un(op::CONVERT_U_TO_F, self.t_f32, ub);
        self.fv_out(v, f)
    }

    fn add_carry(&mut self, v: &crate::gfx10::Valu, sub: bool) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let mut r = self.bin(if sub { op::I_SUB } else { op::I_ADD }, self.t_i32, a, b);
        // Carry/borrow in, when the VOP3 form has a third source.
        if v.src.len() > 2 {
            let cin = self.src32(&v.src[2])?;
            let bit = self.bin(op::BITWISE_AND, self.t_i32, cin, self.one);
            r = self.bin(if sub { op::I_SUB } else { op::I_ADD }, self.t_i32, r, bit);
        }
        let (ua, ub) = (self.as_u32(a), self.as_u32(b));
        let carry = if sub {
            self.bin(op::U_LESS_THAN, self.t_bool, ua, ub)
        } else {
            let ur = self.as_u32(r);
            self.bin(op::U_LESS_THAN, self.t_bool, ur, ua)
        };
        let mask = self.tern(op::SELECT, self.t_i32, carry, self.minus_one, self.zero);
        if let Some(c) = v.carry {
            self.wdst(&c, mask)?;
        }
        self.wdst(
            &v.dst
                .ok_or(Gap("a carry operation without a destination"))?,
            r,
        )
    }

    fn mul_hi(&mut self, v: &crate::gfx10::Valu, signed: bool) -> R<()> {
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let (a, b) = if signed {
            (self.sext64(a), self.sext64(b))
        } else {
            (self.u32_to_u64(a), self.u32_to_u64(b))
        };
        let p = self.bin(op::I_MUL, self.t_i64, a, b);
        let shift = self.c64(32);
        let hi = self.bin(op::SHIFT_RIGHT_LOGICAL, self.t_i64, p, shift);
        let hi = if signed {
            let u = self.un(op::BITCAST, self.t_u64, hi);
            let l = self.un(op::U_CONVERT, self.t_u32, u);
            self.un(op::BITCAST, self.t_i32, l)
        } else {
            self.u64_to_u32(hi)
        };
        self.wdst(&v.dst.ok_or(Gap("a multiply without a destination"))?, hi)
    }

    fn mul_u24(&mut self, v: &crate::gfx10::Valu) -> R<()> {
        let mask = self.c32(0x00ff_ffff);
        let a = self.src32(&v.src[0])?;
        let b = self.src32(&v.src[1])?;
        let a = self.bin(op::BITWISE_AND, self.t_i32, a, mask);
        let b = self.bin(op::BITWISE_AND, self.t_i32, b, mask);
        let r = self.bin(op::I_MUL, self.t_i32, a, b);
        self.wdst(&v.dst.ok_or(Gap("a multiply without a destination"))?, r)
    }

    fn mad_u24(&mut self, v: &crate::gfx10::Valu) -> R<()> {
        let (a, b, c) = self.src3(v)?;
        let mask = self.c32(0x00ff_ffff);
        let a = self.bin(op::BITWISE_AND, self.t_i32, a, mask);
        let b = self.bin(op::BITWISE_AND, self.t_i32, b, mask);
        let p = self.bin(op::I_MUL, self.t_i32, a, b);
        let r = self.bin(op::I_ADD, self.t_i32, p, c);
        self.wdst(
            &v.dst.ok_or(Gap("a multiply-add without a destination"))?,
            r,
        )
    }

    fn mad_i24(&mut self, v: &crate::gfx10::Valu) -> R<()> {
        let (a, b, c) = self.src3(v)?;
        let eight = self.c32(8);
        let a = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, a, eight);
        let a = self.bin(op::SHIFT_RIGHT_ARITHMETIC, self.t_i32, a, eight);
        let b = self.bin(op::SHIFT_LEFT_LOGICAL, self.t_i32, b, eight);
        let b = self.bin(op::SHIFT_RIGHT_ARITHMETIC, self.t_i32, b, eight);
        let p = self.bin(op::I_MUL, self.t_i32, a, b);
        let r = self.bin(op::I_ADD, self.t_i32, p, c);
        self.wdst(
            &v.dst.ok_or(Gap("a multiply-add without a destination"))?,
            r,
        )
    }

    fn vbfe(&mut self, v: &crate::gfx10::Valu, signed: bool) -> R<()> {
        let (data, off, width) = self.src3(v)?;
        let pred = if signed {
            op::BIT_FIELD_S_EXTRACT
        } else {
            op::BIT_FIELD_U_EXTRACT
        };
        let r = self.tern(pred, self.t_i32, data, off, width);
        self.wdst(&v.dst.ok_or(Gap("a bitfield without a destination"))?, r)
    }

    fn valu_compare(&mut self, v: &crate::gfx10::Valu, rest: &str, exec: bool) -> R<()> {
        let (kind, ty) = rest
            .split_once('_')
            .ok_or(Gap("a compare name this emitter does not know"))?;
        let (a_ty, int) = match ty {
            "u32" | "i32" => ("i32", true),
            "f32" => ("f32", false),
            _ => return gap("a compare width this emitter does not implement"),
        };
        let pred = if int {
            int_pred(kind, ty == "i32").ok_or(Gap("a compare kind this emitter does not know"))?
        } else {
            f_pred(kind).ok_or(Gap("a compare kind this emitter does not know"))?
        };
        let unsigned = matches!(
            pred,
            op::U_GREATER_THAN | op::U_GREATER_THAN_EQUAL | op::U_LESS_THAN | op::U_LESS_THAN_EQUAL
        );
        let (a, b) = if a_ty == "f32" {
            (
                self.srcf32(&v.src[0], v.neg & 1 != 0, v.abs & 1 != 0)?,
                self.srcf32(&v.src[1], v.neg & 2 != 0, v.abs & 2 != 0)?,
            )
        } else {
            let (a, b) = (self.src32(&v.src[0])?, self.src32(&v.src[1])?);
            if unsigned {
                (self.as_u32(a), self.as_u32(b))
            } else if a_ty == "i32" {
                (self.as_s32(a), self.as_s32(b))
            } else {
                (a, b)
            }
        };
        let _ = a_ty;
        let c = self.bin(pred, self.t_bool, a, b);
        let mask = self.tern(op::SELECT, self.t_i32, c, self.minus_one, self.zero);
        let dst = v.dst.ok_or(Gap("a compare without a destination"))?;
        self.wdst(&dst, mask)?;
        if exec {
            self.store(126, mask);
            self.store(127, mask);
        }
        Ok(())
    }
}

/// Whether an `opsel` source half select applies to this mnemonic: a 32-bit
/// or narrower operation (the 64-bit and f64 forms have their own rules).
fn is_half_selectable(name: &str) -> bool {
    ["_b32", "_f32", "_i32", "_u32", "_b16", "_f16", "_i16", "_u16"]
        .iter()
        .any(|s| name.ends_with(s))
        && !name.contains("_b64")
        && !name.contains("_f64")
}

fn is_null(o: &Operand) -> bool {
    matches!(o, Operand::Const(125) | Operand::Scalar { code: 125, .. })
}

/// A scalar instruction's operands.
struct Sop {
    dst: Option<Operand>,
    src: Vec<Operand>,
    imm: Option<u16>,
}

fn int_pred(kind: &str, signed: bool) -> Option<u16> {
    Some(match (kind, signed) {
        ("eq", _) => op::I_EQUAL,
        ("ne", _) | ("lg", _) => op::I_NOT_EQUAL,
        ("lt", false) => op::U_LESS_THAN,
        ("le", false) => op::U_LESS_THAN_EQUAL,
        ("gt", false) => op::U_GREATER_THAN,
        ("ge", false) => op::U_GREATER_THAN_EQUAL,
        ("lt", true) => op::S_LESS_THAN,
        ("le", true) => op::S_LESS_THAN_EQUAL,
        ("gt", true) => op::S_GREATER_THAN,
        ("ge", true) => op::S_GREATER_THAN_EQUAL,
        _ => return None,
    })
}

fn f_pred(kind: &str) -> Option<u16> {
    Some(match kind {
        "eq" => op::F_ORD_EQUAL,
        "ne" => op::F_ORD_NOT_EQUAL,
        "lg" => op::F_UNORD_NOT_EQUAL,
        "lt" => op::F_ORD_LESS_THAN,
        "le" => op::F_ORD_LESS_THAN_EQUAL,
        "gt" => op::F_ORD_GREATER_THAN,
        "ge" => op::F_ORD_GREATER_THAN_EQUAL,
        "nge" => op::F_UNORD_LESS_THAN,
        "nle" => op::F_UNORD_GREATER_THAN,
        "nlt" => op::F_UNORD_GREATER_THAN_EQUAL,
        "ngt" => op::F_UNORD_LESS_THAN_EQUAL,
        _ => return None,
    })
}
