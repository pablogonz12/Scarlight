//! Multi-wave group convergence analysis and single-lane `v_readfirstlane_b32`
//! narrowing helpers for `lower`.

use super::*;

fn exits_succ(st: &Structured) -> Option<Vec<Vec<usize>>> {
    let n = st.exits.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, e) in st.exits.iter().enumerate() {
        match *e {
            LaneExit::Jump(t) => succ[b].push(t),
            LaneExit::Branch { taken, not_taken, .. } => {
                succ[b].push(taken);
                succ[b].push(not_taken);
            }
            LaneExit::End => {}
            LaneExit::Indirect => return None,
        }
    }
    Some(succ)
}

fn postdominators(succ: &[Vec<usize>]) -> Vec<Vec<u64>> {
    let n = succ.len();
    let words = n.div_ceil(64).max(1);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n)
        .map(|b| {
            if succ[b].is_empty() {
                let mut v = vec![0; words];
                v[b / 64] |= 1 << (b % 64);
                v
            } else {
                full.clone()
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    pdom
}

fn mark_branch_region(succ: &[Vec<usize>], pdom: &[Vec<u64>], d: usize, seen: &mut [bool]) {
    let mut stack: Vec<usize> = succ[d].clone();
    while let Some(b) = stack.pop() {
        if seen[b] || (pdom[d][b / 64] >> (b % 64)) & 1 == 1 {
            continue;
        }
        seen[b] = true;
        stack.extend(succ[b].iter().copied());
    }
}

/// Every per-lane branch (by its block) with its region: the blocks reachable from it that do
/// not post-dominate it, which only some lanes run. Not the branches the wave takes together
/// ([`Structured::wide`]), nor those on scalar conditions. None with an indirect exit.
pub(super) fn lane_regions(st: &Structured) -> Option<Vec<(usize, Vec<bool>)>> {
    let n = st.exits.len();
    let succ = exits_succ(st)?;
    let pdom = postdominators(&succ);
    let mut regions = Vec::new();
    for (d, e) in st.exits.iter().enumerate() {
        let LaneExit::Branch { when, .. } = *e else { continue };
        if st.wide.contains(&d) || st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))) {
            continue; // a wave-wide or uniform branch: all lanes go the same way
        }
        let mut seen = vec![false; n];
        mark_branch_region(&succ, &pdom, d, &mut seen);
        regions.push((d, seen));
    }
    Some(regions)
}

/// Per IR block in a multi-wave compute group: whether every wave of the threadgroup runs it
/// together (lies outside both un-widened lane branches and scalar branches whose condition
/// is not provably threadgroup-uniform). None with an indirect exit.
pub(super) fn all_group_blocks(p: &Program, st: &Structured) -> Option<Vec<bool>> {
    let n = st.exits.len();
    let succ = exits_succ(st)?;
    let pdom = postdominators(&succ);
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut in_order = vec![false; n];
    for &b in &order {
        in_order[b] = true;
    }
    let mut assumed_s: HashSet<Value> = HashSet::new();
    for block in &p.blocks {
        for phi in &block.phis {
            if matches!(phi.reg, Reg::S(_) | Reg::Scc) {
                assumed_s.insert(phi.value);
            }
        }
    }
    loop {
        let mut uniform_s: HashSet<Value> = p
            .inputs
            .iter()
            .filter_map(|&(r, v)| match r {
                Reg::S(c) if c < 106 => Some(v),
                _ => None,
            })
            .collect();
        for &b in &order {
            for phi in &p.blocks[b].phis {
                if assumed_s.contains(&phi.value) {
                    uniform_s.insert(phi.value);
                }
            }
            for inst in &p.blocks[b].insts {
                let name = inst.name();
                let produces_uniform = match &inst.fields {
                    Fields::Smem { .. } => {
                        !name.contains("atomic")
                            && inst.reads.iter().all(|&(_, v)| uniform_s.contains(&v))
                    }
                    Fields::Sop { .. } => {
                        !name.starts_with("s_getreg")
                            && inst
                                .reads
                                .iter()
                                .all(|&(r, v)| matches!(r, Reg::S(_) | Reg::Scc) && uniform_s.contains(&v))
                    }
                    _ => false,
                };
                if produces_uniform {
                    for &(r, w) in &inst.writes {
                        if matches!(r, Reg::S(_) | Reg::Scc) {
                            uniform_s.insert(w);
                        }
                    }
                }
            }
        }
        let mut diverged = vec![false; n];
        for (d, e) in st.exits.iter().enumerate() {
            if !in_order[d] || st.wide.contains(&d) {
                continue;
            }
            let LaneExit::Branch { when, .. } = *e else {
                continue;
            };
            let group_uniform = st
                .bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(v) if uniform_s.contains(v)));
            if !group_uniform {
                mark_branch_region(&succ, &pdom, d, &mut diverged);
            }
        }
        let mut changed = false;
        for &b in &order {
            let block = &p.blocks[b];
            let preds_uniform = block
                .preds
                .iter()
                .all(|&q| !in_order[q] || !diverged[q]);
            for phi in &block.phis {
                if assumed_s.contains(&phi.value)
                    && (!preds_uniform || !phi.args.iter().all(|a| uniform_s.contains(a)))
                {
                    assumed_s.remove(&phi.value);
                    changed = true;
                }
            }
        }
        if !changed {
            return Some(diverged.into_iter().map(|d| !d).collect());
        }
    }
}

