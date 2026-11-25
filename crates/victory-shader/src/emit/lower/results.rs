//! The emitter's result section: what a translated instruction's value
//! becomes (`set`, `write`, `write_placed`, the fused-write helpers), the
//! guarded stretches, and the lane and thread indices a program starts from.
//! A child module of [`super`] so it shares the emitter's state.

use super::*;

impl<'a> Lower<'a> {
    // ---- results ----

    pub(super) fn set(&mut self, v: Option<Value>, val: Val) {
        if let Some(v) = v {
            self.vals.insert(v, val);
        }
    }

    /// Writes VGPR `reg` (or the SGPR of a lane read), under `guard`.
    pub(super) fn write(&mut self, inst: &Inst, reg: Reg, x: V, guard: Option<V>) {
        let x = match (Self::sdwa_dst(inst), reg) {
            (Some((sel, unused)), Reg::V(_)) => self.place_bits(inst, reg, x, sel, unused),
            _ => x,
        };
        self.write_placed(inst, reg, x, guard);
    }

    /// [`Self::write`] of a dword already placed (SDWA applied).
    pub(super) fn write_placed(&mut self, inst: &Inst, reg: Reg, x: V, guard: Option<V>) {
        let Some(new) = inst.write(reg) else { return };
        let x = match (guard, reg) {
            (Some(g), Reg::V(_)) => {
                let old = match inst.read(reg).map(|o| self.i32_of(o)) {
                    Some(Ok(o)) => o,
                    // The validator refuses reads of undef.
                    _ => self.s.m.i32(0),
                };
                let v = self.s.m.select(self.cur, g, x, old);
                self.s.m.tag(v, Tag::Guard);
                v
            }
            _ => x,
        };
        self.vals.insert(new, Val::I32(x));
    }

