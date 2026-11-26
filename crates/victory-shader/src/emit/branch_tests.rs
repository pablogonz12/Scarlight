//! A `LaneExit::Branch` whose arms are the same block translates, and as a
//! jump: two `CondBr` edges to one block would make the same critical edge
//! twice (VIC-BT-007).

use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

use super::lower::{Parts, lower};

/// `LaneExit::Branch` with `taken == not_taken` is lowered as a jump: the
/// edge map keys a critical edge by `(from, to)`, so the second `edge()` for
/// the same pair would overwrite the first block and the PHIs of the one
/// successor would miss a predecessor.
#[test]
fn identical_branch_arms_lower_as_a_jump() {
    // Both arms lead to the end, which the branch and the block it skips
    // reach: two predecessors, so the edge is critical and `edge` is called
    // for it (the fixture is what can go wrong, not this program).
    let words = [
        0xBF06_0100u32, // s_cmp_eq_u32 s0, s1
        0xBF85_0001,    // s_cbranch_scc1 -> s_endpgm
        0x7E02_0280,    // v_mov_b32 v1, 0
        0xBF81_0000,    // s_endpgm
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, false, 256).unwrap();
    let mut st = structure::structure(&p).unwrap();
    let mut collapsed = false;
    for exit in &mut st.exits {
        if let structure::LaneExit::Branch {
            taken, not_taken, ..
        } = exit
        {
            *not_taken = *taken;
            collapsed = true;
        }
    }
    assert!(
        collapsed,
        "the fixture must expose a LaneExit::Branch to give both arms one target"
    );
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let lowered = lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    })
    .expect("a branch with two identical arms must lower");
    assert!(!lowered.dxil.is_empty());
    // It is the jump it means: the same exit taken as a `Jump` makes the same
    // blocks. The two-way form made one critical-edge block per `edge` call
    // and lost the first from the map, so it had a block more.
    st.exits[0] = structure::LaneExit::Jump(2);
    let jumped = lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    })
    .expect("the jump lowers");
    assert_eq!(
        lowered.blocks, jumped.blocks,
        "a branch with two identical arms must lower as the jump it is"
    );
}
