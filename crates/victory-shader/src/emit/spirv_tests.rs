//! Hand-assembled GFX10 words for the SPIR-V emitter's newer families: the
//! scalar carry chain (SCC used as a value), 64-bit shifts, bit counts, the
//! first/last-bit searches, three-source integer medians, VALU source
//! modifiers (VOP3 `neg`/`abs`, SDWA field selection, DPP) and one image
//! sample. No game data: each program is a handful of words built here, so
//! a failure names the instruction family the emitter broke rather than a
//! hash nobody can read.
//!
//! Each module is checked two ways: [`assert_operand_types`], which catches
//! an `i1` reaching an integer operation (the class `spirv-val` reports as
//! "operand index 2 expects int"), and `spirv-val` itself when the tool is
//! installed (`spirv-tools` is not a build requirement; the spike's
//! `offline/spirv-check.sh` runs it over the whole dump as well).

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use super::spirv;
use crate::io::Io;
use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::{gfx10, ir};

const S_ENDPGM: u32 = 0xBF81_0000;
/// VOP sources: 0..=105 are SGPRs, 128.. inline constants, 256.. VGPRs.
const V0: u16 = 256;

/// SOP2 `op` from the table: `sdst, ssrc0, ssrc1`.
const fn sop2(op: u32, sdst: u8, ssrc1: u8, ssrc0: u8) -> u32 {
    0x8000_0000 | op << 23 | (sdst as u32) << 16 | (ssrc1 as u32) << 8 | ssrc0 as u32
}

/// SOP1 `op` from the table: `sdst, ssrc0`.
const fn sop1(op: u32, sdst: u8, ssrc0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | op << 8 | ssrc0 as u32
}

/// The same word, reading its source through SDWA (`src_sel`, and the
/// destination masking `dst_sel`/`dst_unused`): the two-word form.
const fn v_mov_sdwa(vdst: u8, src_sel: u32, dst_sel: u32) -> [u32; 2] {
    [
        0x7E00_0000 | (vdst as u32) << 17 | 0x01 << 9 | 0xF9,
        dst_sel << 8 | src_sel << 16 | src_sel << 24,
    ]
}

/// `v_mov_b32 vdst, v0` in its DPP16 form, quad-perm 0xE4 (identity).
const fn v_mov_dpp(vdst: u8) -> [u32; 2] {
    [
        0x7E00_0000 | (vdst as u32) << 17 | 0x01 << 9 | 0xFA,
        0xE4 << 8,
    ]
}

/// VOP3 `op`: `vdst, vsrc0, vsrc1, vsrc2` with the `neg`/`abs` bit per
/// source (bit 0 is the first source).
const fn vop3(op: u32, vdst: u8, src: [u16; 3], neg: u32, abs: u32) -> [u32; 2] {
    [
        0xD400_0000 | op << 16 | abs << 8 | vdst as u32,
        (src[0] as u32 & 0x1FF)
            | (src[1] as u32 & 0x1FF) << 9
            | (src[2] as u32 & 0x1FF) << 18
            | neg << 29,
    ]
}

/// MIMG `image_sample v[vdata:], v[vaddr:], s[4:7], s[8:11]` (`dmask`, `dim`).
const fn image_sample(vdata: u8, vaddr: u8, dmask: u32, dim: u32) -> [u32; 2] {
    [
        0x3C << 26 | 0x20 << 18 | dmask << 8 | dim << 3,
        vaddr as u32 | (vdata as u32) << 8 | (16 >> 2) << 16 | (24 >> 2) << 21,
    ]
}

// ---- the program and module under test --------------------------------------

fn program(words: &[u32]) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// The names the hand-built words decode to, so a mis-encoded word fails
/// here instead of looking like an emitter bug.
fn names(words: &[u32]) -> Vec<&'static str> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    gfx10::decode(&code)
        .expect("the hand-built words decode")
        .iter()
        .map(|i| i.name().unwrap_or("?"))
        .collect()
}

/// The emitted module, with the operand types and `spirv-val` checked.
fn module(words: &[u32]) -> Vec<u32> {
    program_of(words, false, [64, 1, 1]).words
}

/// The emitted module with its metadata: `wave64`, thread-group size.
fn program_of(words: &[u32], wave64: bool, threads: [u32; 3]) -> spirv::SpirvProgram {
    let mut native = program(words);
    native.wave64 = wave64;
    native.io = StageIo::Cs(ComputeIo { rsrc2: 0, threads });
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let prog = spirv::emit(&native, &p, &Io::default()).expect("the program emits");
    assert_operand_types(&prog.words, &format!("{words:08x?}"));
    assert_valid(&prog.words, &format!("{words:08x?}"));
    prog
}

// ---- reading a module back --------------------------------------------------

const TYPE_BOOL: u16 = 20;
const TYPE_INT: u16 = 21;
const TYPE_FLOAT: u16 = 22;
const DECORATE: u16 = 71;
const BINDING: u32 = 33;
const EXT_INST: u16 = 12;

/// The SPIR-V opcodes these tests name (`spirv`'s constants are private).
mod opin {
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
    pub const SELECT: u16 = 169;
    pub const I_EQUAL: u16 = 170;
    pub const I_NOT_EQUAL: u16 = 171;
    pub const F_ORD_LESS_THAN: u16 = 184;
    pub const F_ORD_GREATER_THAN: u16 = 186;
    pub const SHIFT_RIGHT_LOGICAL: u16 = 194;
    pub const SHIFT_RIGHT_ARITHMETIC: u16 = 195;
    pub const SHIFT_LEFT_LOGICAL: u16 = 196;
    pub const BITWISE_OR: u16 = 197;
    pub const BITWISE_XOR: u16 = 198;
    pub const BITWISE_AND: u16 = 199;
    pub const BIT_COUNT: u16 = 205;
    pub const SAMPLED_IMAGE: u16 = 86;
    pub const IMAGE_SAMPLE_EXPLICIT_LOD: u16 = 88;
}