    /// The SDWA destination selection: a byte or word, and what the other
    /// bits get (0 zeros, 1 the sign above the field, 2 the old bits).
    pub(super) fn sdwa_dst(inst: &Inst) -> Option<(u8, u8)> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        let s = v.sdwa?;
        (s.dst_sel < 6).then_some((s.dst_sel, s.dst_unused))
    }

    /// The low byte or word of `x` placed at `sel` in VGPR `reg`.
    pub(super) fn place_bits(&mut self, inst: &Inst, reg: Reg, x: V, sel: u8, unused: u8) -> V {
        let c = self.cur;
        let (width, at) = if sel < 4 {
            (8u32, 8 * u32::from(sel))
        } else {
            (16, 16 * u32::from(sel - 4))
        };
        let left = self.s.m.i32(32 - width);
        let t = self.s.m.bin(c, Bin::Shl, x, left);
        let right = self.s.m.i32(32 - width - at);
        if unused == 1 {
            // Bits below the field shift in as zeros.
            return self.s.m.bin(c, Bin::AShr, t, right);
        }
        let field = self.s.m.bin(c, Bin::LShr, t, right);
        if unused != 2 {
            return field;
        }
        let old = match inst.read(reg).map(|o| self.i32_of(o)) {
            Some(Ok(o)) => o,
            _ => self.s.m.i32(0),
        };
        let keep = self.s.m.i32(!((((1u64 << width) - 1) as u32) << at));
        let old = self.s.m.bin(c, Bin::And, old, keep);
        self.s.m.bin(c, Bin::Or, old, field)
    }

    pub(super) fn write_f(&mut self, inst: &Inst, f: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let mut f = f;
        match v.omod {
            0 => {}
            k => {
                let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(k)];
                let c = self.s.m.f32(factor.to_bits());
                f = self.s.m.bin(self.cur, Bin::Mul, f, c);
            }
        }
        if v.clamp {
            f = self.unary(7, f);
        }
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a float result outside a VGPR");
        };
        let x = self.as_int(f);
        self.write(inst, Reg::V(reg), x, guard);
        if guard.is_none()
            && Self::sdwa_dst(inst).is_none()
            && let Some(new) = inst.write(Reg::V(reg))
        {
            self.floats.insert(new, f);
        }
        Ok(())
    }

    pub(super) fn write_i(&mut self, inst: &Inst, x: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.clamp || v.omod != 0 {
            return Err("integer output modifiers");
        }
        match v.dst {
            Some(Operand::Vector { reg, .. }) => self.write(inst, Reg::V(reg), x, guard),
            Some(Operand::Scalar { code, .. }) => self.write(inst, Reg::S(code), x, None),
            _ => return Err("a result without a register"),
        }
        Ok(())
    }

    /// Starts code only the lanes where `guard` holds run (a store under a
    /// lane guard): returns where it started and the join block.
    pub(super) fn begin_guarded(&mut self, guard: Option<V>) -> Option<(usize, usize)> {
        let g = guard?;
        let (then, join) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(self.cur, Term::CondBr(g, then, join));
        let head = self.cur;
        self.cur = then;
        Some((head, join))
    }

    /// Ends guarded code; each (value inside, value outside) becomes a phi.
    pub(super) fn end_guarded(&mut self, at: Option<(usize, usize)>, values: &[(V, V)]) -> Vec<V> {
        let Some((head, join)) = at else {
            return values.iter().map(|v| v.0).collect();
        };
        let then = self.cur;
        self.s.m.terminate(then, Term::Br(join));
        self.cur = join;
        values
            .iter()
            .map(|&(inside, outside)| {
                let ty = self.s.m.type_of(inside);
                let phi = self.s.m.phi(join, ty);
                self.s.m.set_phi(phi, vec![(inside, then), (outside, head)]);
                phi
            })
            .collect()
    }

    /// Code that depends on the lane layout: wave operations, and in compute
    /// the console's wave size (lanes are thread ids in group order there).
    /// Pixel lanes match only within a quad.
    pub(super) fn cross_lane(&mut self, quad: bool) -> Result<(), Why> {
        if self.s.mesh.is_some() {
            // shortcut: no wave size for mesh shaders in SM 6.6, so host
            // lanes cannot follow the console's; emulate through group-shared
            // memory if mesh programs turn out to need these.
            return Err("a cross-lane operation in a mesh shader");
        }
        if self.s.threads.is_none() && !(quad && self.s.pixel) {
            return Err("a cross-lane operation outside compute");
        }
        debug_assert!(self.s.threads.is_none() || self.console_lanes);
        self.s.flags |= flags::WAVE_OPS;
        Ok(())
    }

    /// Component `c` of the thread id in the group. With console lanes, the
    /// thread is relabelled so that its host lane is its console lane: the
    /// flat id is the wave's first lane's, rounded down to the wave size,
    /// plus the lane index. (A bijection while each host wave holds a
    /// wave-aligned run of flat ids, as the console's waves do.)
    pub(super) fn thread_id(&mut self, c: u8) -> Val {
        if !self.console_lanes {
            return self.compute_id("threadIdInGroup.i32", 95, c);
        }
        let [x, y, _] = self.s.threads.expect("compute");
        let e = self.s.entry;
        let i32t = self.s.t.i32;
        if self.halves.is_some() {
            let f = self.console_thread();
            self.s.flags |= flags::WAVE_OPS;
            let (xk, yk, xy) = (self.s.m.i32(x), self.s.m.i32(y), self.s.m.i32(x * y));
            return Val::I32(match c {
                0 => self.s.m.bin(e, Bin::URem, f, xk),
                1 => {
                    let t = self.s.m.bin(e, Bin::UDiv, f, xk);
                    self.s.m.bin(e, Bin::URem, t, yk)
                }
                _ => self.s.m.bin(e, Bin::UDiv, f, xy),
            });
        }
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let first = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[flat],
        );
        let wave = self.s.wave_size.expect("set with console lanes");
        let mask = self.s.m.i32(!(wave - 1));
        let base = self.s.m.bin(e, Bin::And, first, mask);
        let lane = self.lane_index();
        let f = self.s.m.bin(e, Bin::Add, base, lane);
        self.s.flags |= flags::WAVE_OPS;
        let (xk, yk, xy) = (self.s.m.i32(x), self.s.m.i32(y), self.s.m.i32(x * y));
        Val::I32(match c {
            0 => self.s.m.bin(e, Bin::URem, f, xk),
            1 => {
                let t = self.s.m.bin(e, Bin::UDiv, f, xk);
                self.s.m.bin(e, Bin::URem, t, yk)
            }
            _ => self.s.m.bin(e, Bin::UDiv, f, xy),
        })
    }

    /// Mesh: the counts and the primitives no lane exports. The console's
    /// counts come from one lane's allocation request, which DXIL cannot
    /// express (one call, dominating every output write).
    // shortcut: the counts are the declared maxima and each lane first culls
    // its primitive (all indices 0, zero area), so primitives past the
    // program's count are culled and vertices past it unused; this costs
    // the host the unused vertex and primitive slots. Pass the program's own
    // counts if that shows in frame time.
    pub(super) fn mesh_prologue(&mut self, mut mesh: super::super::shader::Mesh) -> Result<(), Why> {
        let threads = self.s.threads.expect("mesh")[0];
        if mesh.topology != 2 {
            return Err("a mesh shader not drawing triangles");
        }
        if threads > 128 || mesh.max_vertices > 256 || mesh.max_primitives > 256 {
            return Err("a mesh shader beyond the host's limits");
        }
        // D3D12 caps the outputs of the declared maxima at 32 KiB. Lane n
        // writes only vertex n and primitive n, so maxima past the lanes are
        // never reached: they come down to the lane count when the outputs
        // do not fit.
        use super::super::container::{MESH_OUTPUT_LIMIT, mesh_output_bytes};
        let (outputs, prims) = (&self.s.outputs, &self.s.prims);
        let fits = |m: super::super::shader::Mesh| {
            mesh_output_bytes(outputs, prims, m.max_vertices, m.max_primitives) <= MESH_OUTPUT_LIMIT
        };
        if !fits(mesh) {
            mesh.max_vertices = mesh.max_vertices.min(threads);
            mesh.max_primitives = mesh.max_primitives.min(threads);
            // shortcut: outputs past the cap even at the lane count are
            // still emitted with the lower maxima (drivers take them);
            // refuse them here if one hangs.
            // Experiment (`VICTORY_MESH_OUTPUT_STRICT=1`): they are refused.
            if !fits(mesh) && std::env::var_os("VICTORY_MESH_OUTPUT_STRICT").is_some() {
                return Err("a mesh shader whose outputs pass the host's 32 KiB");
            }
            self.s.mesh = Some(mesh);
        }
        let e = self.s.entry;
        let void = self.s.t.void;
        let (v, p) = (
            self.s.m.i32(mesh.max_vertices),
            self.s.m.i32(mesh.max_primitives),
        );
        self.s
            .op(e, "setMeshOutputCounts", Attr::NoUnwind, void, 168, &[v, p]);
        let flat = self.mesh_thread();
        let z = self.s.m.i32(0);
        // Every primitive of the declared count starts culled, also those
        // past the lanes (a count of 256 with 128 lanes): lane n culls
        // primitives n, n + lanes, ... The host draws all it was promised,
        // and one never written has whatever indices its memory held: long
        // triangles over the whole target, each pixel of them shaded.
        let passes = mesh.max_primitives.div_ceil(threads.max(1));
        for pass in 0..passes {
            let index = if pass == 0 {
                flat
            } else {
                let past = self.s.m.i32(pass * threads);
                self.s.m.bin(self.cur, Bin::Add, flat, past)
            };
            let guard = ((pass + 1) * threads > mesh.max_primitives)
                .then(|| self.s.m.cmp(self.cur, pred::IULT, index, p));
            let at = self.begin_guarded(guard);
            self.s.op(
                self.cur,
                "emitIndices",
                Attr::NoUnwind,
                void,
                169,
                &[index, z, z, z],
            );
            self.end_guarded(at, &[]);
        }
        Ok(())
    }

    /// Mesh: the thread's index in the group, which is its vertex and
    /// primitive index.
    pub(super) fn mesh_thread(&mut self) -> V {
        if let Some(f) = self.flat {
            return f;
        }
        let e = self.s.entry;
        let f = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            self.s.t.i32,
            96,
            &[],
        );
        self.flat = Some(f);
        f
    }

    /// The console wave size.
    pub(super) fn wave(&self) -> u32 {
        if self.x.native.wave64 { 64 } else { 32 }
    }

    pub(super) fn lane_index(&mut self) -> V {
        if self.s.mesh.is_some() {
            // The console's lane: mesh code reads no other lanes, so host
            // waves need not match the console's.
            let f = self.mesh_thread();
            let m = self.s.m.i32(self.wave() - 1);
            return self.s.m.bin(self.s.entry, Bin::And, f, m);
        }
        self.s.op(
            self.cur,
            "waveGetLaneIndex",
            Attr::ReadOnly,
            self.s.t.i32,
            111,
            &[],
        )
    }

    /// `x` of lane `src` (in this lane's quad) through quad reads, whose
    /// lane must be a constant.
    pub(super) fn quad_pick(&mut self, x: V, src: V) -> V {
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let three = self.s.m.i32(3);
        let q = self.s.m.bin(c, Bin::And, src, three);
        let mut r = x;
        for n in 0..4 {
            let kn = self.s.m.i32(n);
            let read = self.s.op(
                c,
                "quadReadLaneAt.i32",
                Attr::NoUnwind,
                self.s.t.i32,
                122,
                &[x, kn],
            );
            let is = self.s.m.cmp(c, pred::EQ, q, kn);
            r = self.s.m.select(c, is, read, r);
        }
        r
    }

    /// `x` in the first lane of this host wave where `guard` holds, and whether it has such a
    /// lane: the console reads the first lane of EXEC, and a lane that got here with EXEC clear
    /// (a guard, not a branch) is not one. With none, `x` of the wave's first lane here.
    pub(super) fn first_lane(&mut self, x: V, guard: Option<V>) -> (V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        self.s.flags |= flags::WAVE_OPS;
        let first = self.s.op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[x]);
        let Some(g) = guard else {
            return (first, self.s.m.bool(true));
        };
        let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
        let r = self.s.op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[g]);
        let (lo, hi) = (self.s.m.extract(c, r, 0), self.s.m.extract(c, r, 1));
        let (z, k32) = (self.s.m.i32(0), self.s.m.i32(32));
        // FirstbitLo (dx.op 32): the lowest set bit
        let flo = self.s.op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[lo]);
        let fhi = self.s.op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[hi]);
        let fhi = self.s.m.bin(c, Bin::Add, fhi, k32);
        let in_lo = self.s.m.cmp(c, pred::NE, lo, z);
        let in_hi = self.s.m.cmp(c, pred::NE, hi, z);
        let any = self.s.m.bin(c, Bin::Or, in_lo, in_hi);
        let lane = self.s.m.select(c, in_lo, flo, fhi);
        // no lane in EXEC: read this lane (one that is here); the read is not used
        let me = self.s.op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let lane = self.s.m.select(c, any, lane, me);
        let at = self.read_lane(x, lane);
        (self.s.m.select(c, any, at, first), any)
    }

    pub(super) fn read_lane(&mut self, x: V, lane: V) -> V {
        self.s.op(
            self.cur,
            "waveReadLaneAt.i32",
            Attr::NoUnwind,
            self.s.t.i32,
            117,
            &[x, lane],
        )
    }

    /// The float value of a register value.
    pub(super) fn float_of(&mut self, v: Value) -> Result<V, Why> {
        if let Some(&f) = self.floats.get(&v) {
            return Ok(f);
        }
        let x = self.i32_of(v)?;
        Ok(self.as_float(x))
    }

    /// The values of VGPRs `reg..reg + n` as `inst` reads them.
    pub(super) fn vgprs(&mut self, inst: &Inst, op: &Operand, n: u8) -> Result<Vec<V>, Why> {
        let Operand::Vector { reg, .. } = *op else {
            return Err("a vector operand");
        };
        (0..n)
            .map(|k| {
                let v = inst.read(Reg::V(reg + k)).ok_or("an unread operand")?;
                self.i32_of(v)
            })
            .collect()
    }

    /// LDS (thread-group shared memory) and `ds_swizzle`.
    pub(super) fn ds(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            offset1,
            gds,
        } = &inst.fields
        else {
            unreachable!()
        };
        if *gds {
            return Err("GDS");
        }
        let name = inst.name();
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let offset = u32::from(*offset0) | u32::from(*offset1) << 8;
        let dst_reg = match dst {
            Some(Operand::Vector { reg, .. }) => Some(*reg),
            _ => None,
        };
        if name == "ds_swizzle_b32" {
            // Bit 15 set: each lane of a quad picks a lane of it; else lanes
            // of each group of 32 combine their index with and/or/xor masks.
            let quad = offset & 0x8000 != 0
                || offset & 0x1C == 0x1C && offset & 0x380 == 0 && offset >> 10 & 0x1F < 4;
            // Mesh: host quads are threads 4n..4n+3 of a 1D group, as the
            // console's, so quad-local swizzles read through quad operations.
            let mesh_quad = self.s.mesh.is_some() && quad;
            if !mesh_quad {
                self.cross_lane(quad)?;
            }
            // The source sits in the address field.
            let x = self.vgprs(inst, addr.as_ref().ok_or("a swizzle source")?, 1)?[0];
            let lane = self.lane_index();
            let k = |l: &mut Self, v: u32| l.s.m.i32(v);
            let src = if offset & 0x8000 != 0 {
                let (three, one) = (k(self, 3), k(self, 1));
                let sel = self.s.m.bin(c, Bin::And, lane, three);
                let sh = self.s.m.bin(c, Bin::Shl, sel, one);
                let pattern = k(self, offset & 0xFF);
                let pick = self.s.m.bin(c, Bin::LShr, pattern, sh);
                let pick = self.s.m.bin(c, Bin::And, pick, three);
                let not3 = k(self, !3);
                let base = self.s.m.bin(c, Bin::And, lane, not3);
                self.s.m.bin(c, Bin::Or, base, pick)
            } else {
                let and = k(self, offset & 0x1F | !0x1F);
                let or = k(self, offset >> 5 & 0x1F);
                let xor = k(self, offset >> 10 & 0x1F);
                let t = self.s.m.bin(c, Bin::And, lane, and);
                let t = self.s.m.bin(c, Bin::Or, t, or);
                self.s.m.bin(c, Bin::Xor, t, xor)
            };
            let r = if mesh_quad {
                self.quad_pick(x, src)
            } else {
                self.read_lane(x, src)
            };
            let reg = dst_reg.ok_or("a swizzle result")?;
            self.write(inst, Reg::V(reg), r, guard);
            return Ok(());
        }
        if name == "ds_bpermute_b32" {
            // Backward permute: no LDS is touched. Each lane takes the data
            // register of the lane its address names, (address + offset) / 4
            // within the wave; a source lane outside EXEC gives 0. (
            // the title's checkerboard shadow-mask generators read their
            // neighbours' samples with it; without them every shadowed light
            // was multiplied by an empty mask.)
            // shortcut: a source lane that is not at this instruction on the
            // host (it left through a branch, not a guard) is read as the
            // host gives it; the console reads 0.
            self.cross_lane(false)?;
            if self.exchange.as_ref().is_some_and(|e| !e.1) {
                // a console wave of 64 run as two host waves: the lane named
                // may be in the other one
                return Err("a lane permute across two host waves");
            }
            let at = self.vgprs(inst, addr.as_ref().ok_or("a permute address")?, 1)?[0];
            let x = self.vgprs(inst, data.first().ok_or("a permute source")?, 1)?[0];
            let o = self.s.m.i32(offset);
            let at = self.s.m.bin(c, Bin::Add, at, o);
            let two = self.s.m.i32(2);
            let src = self.s.m.bin(c, Bin::LShr, at, two);
            let last = self.s.m.i32(self.wave() - 1);
            let src = self.s.m.bin(c, Bin::And, src, last);
            let x = match guard {
                Some(g) => {
                    let z = self.s.m.i32(0);
                    self.s.m.select(c, g, x, z)
                }
                None => x,
            };
            let r = self.read_lane(x, src);
            let reg = dst_reg.ok_or("a permute result")?;
            self.write(inst, Reg::V(reg), r, guard);
            return Ok(());
        }
        if self.s.lds_dwords == 0 {
            return Err("LDS the program does not allocate");
        }
        let a = self.vgprs(inst, addr.as_ref().ok_or("an LDS address")?, 1)?[0];
        // shortcut: LDS addresses are taken as dword-aligned and in bounds
        // (out-of-bounds console accesses read 0 and drop writes).
        let dword = |l: &mut Self, bytes: u32| -> V {
            let c = l.cur;
            let o = l.s.m.i32(bytes);
            let at = l.s.m.bin(c, Bin::Add, a, o);
            let two = l.s.m.i32(2);
            let index = l.s.m.bin(c, Bin::LShr, at, two);
            let lds = l.s.lds();
            l.s.m.element(c, lds, index)
        };
        let width = |n: &str| match n.rsplit('_').next() {
            Some("b64") => 2,
            Some("b96") => 3,
            Some("b128") => 4,
            _ => 1,
        };
        // (byte offset, dwords) per element; read2/write2 scale their two
        // offsets by the element size (×64 for st64).
        let pair = |n: &str| -> Vec<(u32, u32)> {
            let w = width(n);
            let scale = 4 * w * if n.contains("st64") { 64 } else { 1 };
            vec![
                (u32::from(*offset0) * scale, w),
                (u32::from(*offset1) * scale, w),
            ]
        };
        // Bytes and halves: the dword holding them, and their bit position
        // in it (naturally aligned, so they never straddle dwords).
        // `_d16` reads fill a register's low half (`_d16_hi` its high half)
        // and keep the other; `_d16_hi` writes take the data's high half.
        let base = name.trim_end_matches("_hi").trim_end_matches("_d16");
        let sub = match base {
            "ds_read_u8" | "ds_read_i8" | "ds_write_b8" => Some(8u32),
            "ds_read_u16" | "ds_read_i16" | "ds_write_b16" => Some(16),
            _ => None,
        };
        let sub = sub.filter(|_| {
            base == name
                || name.ends_with("_d16") && base.starts_with("ds_read")
                || name.ends_with("_d16_hi")
        });
        if let Some(bits) = sub {
            let p = dword(self, offset);
            let o = self.s.m.i32(offset);
            let at = self.s.m.bin(c, Bin::Add, a, o);
            let three = self.s.m.i32(3);
            let byte = self.s.m.bin(c, Bin::And, at, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            let mask = self.s.m.i32((1u32 << bits) - 1);
            if name.starts_with("ds_read") {
                let reg = dst_reg.ok_or("an LDS read result")?;
                let w = self.s.m.load(c, p);
                let x = self.s.m.bin(c, Bin::LShr, w, sh);
                let x = if base.contains("_i") {
                    let k = self.s.m.i32(32 - bits);
                    let x = self.s.m.bin(c, Bin::Shl, x, k);
                    self.s.m.bin(c, Bin::AShr, x, k)
                } else {
                    self.s.m.bin(c, Bin::And, x, mask)
                };
                let x = if base == name {
                    x
                } else {
                    let old = match inst.read(Reg::V(reg)) {
                        Some(v) => self.i32_of(v)?,
                        None => self.s.m.i32(0),
                    };
                    let half = self.s.m.i32(0xFFFF);
                    let x = self.s.m.bin(c, Bin::And, x, half);
                    let (x, keep) = if name.ends_with("_hi") {
                        let k16 = self.s.m.i32(16);
                        (self.s.m.bin(c, Bin::Shl, x, k16), half)
                    } else {
                        (x, self.s.m.i32(0xFFFF_0000))
                    };
                    let old = self.s.m.bin(c, Bin::And, old, keep);
                    self.s.m.bin(c, Bin::Or, old, x)
                };
                self.write(inst, Reg::V(reg), x, guard);
            } else {
                // Clear, then set: two atomics, so lanes writing other
                // bytes of the dword keep theirs.
                let mut x = self.vgprs(inst, data.first().ok_or("an LDS write")?, 1)?[0];
                if name != base {
                    // The data's high half.
                    let k16 = self.s.m.i32(16);
                    x = self.s.m.bin(c, Bin::LShr, x, k16);
                }
                let x = self.s.m.bin(c, Bin::And, x, mask);
                let x = self.s.m.bin(c, Bin::Shl, x, sh);
                let m = self.s.m.bin(c, Bin::Shl, mask, sh);
                let ones = self.s.m.i32(u32::MAX);
                let keep = self.s.m.bin(c, Bin::Xor, m, ones);
                use super::super::bitcode::Rmw;
                let at = self.begin_guarded(guard);
                self.s.m.atomic(self.cur, Rmw::And, p, keep);
                self.s.m.atomic(self.cur, Rmw::Or, p, x);
                self.end_guarded(at, &[]);
            }
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("ds_read") {
            let elements = if rest.starts_with('2') {
                pair(name)
            } else if matches!(rest, "_b32" | "_b64" | "_b96" | "_b128") {
                vec![(offset, width(name))]
            } else {
                return Err("an LDS read of bytes or halves");
            };
            let reg = dst_reg.ok_or("an LDS read result")?;
            let mut n = 0;
            for (off, w) in elements {
                for k in 0..w {
                    let p = dword(self, off + 4 * k);
                    let x = self.s.m.load(self.cur, p);
                    self.write(inst, Reg::V(reg + n), x, guard);
                    n += 1;
                }
            }
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("ds_write") {
            let elements = if rest.starts_with('2') {
                pair(name)
            } else if matches!(rest, "_b32" | "_b64" | "_b96" | "_b128") {
                vec![(offset, width(name))]
            } else {
                return Err("an LDS write of bytes or halves");
            };
            let mut stores = Vec::new();
            for (e, (off, w)) in elements.into_iter().enumerate() {
                let vals = self.vgprs(inst, data.get(e).ok_or("an LDS write")?, w as u8)?;
                for (k, x) in vals.into_iter().enumerate() {
                    stores.push((off + 4 * k as u32, x));
                }
            }
            let at = self.begin_guarded(guard);
            for (off, x) in stores {
                let p = dword(self, off);
                self.s.m.store(self.cur, p, x);
            }
            self.end_guarded(at, &[]);
            return Ok(());
        }
        // Atomics: the old value returns with `_rtn`.
        let op = name
            .strip_prefix("ds_")
            .map(|n| n.replace("_rtn", ""))
            .ok_or("an LDS operation")?;
        use super::super::bitcode::Rmw;
        let rmw = match op.as_str() {
            "add_u32" => Some(Rmw::Add),
            "sub_u32" => Some(Rmw::Sub),
            "and_b32" => Some(Rmw::And),
            "or_b32" => Some(Rmw::Or),
            "xor_b32" => Some(Rmw::Xor),
            "min_i32" => Some(Rmw::Min),
            "max_i32" => Some(Rmw::Max),
            "min_u32" => Some(Rmw::UMin),
            "max_u32" => Some(Rmw::UMax),
            "wrxchg_b32" => Some(Rmw::Xchg),
            "cmpst_b32" | "max_f32" | "min_f32" => None,
            _ => return Err("an LDS operation"),
        };
        let v0 = self.vgprs(inst, data.first().ok_or("an LDS atomic")?, 1)?[0];
        let v1 = match data.get(1) {
            Some(d) => Some(self.vgprs(inst, d, 1)?[0]),
            None => None,
        };
        let old_reg = dst_reg.and_then(|r| inst.read(Reg::V(r)).map(|v| (r, v)));
        let outside = match old_reg {
            Some((_, v)) => self.i32_of(v).unwrap_or(self.s.m.i32(0)),
            None => self.s.m.i32(0),
        };
        let p = dword(self, offset);
        if let Some(max) = match op.as_str() {
            "max_f32" => Some(true),
            "min_f32" => Some(false),
            _ => None,
        } {
            let at = self.begin_guarded(guard);
            let r = self.lds_float_min_max(p, v0, max);
            let r = self.end_guarded(at, &[(r, outside)])[0];
            if let Some(reg) = dst_reg {
                self.write(inst, Reg::V(reg), r, None);
            }
            return Ok(());
        }
        let at = self.begin_guarded(guard);
        let r = match rmw {
            Some(op) => self.s.m.atomic(self.cur, op, p, v0),
            // Compare with data0, store data1.
            None => self
                .s
                .m
                .cmpxchg(self.cur, p, v0, v1.ok_or("an LDS compare")?),
        };
        let r = self.end_guarded(at, &[(r, outside)])[0];
        if let Some(reg) = dst_reg {
            self.write(inst, Reg::V(reg), r, None);
        }
        Ok(())
    }

    /// `ds_max_f32` (`max`) or `ds_min_f32` on the LDS dword at `p` with
    /// the float bits `data`; the old bits return.
    ///
    /// DXIL has no float atomics on group-shared memory, so this is a
    /// compare-exchange loop around [`Lower::lds_float_pick`], which gives
    /// what RDNA2 writes (measured on gfx1036, reported on the pull
    /// request): an sNaN, data first, then memory, is written quieted;
    /// otherwise a NaN loses to a number (two quiet NaNs give the data);
    /// numbers compare as floats with -0 below +0.
    // shortcut: f32 denormals compare as kept; with the console's MODE
    // flushing them they would order as ±0 first (MODE is not modelled).
    pub(super) fn lds_float_min_max(&mut self, p: V, data: V, max: bool) -> V {
        let pre = self.cur;
        let init = self.s.m.load(pre, p);
        let (lp, exit) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(pre, Term::Br(lp));
        let i32t = self.s.t.i32;
        let old = self.s.m.phi(lp, i32t);
        self.cur = lp;
        let new = self.lds_float_pick(old, data, max);
        let prev = self.s.m.cmpxchg(lp, p, old, new);
        let done = self.s.m.cmp(lp, pred::EQ, prev, old);
        self.s.m.set_phi(old, vec![(init, pre), (prev, lp)]);
        self.s.m.terminate(lp, Term::CondBr(done, exit, lp));
        self.cur = exit;
        old
    }

    /// What `ds_max_f32` (`max`) or `ds_min_f32` writes over the dword
    /// `mem` with `data` (float bits; see [`Lower::lds_float_min_max`]).
    pub(super) fn lds_float_pick(&mut self, mem: V, data: V, max: bool) -> V {
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (abs_mask, inf, quiet, zero) = (
            k(self, 0x7FFF_FFFF),
            k(self, 0x7F80_0000),
            k(self, 0x40_0000),
            k(self, 0),
        );
        let nan = |l: &mut Self, x: V| {
            let m = l.s.m.bin(c, Bin::And, x, abs_mask);
            l.s.m.cmp(c, pred::IUGT, m, inf)
        };
        let snan = |l: &mut Self, x: V| {
            let n = nan(l, x);
            let q = l.s.m.bin(c, Bin::And, x, quiet);
            let sig = l.s.m.cmp(c, pred::EQ, q, zero);
            l.s.m.bin(c, Bin::And, n, sig)
        };
        // A total order of the numbers as signed integers, -0 below +0:
        // negative floats' magnitude bits reversed.
        let key = |l: &mut Self, x: V| {
            let k31 = l.s.m.i32(31);
            let sign = l.s.m.bin(c, Bin::AShr, x, k31);
            let flip = l.s.m.bin(c, Bin::And, sign, abs_mask);
            l.s.m.bin(c, Bin::Xor, x, flip)
        };
        let (dk, mk) = (key(self, data), key(self, mem));
        let take = self
            .s
            .m
            .cmp(c, if max { pred::SGT } else { pred::SLT }, dk, mk);
        let r = self.s.m.select(c, take, data, mem);
        let (dn, mn) = (nan(self, data), nan(self, mem));
        // One NaN: the number; two quiet NaNs: the data.
        let r = self.s.m.select(c, mn, data, r);
        let both = self.s.m.bin(c, Bin::And, dn, mn);
        let only_data = {
            let t = self.s.m.bool(true);
            let not_both = self.s.m.bin(c, Bin::Xor, both, t);
            self.s.m.bin(c, Bin::And, dn, not_both)
        };
        let r = self.s.m.select(c, only_data, mem, r);
        // Signalling NaNs win, quieted: the data's first.
        let qm = self.s.m.bin(c, Bin::Or, mem, quiet);
        let sm = snan(self, mem);
        let r = self.s.m.select(c, sm, qm, r);
        let qd = self.s.m.bin(c, Bin::Or, data, quiet);
        let sd = snan(self, data);
        self.s.m.select(c, sd, qd, r)
    }

}