impl<'a> Lower<'a> {
    /// Whether `v` is the current console wave's lane index `0..63` built from
    /// `v_mbcnt_hi_u32_b32(-1, v_mbcnt_lo_u32_b32(-1, 0))` (possibly via moves).
    fn is_mbcnt_lane(&self, mut v: Value) -> bool {
        let find_def = |val: Value| -> Option<&Inst> {
            let &b = self.def_block.get(&val)?;
            self.x.p.blocks[b]
                .insts
                .iter()
                .find(|inst| inst.writes.iter().any(|w| w.1 == val))
        };
        let is_ones = |op: &Operand| matches!(*op, Operand::Const(193) | Operand::Literal(u32::MAX));
        let is_zero = |op: &Operand| matches!(*op, Operand::Const(128) | Operand::Literal(0));
        for _ in 0..4 {
            let Some(inst) = find_def(v) else {
                return false;
            };
            match (&inst.fields, inst.name()) {
                (Fields::Valu(f), "v_mov_b32") if f.dpp.is_none() && f.sdwa.is_none() => {
                    let Some(&Operand::Vector { reg, .. }) = f.src.first() else {
                        return false;
                    };
                    let Some(next) = inst.read(Reg::V(reg)) else {
                        return false;
                    };
                    v = next;
                }
                (Fields::Valu(f), "v_mbcnt_hi_u32_b32")
                    if f.src.first().is_some_and(is_ones)
                        && matches!(f.src.get(1), Some(Operand::Vector { .. })) =>
                {
                    let Some(&Operand::Vector { reg, .. }) = f.src.get(1) else {
                        return false;
                    };
                    let Some(lo_v) = inst.read(Reg::V(reg)) else {
                        return false;
                    };
                    let Some(lo) = find_def(lo_v) else {
                        return false;
                    };
                    return lo.name() == "v_mbcnt_lo_u32_b32"
                        && matches!(&lo.fields, Fields::Valu(lf)
                            if lf.src.first().is_some_and(is_ones) && lf.src.get(1).is_some_and(is_zero));
                }
                _ => return false,
            }
        }
        false
    }