/// The instructions of a module, as (opcode, operands), the header skipped.
fn instructions(m: &[u32]) -> Vec<(u16, Vec<u32>)> {
    let mut out = Vec::new();
    let mut i = 5;
    while i < m.len() {
        let count = (m[i] >> 16) as usize;
        assert!(count > 0 && i + count <= m.len(), "bad instruction length");
        out.push(((m[i] & 0xFFFF) as u16, m[i + 1..i + count].to_vec()));
        i += count;
    }
    out
}

/// Whether the opcode's first two operands are its result type and id.
fn has_result_type(op: u16) -> bool {
    matches!(
        op,
        12 | 43 | 59 | 61 | 65 | 80 | 81 | 86 | 88 | 90 | 95..=100 | 103 | 104 | 106 | 109..=115
            | 120 | 124 | 127..=141 | 167..=191 | 194..=205 | 229 | 234..=242
    )
}

/// Every id's result type.
fn types(m: &[u32]) -> HashMap<u32, u32> {
    let mut t = HashMap::new();
    for (op, ops) in instructions(m) {
        if has_result_type(op) && ops.len() >= 2 {
            t.insert(ops[1], ops[0]);
        }
    }
    t
}

/// The ids of the module's `OpTypeBool` and integer types.
fn type_ids(m: &[u32], kind: u16) -> Vec<u32> {
    instructions(m)
        .into_iter()
        .filter(|(op, _)| *op == kind)
        .filter_map(|(_, ops)| ops.first().copied())
        .collect()
}

/// Whether `id`'s type is a 64-bit integer (`OpTypeInt` width 64).
fn is_i64(m: &[u32], t: &HashMap<u32, u32>, id: u32) -> bool {
    let Some(&ty) = t.get(&id) else {
        return false;
    };
    instructions(m).into_iter().any(|(op, ops)| {
        op == TYPE_INT && ops.first() == Some(&ty) && ops.get(1) == Some(&64)
    })
}

/// The operations whose two operands must be the type they produce. Shifts
/// are separate (SPIR-V lets their count be a different width) and compares
/// produce a flag from two same-typed operands.
const SAME_TYPE_BINARY: &[u16] = &[
    opin::I_ADD,
    opin::F_ADD,
    opin::I_SUB,
    opin::F_SUB,
    opin::I_MUL,
    opin::F_MUL,
    opin::U_DIV,
    opin::S_DIV,
    opin::F_DIV,
    opin::U_MOD,
    opin::S_REM,
    opin::F_REM,
    opin::F_MOD,
    opin::LOGICAL_AND,
    opin::BITWISE_OR,
    opin::BITWISE_XOR,
    opin::BITWISE_AND,
];

const SHIFTS: &[u16] = &[
    opin::SHIFT_RIGHT_LOGICAL,
    opin::SHIFT_RIGHT_ARITHMETIC,
    opin::SHIFT_LEFT_LOGICAL,
];

const COMPARES: &[u16] = &[
    opin::I_EQUAL,
    opin::I_NOT_EQUAL,
    172,
    173,
    174,
    175,
    176,
    177,
    178,
    179,
    180,
    181,
    182,
    183,
    opin::F_ORD_LESS_THAN,
    185,
    opin::F_ORD_GREATER_THAN,
    187,
    188,
    189,
    190,
    191,
];

/// Fails for an operation whose operand is not the type it produces: the
/// class where an `i1` (a compare's result, SCC) reached `OpBitwiseAnd` and
/// `spirv-val` said "operand index 2 expects int".
fn assert_operand_types(m: &[u32], what: &str) {
    let t = types(m);
    for (op, ops) in instructions(m) {
        if ops.len() < 4 {
            continue;
        }
        let (ty, a, b) = (ops[0], ops[2], ops[3]);
        if SAME_TYPE_BINARY.contains(&op) {
            for (k, v) in [a, b].into_iter().enumerate() {
                if let Some(&at) = t.get(&v) {
                    assert_eq!(at, ty, "{what}: {op} operand {k} is not its own type");
                }
            }
        } else if SHIFTS.contains(&op) {
            if let Some(&at) = t.get(&a)
                && at != ty
            {
                panic!("{what}: {op} shifts a value of another type");
            }
        } else if COMPARES.contains(&op)
            && matches!((t.get(&a), t.get(&b)), (Some(x), Some(y)) if x != y)
        {
            panic!("{what}: {op} compares two different types");
        }
    }
}

/// `spirv-val` on the module, when the tool is installed (it is not a
/// build requirement, so a machine without it skips this check).
fn assert_valid(m: &[u32], what: &str) {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "vic-spirv-{}-{}.spv",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes: Vec<u8> = m.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(&path, &bytes).expect("the module is written");
    let out = Command::new("spirv-val")
        .arg("--target-env")
        .arg("vulkan1.3")
        .arg(&path)
        .output();
    match out {
        Ok(o) => {
            // A module that does not validate stays on disk, for spirv-dis.
            if o.status.success() {
                let _ = std::fs::remove_file(&path);
            } else {
                eprintln!("kept {}", path.display());
            }
            assert!(
                o.status.success(),
                "{what}: spirv-val: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Err(e) => eprintln!("no spirv-val ({e}); skipped"),
    }
}

// ---- the words mean what the tests say --------------------------------------

#[test]
fn the_hand_built_words_decode() {
    assert_eq!(
        names(&[sop2(0x00, 2, 2, 2), sop2(0x04, 3, 2, 2), S_ENDPGM]),
        ["s_add_u32", "s_addc_u32", "s_endpgm"]
    );
    assert_eq!(
        names(&[
            sop2(0x1f, 4, 0, 2),
            sop2(0x21, 6, 0, 2),
            sop2(0x23, 8, 0, 2),
            S_ENDPGM
        ]),
        ["s_lshl_b64", "s_lshr_b64", "s_ashr_i64", "s_endpgm"]
    );
    assert_eq!(
        names(&[
            sop1(0x10, 2, 0),
            sop1(0x13, 4, 4),
            sop1(0x17, 6, 6),
            S_ENDPGM
        ]),
        ["s_bcnt1_i32_b64", "s_ff1_i32_b32", "s_flbit_i32", "s_endpgm"]
    );
    assert_eq!(
        names(&[vop3(0x158, 0, [V0 + 1, V0 + 2, V0 + 3], 0, 0)[0],
            vop3(0x158, 0, [V0 + 1, V0 + 2, V0 + 3], 0, 0)[1],
            S_ENDPGM]),
        ["v_med3_i32", "s_endpgm"]
    );
    assert_eq!(
        names(&[vop3(0x103, 0, [V0 + 1, V0 + 2, 0], 1, 2)[0],
            vop3(0x103, 0, [V0 + 1, V0 + 2, 0], 1, 2)[1],
            S_ENDPGM]),
        ["v_add_f32", "s_endpgm"]
    );
    let sdwa = v_mov_sdwa(2, 5, 6);
    assert_eq!(names(&[sdwa[0], sdwa[1], S_ENDPGM]), ["v_mov_b32", "s_endpgm"]);
    let dpp = v_mov_dpp(1);
    assert_eq!(names(&[dpp[0], dpp[1], S_ENDPGM]), ["v_mov_b32", "s_endpgm"]);
    let img = image_sample(0, 3, 1, 1);
    assert_eq!(names(&[img[0], img[1], S_ENDPGM]), ["image_sample", "s_endpgm"]);
}

// ---- the scalar carry chain -------------------------------------------------

/// `s_addc_u32`'s carry-in is SCC, an `i1` in the module: it must be
/// widened to `i32` before it is added, not bitwise-anded as a flag.
#[test]
fn a_carry_in_is_widened_to_an_integer() {
    // s_add_u32 s2, s2, s2; s_addc_u32 s3, s2, s2
    let m = module(&[sop2(0x00, 2, 2, 2), sop2(0x04, 3, 2, 2), S_ENDPGM]);
    let t = types(&m);
    let bools = type_ids(&m, TYPE_BOOL);
    let ints = type_ids(&m, TYPE_INT);
    assert!(!bools.is_empty() && !ints.is_empty());
    // A select from a flag into an integer: the widening.
    let widened = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| {
            *op == opin::SELECT
                && ops.len() == 5
                && t.get(&ops[2]).is_some_and(|b| bools.contains(b))
                && ints.contains(&ops[0])
        })
        .count();
    assert!(widened >= 1, "no OpSelect widening SCC to i32: {m:08x?}");
    // Nothing bitwise-ors/ands a flag: the type check above proves it, but a
    // module that lost the add's own operand would pass that vacuously.
    let adds = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::I_ADD)
        .count();
    assert!(adds >= 2, "the add and its carry-in are missing: {m:08x?}");
}

// ---- wide shifts and bit counting ------------------------------------------

/// `s_lshl_b64`, `s_lshr_b64` and `s_ashr_i64` shift a 64-bit register pair
/// by a 32-bit count: the value keeps its wide type, only the count is
/// narrower.
#[test]
fn sixty_four_bit_shifts_keep_their_width() {
    let m = module(&[
        sop2(0x1f, 4, 0, 2),
        sop2(0x21, 6, 0, 4),
        sop2(0x23, 8, 0, 6),
        S_ENDPGM,
    ]);
    let t = types(&m);
    for (op, ops) in instructions(&m) {
        if ops.len() == 4 && SHIFTS.contains(&op) {
            assert!(is_i64(&m, &t, ops[1]), "{op} does not shift an i64");
        }
    }
    let wide = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| SHIFTS.contains(op) && ops.len() == 4 && is_i64(&m, &t, ops[1]))
        .count();
    assert!(wide >= 3, "not all three shifts are 64-bit: {m:08x?}");
}

/// `s_bcnt1_i32_b64` is the sum of both halves' popcounts.
#[test]
fn a_lane_mask_counts_both_halves() {
    let m = module(&[sop1(0x10, 2, 0), S_ENDPGM]);
    let counts = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::BIT_COUNT)
        .count();
    assert_eq!(counts, 2, "not one popcount per half: {m:08x?}");
    let adds = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::I_ADD)
        .count();
    assert!(adds >= 1, "the halves are not summed: {m:08x?}");
}