    /// Whether a `v_readfirstlane_b32` at `(b, rf_idx)` runs with `EXEC` narrowed to at most one
    /// console lane (e.g. `v_cmpx_eq_i32 exec, s, v_mbcnt(-1)`) and every consumer of its result
    /// remains inside the same block before `EXEC` is widened or restored. Under that condition only
    /// the host wave holding that single active lane performs any side effect using the result, so
    /// that host wave's own `first_lane` needs no cross-half exchange or barrier.
    pub(super) fn single_lane_readfirstlane(&self, rf_inst: &Inst) -> bool {
        let b = self.cur_ir;
        let insts = &self.x.p.blocks[b].insts;
        let Some(rf_idx) = insts.iter().position(|x| std::ptr::eq(x, rf_inst)) else {
            return false;
        };
        let Some(dst_v) = rf_inst.writes.first().map(|w| w.1) else {
            return false;
        };
        let narrows = |inst: &Inst, prev: Option<Value>| -> bool {
            let Some(pe) = prev else {
                return false;
            };
            if inst.read(Reg::S(EXEC)) != Some(pe) {
                return false;
            }
            if inst.name().starts_with("v_cmpx_") {
                return true;
            }
            matches!(
                inst.name(),
                "s_and_b64" | "s_and_saveexec_b64" | "s_and_b32" | "s_and_saveexec_b32"
            )
        };
        // Walk backwards in `b` (and across unique-predecessor unconditional jumps) to find a
        // singleton `v_cmpx_eq_{i,u}32` narrowing `EXEC` down to `rf_idx`.
        let mut expected_exec = rf_inst.read(Reg::S(EXEC));
        let mut singleton = false;
        let mut scan_b = b;
        let mut scan_end = rf_idx;
        let mut hops = 0usize;
        'back: loop {
            for inst in self.x.p.blocks[scan_b].insts[..scan_end].iter().rev() {
                let Some(w_exec) = inst.write(Reg::S(EXEC)) else {
                    continue;
                };
                if Some(w_exec) != expected_exec {
                    return false;
                }
                if matches!(inst.name(), "v_cmpx_eq_i32" | "v_cmpx_eq_u32")
                    && let Fields::Valu(vf) = &inst.fields
                    && vf.dpp.is_none()
                    && vf.sdwa.is_none()
                {
                    let single = match (vf.src.first(), vf.src.get(1)) {
                        (Some(Operand::Scalar { .. }), Some(Operand::Vector { reg, .. }))
                        | (Some(Operand::Vector { reg, .. }), Some(Operand::Scalar { .. })) => {
                            inst.read(Reg::V(*reg)).is_some_and(|v| self.is_mbcnt_lane(v))
                        }
                        _ => false,
                    };
                    if single {
                        singleton = true;
                        break 'back;
                    }
                }
                if narrows(inst, expected_exec) {
                    expected_exec = inst.read(Reg::S(EXEC));
                } else {
                    return false;
                }
            }
            let cur = &self.x.p.blocks[scan_b];
            if hops >= 8
                || cur.preds.len() != 1
                || expected_exec.is_some_and(|ex| cur.phis.iter().any(|p| p.value == ex))
            {
                break;
            }
            let pred = cur.preds[0];
            if !matches!(self.x.p.blocks[pred].exit, Exit::Jump(t) if t == scan_b) {
                break;
            }
            scan_b = pred;
            scan_end = self.x.p.blocks[pred].insts.len();
            hops += 1;
        }
        if !singleton {
            return false;
        }
        // Track scalar/SCC uses in `b` after `rf_idx` while `EXEC` remains strictly at/below `expected_exec`.
        let mut tracked: HashSet<Value> = HashSet::from([dst_v]);
        let mut cur_exec = rf_inst.read(Reg::S(EXEC));
        let mut narrow = true;
        for inst in &insts[rf_idx + 1..] {
            if let Some(w_exec) = inst.write(Reg::S(EXEC)) {
                if narrow && narrows(inst, cur_exec) {
                    cur_exec = Some(w_exec);
                } else {
                    narrow = false;
                }
            }
            if inst.reads.iter().any(|&(_, v)| tracked.contains(&v)) {
                if !narrow
                    || matches!(inst.fields, Fields::Smem { .. })
                    || inst.write(Reg::S(EXEC)).is_some()
                {
                    return false;
                }
                for &(r, w) in &inst.writes {
                    if matches!(r, Reg::S(_) | Reg::Scc) {
                        tracked.insert(w);
                    }
                }
            }
        }
        // Verify nothing outside `b`'s tracked linear suffix reads `tracked`.
        for (bi, blk) in self.x.p.blocks.iter().enumerate() {
            for phi in &blk.phis {
                if phi.args.iter().any(|a| tracked.contains(a)) {
                    return false;
                }
            }
            if bi == b {
                continue;
            }
            for inst in &blk.insts {
                if inst.reads.iter().any(|&(_, v)| tracked.contains(&v)) {
                    return false;
                }
            }
        }
        true
    }
}