/// `s_ff1_i32_b32` and `s_flbit_i32` count with `OpBitCount` (on the
/// isolated bit) and answer -1 for a zero input: no GLSL instruction, which
/// would need a capability, and no 64-bit count.
#[test]
fn the_bit_searches_answer_minus_one_for_zero() {
    let ff1 = module(&[sop1(0x13, 2, 2), S_ENDPGM]);
    let flbit = module(&[sop1(0x17, 4, 4), S_ENDPGM]);
    let ints = type_ids(&ff1, TYPE_INT);
    for m in [&ff1, &flbit] {
        let counts = instructions(m)
            .into_iter()
            .filter(|(op, _)| *op == opin::BIT_COUNT)
            .count();
        let selects = instructions(m)
            .into_iter()
            .filter(|(op, ops)| {
                *op == opin::SELECT && ops.len() == 5 && ints.contains(&ops[0])
            })
            .count();
        assert!(counts >= 1, "no popcount of the isolated bit: {m:08x?}");
        assert!(selects >= 1, "no choice between -1 and the count: {m:08x?}");
    }
    // The last-bit search smears the value right first: more shifts than the
    // first-bit search, which reads the isolated bit directly.
    let shifts = |m: &[u32]| {
        instructions(m)
            .into_iter()
            .filter(|(op, _)| SHIFTS.contains(op))
            .count()
    };
    assert!(shifts(&flbit) > shifts(&ff1));
}

/// `v_med3_i32`: the median of three signed integers, twice a min and twice
/// a max of the same signedness.
#[test]
fn a_median_of_three_integers() {
    let w = vop3(0x158, 0, [V0 + 1, V0 + 2, V0 + 3], 0, 0);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let (smin, smax) = (39u32, 42u32);
    let mut seen = Vec::new();
    for (op, ops) in instructions(&m) {
        if op == EXT_INST && ops.len() >= 4 {
            seen.push(ops[3]);
        }
    }
    let s_mins = seen.iter().filter(|&&i| i == smin).count();
    let s_maxs = seen.iter().filter(|&&i| i == smax).count();
    assert_eq!((s_mins, s_maxs), (2, 2), "not a signed median: {seen:?}");
}

// ---- source modifiers -------------------------------------------------------

/// VOP3 `neg`/`abs` on a float source: the negation is an
/// `OpFNegate`, the absolute value GLSL `FAbs`.
#[test]
fn a_source_is_negated_and_made_absolute() {
    let w = vop3(0x103, 0, [V0 + 1, V0 + 2, V0], 1, 2);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let f32s = type_ids(&m, TYPE_FLOAT);
    let negates = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == opin::F_NEGATE && f32s.contains(&ops[0]))
        .count();
    let fabs = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == EXT_INST && ops.len() >= 4 && ops[3] == 4)
        .count();
    assert_eq!((negates, fabs), (1, 1), "{m:08x?}");
    assert!(instructions(&m)
        .iter()
        .any(|(op, _)| *op == opin::F_ADD));
}

/// SDWA source selection reads a byte, word or dword field of the source and
/// zero- or sign-extends it: shifts and masks on the integer, not a new type.
#[test]
fn an_sdwa_source_selects_a_field() {
    // v_mov_b32 v2, v0 with src_sel byte 1 (bits 8..15), dst_sel dword.
    let w = v_mov_sdwa(2, 1, 6);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let t = types(&m);
    let shifts = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| SHIFTS.contains(op) && ops.len() == 4 && !is_i64(&m, &t, ops[1]))
        .count();
    assert!(shifts >= 1, "the field is not shifted down: {m:08x?}");
    let masks = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::BITWISE_AND)
        .count();
    assert!(masks >= 1, "the field is not masked: {m:08x?}");
}

// ---- images -----------------------------------------------------------------

/// An image sample binds its own descriptor and sampler: one
/// `UniformConstant` image per static site (binding = site index), the
/// sampler after the images, joined with `OpSampledImage`.
#[test]
fn an_image_sample_binds_a_sampled_image() {
    let w = image_sample(0, 3, 1, 1);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let sampled = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::SAMPLED_IMAGE)
        .count();
    let samples = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == opin::IMAGE_SAMPLE_EXPLICIT_LOD)
        .count();
    assert_eq!((sampled, samples), (1, 1), "{m:08x?}");
    let bindings: Vec<u32> = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == DECORATE && ops.len() == 3 && ops[1] == BINDING)
        .map(|(_, ops)| ops[2])
        .collect();
    assert_eq!(bindings, [0, 1], "the image and sampler bindings: {bindings:?}");
    // The image type is sampled with `Unknown` format at f32 components.
    let images = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == 25)
        .count();
    assert_eq!(images, 1, "{m:08x?}");
}


// ---- the long tail added on the unified core --------------------------------

/// SOPK `s_cmpk_*`: one register source and the immediate, sign-extended in
/// the `_i32` forms (the word holds 16 bits).
const fn cmpk(op: u32, sdst: u8, imm: u16) -> u32 {
    0xB000_0000 | op << 23 | (sdst as u32) << 16 | imm as u32
}

#[test]
fn the_hand_built_sopk_and_tail_words_decode() {
    assert_eq!(
        names(&[cmpk(0x005, 2, 0xFFFE), S_ENDPGM]),
        ["s_cmpk_gt_i32", "s_endpgm"]
    );
    assert_eq!(
        names(&[cmpk(0x004, 2, 3), S_ENDPGM]),
        ["s_cmpk_lg_i32", "s_endpgm"]
    );
    let min3 = vop3(0x153, 0, [V0 + 1, V0 + 2, V0 + 3], 0, 0);
    assert_eq!(names(&[min3[0], min3[1], S_ENDPGM]), ["v_min3_u32", "s_endpgm"]);
}

/// `s_cmpk_gt_i32 s2, -2` compares against the sign-extended immediate, not
/// against 0xfffe read as unsigned (which would flip the result).
#[test]
fn a_sopk_compare_sign_extends_its_immediate() {
    let m = module(&[cmpk(0x005, 2, 0xFFFE), S_ENDPGM]);
    // SCC = s2 > -2, so the compare is the signed one...
    let signed = instructions(&m)
        .into_iter()
        .filter(|(op, _)| *op == 173) // OpSGreaterThan
        .count();
    assert_eq!(signed, 1, "not a signed compare: {m:08x?}");
    // ...and its second operand is the constant 0xfffffffe.
    let constants: Vec<u32> = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == 43 && ops.len() == 3)
        .map(|(_, ops)| ops[2])
        .collect();
    assert!(
        constants.contains(&0xFFFF_FFFE),
        "the immediate is not sign-extended: {constants:08x?}"
    );
}

/// `v_min3_u32`: the smallest of three unsigned integers, as two
/// `UMin` instructions.
#[test]
fn a_minimum_of_three_integers() {
    let w = vop3(0x153, 0, [V0 + 1, V0 + 2, V0 + 3], 0, 0);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let umins = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == EXT_INST && ops.len() >= 4 && ops[3] == 38)
        .count();
    assert_eq!(umins, 2, "not a minimum of three: {m:08x?}");
}

/// `v_ffbl_b32` and `v_ffbh_i32` are GLSL `FindILsb`/`FindSMsb`: the bit
/// position, -1 for a zero (or all-ones) input, no capability needed.
#[test]
fn the_vector_bit_searches_use_the_glsl_ext() {
    for (op, ext) in [(0x03au32, 73u32), (0x03b, 75)] {
        // VOP1 `v_ffb*_b32 v0, v1`
        let word = 0x7E00_0000 | op << 9 | 1 << 17 | u32::from(V0);
        let m = module(&[word, S_ENDPGM]);
        let found = instructions(&m)
            .into_iter()
            .filter(|(o, ops)| *o == EXT_INST && ops.len() >= 4 && ops[3] == ext)
            .count();
        assert_eq!(found, 1, "no ext {ext} for VOP1 {op:#05x}: {m:08x?}");
    }
}

// ---- the console's wave size (subgroup pinning) -----------------------------

const EXECUTION_MODE_OP: u16 = 16;
const CAPABILITY_OP: u16 = 17;
const LOCAL_SIZE: u32 = 17;

/// The declared `LocalSize` of the module's entry point.
fn local_size(m: &[u32]) -> Vec<u32> {
    instructions(m)
        .into_iter()
        .find(|(op, ops)| *op == EXECUTION_MODE_OP && ops.len() == 5 && ops[1] == LOCAL_SIZE)
        .map(|(_, ops)| ops[2..].to_vec())
        .expect("an OpExecutionMode LocalSize")
}

/// The declared capabilities.
fn capabilities(m: &[u32]) -> Vec<u32> {
    instructions(m)
        .into_iter()
        .filter(|(op, _)| *op == CAPABILITY_OP)
        .map(|(_, ops)| ops[0])
        .collect()
}

/// The module says which console wave it runs, so the pipeline can pin the
/// subgroup to it: `SpirvProgram::{wave_size, local_size}` travel with the
/// words, and `LocalSize` is the program's own group size.
#[test]
fn the_wave_size_travels_with_the_module() {
    let words = [v_mov_dpp(1)[0], v_mov_dpp(1)[1], S_ENDPGM];
    let wave64 = program_of(&words, true, [64, 1, 1]);
    assert_eq!(wave64.wave_size, 64);
    assert_eq!(wave64.local_size, [64, 1, 1]);
    assert_eq!(local_size(&wave64.words), [64, 1, 1]);
    assert!(wave64.full_subgroups(), "64 threads, one 64-lane subgroup");

    let wave32 = program_of(&words, false, [64, 1, 1]);
    assert_eq!(wave32.wave_size, 32);
    assert!(wave32.full_subgroups(), "64 threads, two 32-lane subgroups");
}

/// Cross-lane code needs the group-non-uniform capabilities, on both wave
/// sizes: DPP row ops become shuffles inside the console's wave.
#[test]
fn a_cross_lane_program_declares_group_non_uniform() {
    let words = [v_mov_dpp(1)[0], v_mov_dpp(1)[1], S_ENDPGM];
    for wave64 in [true, false] {
        let prog = program_of(&words, wave64, [64, 1, 1]);
        let caps = capabilities(&prog.words);
        assert!(caps.contains(&61), "GroupNonUniform ({wave64})");
        assert!(caps.contains(&65), "GroupNonUniformShuffle ({wave64})");
    }
    // A program that stays inside its own lane asks for neither.
    let prog = program_of(&[sop2(0x00, 2, 2, 2), S_ENDPGM], false, [64, 1, 1]);
    let caps = capabilities(&prog.words);
    assert!(!caps.contains(&61), "no cross-lane code");
    assert!(!caps.contains(&65), "no cross-lane code");
}

/// `REQUIRE_FULL_SUBGROUPS` needs a whole number of subgroups per group: a
/// 32-thread group run in 64-lane waves cannot have one, and the metadata has
/// to say so rather than the flag being dropped quietly.
#[test]
fn a_group_smaller_than_the_wave_reports_no_full_subgroups() {
    let prog = program_of(&[S_ENDPGM], true, [32, 1, 1]);
    assert_eq!(prog.wave_size, 64);
    assert_eq!(prog.local_size, [32, 1, 1]);
    assert!(!prog.full_subgroups());
}

/// `v_mov_b32 vdst, v0` in its DPP16 form: the 9-bit control, the row and
/// bank masks, and `bound_ctrl` (an out-of-row lane reads itself).
const fn v_mov_dpp_at(
    vdst: u8,
    ctrl: u32,
    row_mask: u32,
    bank_mask: u32,
    bound: bool,
) -> [u32; 2] {
    [
        0x7E00_0000 | (vdst as u32) << 17 | 0x01 << 9 | 0xFA,
        ctrl << 8 | (row_mask & 0xF) << 28 | (bank_mask & 0xF) << 24 | (bound as u32) << 19,
    ]
}

/// `v_min_u32 v1, v0, v0` with a DPP16 second word: the first source is the
/// permuted one, the second (the same register) is the lane's own.
const fn v_min_dpp(vdst: u8, ctrl: u32) -> [u32; 2] {
    [
        0x013 << 25 | (vdst as u32) << 17 | 0xFA,
        ctrl << 8 | 0xF << 28 | 0xF << 24,
    ]
}

/// The module's count of an opcode.
fn ops_count(m: &[u32], op: u16) -> usize {
    instructions(m).iter().filter(|(o, _)| *o == op).count()
}

/// The value the constant `id` defines, when it is one (`OpConstant`).
fn constant_of(m: &[u32], id: u32) -> Option<u32> {
    instructions(m)
        .into_iter()
        .find(|(op, ops)| *op == 43 && ops.len() == 3 && ops[1] == id)
        .map(|(_, ops)| ops[2])
}

/// The opcode of the instruction that defines `id`.
fn defining_op(m: &[u32], id: u32) -> Option<u16> {
    for (op, ops) in instructions(m) {
        // A type (and a label) starts its operands with its id, a value with
        // its result type.
        let at = if (19..=39).contains(&op) || op == 248 {
            Some(0)
        } else if has_result_type(op) {
            Some(1)
        } else {
            None
        };
        if let Some(at) = at
            && ops.get(at) == Some(&id)
        {
            return Some(op);
        }
    }
    None
}

/// The emitter's refusal, for a program it cannot model.
fn refusal(words: &[u32], wave64: bool) -> String {
    let mut native = program(words);
    native.wave64 = wave64;
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    match spirv::emit(&native, &p, &Io::default()) {
        Ok(_) => panic!("the program emitted"),
        Err(e) => format!("{e}"),
    }
}

mod dpp {
    pub const SHUFFLE: u16 = 345;
    pub const SHUFFLE_XOR: u16 = 346;
    pub const QUAD_SWAP: u16 = 366;
    pub const SELECT: u16 = 169;
    pub const LESS_THAN: u16 = 176;
    /// `GLSL.std.450` `UMin`, the ext instruction `row_share` counts with.
    pub const EXT_INST: u16 = 12;
    pub const U_MIN: u32 = 38;
    pub const VECTOR_EXTRACT_DYNAMIC: u16 = 77;
    pub const CONSTANT_COMPOSITE: u16 = 44;
    pub const CAP_QUAD: u32 = 68;
}

/// The hand-built words carry the control, the masks and `bound_ctrl` the
/// emitter reads: a mis-encoded word must fail here, not look like an
/// emitter bug.
#[test]
fn the_hand_built_dpp_words_decode_with_their_controls() {
    let w = v_mov_dpp_at(1, 0x104, 0x3, 0x5, true);
    let add = v_min_dpp(1, 0x141);
    let code: Vec<u8> = [w[0], w[1], add[0], add[1], S_ENDPGM]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let insts = gfx10::decode(&code).expect("the hand-built words decode");
    assert_eq!(insts[0].name(), Some("v_mov_b32"));
    assert_eq!(insts[1].name(), Some("v_min_u32"));
    let gfx10::Fields::Valu(v) = insts[0].fields(false) else {
        panic!("a move is a VALU instruction");
    };
    assert_eq!(
        v.dpp,
        Some(gfx10::Dpp::Row {
            ctrl: 0x104,
            row_mask: 0x3,
            bank_mask: 0x5,
            bound_ctrl: true,
            fi: false,
        })
    );
    let gfx10::Fields::Valu(a) = insts[1].fields(false) else {
        panic!("a minimum is a VALU instruction");
    };
    assert!(matches!(
        a.dpp,
        Some(gfx10::Dpp::Row { ctrl: 0x141, .. })
    ));
}

/// `quad_perm:[0,1,2,3]` reads the lane's own value: no cross-lane
/// instruction at all.
#[test]
fn an_identity_quad_permutation_reads_the_lane_itself() {
    let w = v_mov_dpp_at(1, 0xE4, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 0);
    assert_eq!(ops_count(&m, dpp::SHUFFLE_XOR), 0);
    assert_eq!(ops_count(&m, dpp::QUAD_SWAP), 0);
    // The lane id is an input the module declares either way; the value
    // written is the source's own.
    assert!(!capabilities(&m).contains(&dpp::CAP_QUAD), "no quad swap here");
}

/// The three permutations that swap all four lanes of a quad are
/// `OpGroupNonUniformQuadSwap` with the direction the ISA names (horizontal,
/// vertical, diagonal), and that needs the quad capability.
#[test]
fn a_whole_quad_permutation_is_a_quad_swap() {
    for (ctrl, dir) in [(0xB1u32, 0u32), (0x4E, 1), (0x1B, 2)] {
        let w = v_mov_dpp_at(1, ctrl, 0xF, 0xF, false);
        let m = module(&[w[0], w[1], S_ENDPGM]);
        let swaps: Vec<u32> = instructions(&m)
            .into_iter()
            .filter(|(op, _)| *op == dpp::QUAD_SWAP)
            .map(|(_, ops)| constant_of(&m, ops[4]).expect("a constant direction"))
            .collect();
        assert_eq!(swaps, [dir], "ctrl {ctrl:#x}: {m:08x?}");
        assert_eq!(ops_count(&m, dpp::SHUFFLE), 0, "ctrl {ctrl:#x} is a swap");
        assert!(capabilities(&m).contains(&dpp::CAP_QUAD), "ctrl {ctrl:#x}");
    }
}

/// A quad permutation that is not a whole-quad swap is a shuffle with the
/// lane number read from the quad's own table, indexed by the lane's place in
/// the quad.
#[test]
fn a_mixed_quad_permutation_reads_a_lane_table() {
    // quad_perm:[0,2,1,3]
    let w = v_mov_dpp_at(1, 0xD8, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1, "{m:08x?}");
    assert_eq!(ops_count(&m, dpp::VECTOR_EXTRACT_DYNAMIC), 1);
    let (_, table) = instructions(&m)
        .into_iter()
        .find(|(op, _)| *op == dpp::CONSTANT_COMPOSITE)
        .expect("the permutation table");
    let parts: Vec<u32> = table[2..]
        .iter()
        .filter_map(|id| constant_of(&m, *id))
        .collect();
    assert_eq!(parts, [0, 2, 1, 3], "the permutation's four lanes");
}

/// `row_shl:4` reads lane i + 4 of the row; past the row's end a lane reads
/// zero, or itself under `bound_ctrl`.
#[test]
fn a_row_shift_left_reads_up_the_row_and_zero_past_its_end() {
    for bound in [false, true] {
        let w = v_mov_dpp_at(1, 0x104, 0xF, 0xF, bound);
        let m = module(&[w[0], w[1], S_ENDPGM]);
        assert_eq!(ops_count(&m, dpp::SHUFFLE), 1, "bound_ctrl {bound}");
        assert_eq!(ops_count(&m, dpp::LESS_THAN), 1, "the row's end");
        let (_, sel) = instructions(&m)
            .into_iter()
            .find(|(op, _)| *op == dpp::SELECT)
            .expect("the lanes past the row's end");
        let past = sel[4];
        if bound {
            assert_eq!(constant_of(&m, past), None, "bound_ctrl reads the lane");
        } else {
            assert_eq!(constant_of(&m, past), Some(0), "zero past the row's end");
        }
    }
}

/// `row_shr` and `row_ror`: a shift reads zero (or the lane itself) below the
/// row's start, a rotate wraps and so never leaves the row.
#[test]
fn a_row_shift_right_and_a_rotate_stay_inside_the_row() {
    let w = v_mov_dpp_at(1, 0x114, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1);
    assert_eq!(ops_count(&m, dpp::SELECT), 1, "zero below the row's start");

    let w = v_mov_dpp_at(1, 0x124, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1, "row_ror is a shuffle");
    assert_eq!(ops_count(&m, dpp::SELECT), 0, "a rotate never leaves the row");
}

/// `row_mirror`, `row_half_mirror` and `row_xmask` are one xor inside the
/// row, which is `OpGroupNonUniformShuffleXor` with that mask.
#[test]
fn the_row_mirrors_and_the_lane_xor_are_shuffle_xors() {
    for (ctrl, mask) in [(0x140u32, 15u32), (0x141, 8), (0x163, 3)] {
        let w = v_mov_dpp_at(1, ctrl, 0xF, 0xF, false);
        let m = module(&[w[0], w[1], S_ENDPGM]);
        let masks: Vec<u32> = instructions(&m)
            .into_iter()
            .filter(|(op, _)| *op == dpp::SHUFFLE_XOR)
            .map(|(_, ops)| constant_of(&m, ops[4]).expect("a constant mask"))
            .collect();
        assert_eq!(masks, [mask], "ctrl {ctrl:#x}: {m:08x?}");
        assert_eq!(ops_count(&m, dpp::SHUFFLE), 0, "ctrl {ctrl:#x}");
    }
}

/// `row_bcast15` broadcasts the last lane of each row; `row_bcast31` the last
/// lane of a 64-lane wave, which a 32-lane wave does not have.
#[test]
fn the_row_broadcasts_name_one_lane() {
    let w = v_mov_dpp_at(1, 0x142, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1, "{m:08x?}");

    let w = v_mov_dpp_at(1, 0x143, 0xF, 0xF, false);
    let prog = program_of(&[w[0], w[1], S_ENDPGM], true, [64, 1, 1]);
    let ids: Vec<u32> = instructions(&prog.words)
        .into_iter()
        .filter(|(op, _)| *op == dpp::SHUFFLE)
        .map(|(_, ops)| ops[4])
        .collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(constant_of(&prog.words, ids[0]), Some(31), "lane 31");
    let why = refusal(&[w[0], w[1], S_ENDPGM], false);
    assert!(why.contains("row_bcast31"), "a 32-lane wave has no lane 31: {why}");
}

/// `row_share` holds at the row's last lane instead of reading past it.
#[test]
fn row_share_stops_at_the_last_lane() {
    let w = v_mov_dpp_at(1, 0x152, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1);
    let mins = instructions(&m)
        .into_iter()
        .filter(|(op, ops)| *op == dpp::EXT_INST && ops.len() >= 4 && ops[3] == dpp::U_MIN)
        .count();
    assert_eq!(mins, 1, "{m:08x?}");
    assert_eq!(ops_count(&m, dpp::SELECT), 0);
}

/// `row_mask` and `bank_mask` leave the lanes they do not name holding the
/// destination's old value; all ones writes the whole wave and needs no mask.
#[test]
fn the_row_and_bank_masks_keep_the_destination_elsewhere() {
    let w = v_mov_dpp_at(1, 0xE4, 0x3, 0x5, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SELECT), 1, "a masked write blends: {m:08x?}");

    let w = v_mov_dpp_at(1, 0xE4, 0xF, 0xF, false);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SELECT), 0, "the whole wave is written");
}

/// DPP replaces the instruction's first source only: a second source that
/// names the same register is the lane's own value, not the permuted one.
#[test]
fn a_dpp_read_replaces_one_source_only() {
    let w = v_min_dpp(1, 0x104);
    let m = module(&[w[0], w[1], S_ENDPGM]);
    let (_, sh) = instructions(&m)
        .into_iter()
        .find(|(op, _)| *op == dpp::SHUFFLE)
        .expect("the permuted source");
    assert_eq!(
        constant_of(&m, sh[2]),
        Some(3),
        "the shuffle is scoped to the subgroup"
    );
    let (_, add) = instructions(&m)
        .into_iter()
        .find(|(op, ops)| *op == 12 && ops.len() > 3 && ops[3] == 38)
        .expect("the unsigned minimum");
    // An `OpExtInst`'s operands are the set and the instruction first.
    let (first, second) = (add[4], add[5]);
    assert_ne!(first, second, "the two sources are not one value");
    // The second source stays the lane's own value: a load, behind whatever
    // conversion the instruction does (a bitcast here).
    let unbitcast = |id: u32| match defining_op(&m, id) {
        Some(124) => {
            let (_, bc) = instructions(&m)
                .into_iter()
                .find(|(op, ops)| *op == 124 && ops[1] == id)
                .expect("a bitcast of the source");
            bc[2]
        }
        _ => id,
    };
    let (own, permuted) = (unbitcast(second), unbitcast(first));
    assert_eq!(
        defining_op(&m, own),
        Some(61),
        "the second source is the lane's own load"
    );
    assert!(
        matches!(
            defining_op(&m, permuted),
            Some(169) | Some(345) | Some(346) | Some(366)
        ),
        "the first source is the permuted value: {:?}",
        defining_op(&m, permuted)
    );
}

/// DPP8: each bank of eight lanes picks one of its own lanes, three bits per
/// lane, and no write mask applies.
#[test]
fn a_dpp8_permutation_reads_inside_eight_lane_banks() {
    // dpp8:[0,1,2,3,4,5,6,7] is the identity; every lane 7 instead is not.
    let word = [
        0x7E00_0000 | 0x01 << 17 | 0x01 << 9 | 0xE9,
        // sel = eight 3-bit lanes, src0 = v0.
        0o7_7_7_7_7_7_7_7 << 8,
    ];
    let m = module(&[word[0], word[1], S_ENDPGM]);
    assert_eq!(ops_count(&m, dpp::SHUFFLE), 1, "{m:08x?}");
    assert_eq!(ops_count(&m, dpp::SELECT), 0, "DPP8 masks no lanes");
    assert_eq!(
        constant_of(&m, instructions(&m)
            .into_iter()
            .find(|(op, _)| *op == dpp::SHUFFLE)
            .map(|(_, ops)| ops[4])
            .expect("the shuffle's id")),
        None,
        "the lane is the selector, not a constant"
    );
}

/// An undefined DPP control is refused with a reason, not emitted as
/// something else.
#[test]
fn a_dpp_control_this_emitter_does_not_model_is_refused() {
    // ctrl[7:4] = 3 is not a DPP operation the ISA defines.
    let w = v_mov_dpp_at(1, 0x134, 0xF, 0xF, false);
    let why = refusal(&[w[0], w[1], S_ENDPGM], false);
    assert!(why.contains("DPP control"), "{why}");
}

/// The pipeline's side: the subgroup size to ask for, whether the console's
/// full waves can be required, and the reason when they cannot.
#[test]
fn the_subgroup_plan_pins_the_wave_size() {
    let words = [S_ENDPGM];
    let plan = program_of(&words, true, [64, 1, 1]).subgroup_plan();
    assert_eq!(plan.required_subgroup_size, 64);
    assert!(plan.require_full_subgroups);
    assert_eq!(plan.no_full_subgroups, None);

    let plan = program_of(&words, false, [32, 1, 1]).subgroup_plan();
    assert_eq!(plan.required_subgroup_size, 32, "wave32 programs");
    assert!(plan.require_full_subgroups, "32 threads is one 32-lane subgroup");

    let plan = program_of(&words, true, [32, 1, 1]).subgroup_plan();
    assert_eq!(plan.required_subgroup_size, 64, "still pinned to the wave");
    assert!(!plan.require_full_subgroups);
    assert!(plan.no_full_subgroups.is_some(), "the reason must be reported");
}

