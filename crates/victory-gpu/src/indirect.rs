//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So active records are copied and clamped
//! on the GPU into a device-local scratch buffer, just before execution, by a
//! compute shader of ours (HLSL compiled at run time by the system's compiler
//! or, under Wine and Proton, by DXC: [`crate::dxc`]; a 5.1 shader with its
//! root signature inside), leaving the title's argument and count buffers
//! untouched.
//!
//! When a count buffer is given, threads at or past `min(max, count)` return
//! immediately without charging the list's budget or reading uninitialized
//! tail slots. Each list has a budget (2^27 vertex instances or threads,
//! groups counted as 64 threads, mesh groups as 128): active records past it
//! run empty, saturating without 32-bit wraparound so a list of thousands of
//! garbage records cannot run for seconds either.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, PCSTR, s};

use crate::host::{HostList, host, transition};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer Record : register(b0) {
    uint kind;
    uint stride;
    uint records;
    uint args_offset;
    // Bit 0: a count buffer bounds the records. Bit 1: the records are a
    // window clamped ahead of their draws: one that asks for more than can
    // run is left empty and costs nothing.
    uint has_count;
    uint count_offset;
};
ByteAddressBuffer src_args : register(t0);
ByteAddressBuffer count_buf : register(t1);
RWByteAddressBuffer dst_args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint eff_count = records;
    bool ahead = (has_count & 2u) != 0;
    if ((has_count & 1u) != 0) {
        eff_count = min(eff_count, count_buf.Load(count_offset));
    }
    if (id.x >= eff_count) return;
    uint src_at = args_offset + id.x * stride;
    uint dst_at = id.x * stride;
    // A tight buffer's last record ends at its argument payload (the title
    // allocates `(records - 1) * stride + payload`): reading a whole stride
    // there would read past its buffer, writing one past the scratch. Every
    // other record is a full stride, as the record before it reads on.
    uint payload = (kind <= 1) ? ((kind == 0) ? 16u : 20u) : 12u;
    uint copy_end = ((id.x + 1u) == records) ? payload : stride;
    uint cost;
    uint2 draw_counts = uint2(0, 0);
    uint3 groups = uint3(0, 0, 0);
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint2 asked = src_args.Load2(src_at);
        if (ahead && (asked.x > (1u << 24) || asked.y > (1u << 24))) {
            asked = uint2(0, 0);
        }
        uint count = min(asked.x, 1u << 24);
        uint instances = min(asked.y, (1u << 26) / max(count, 1u));
        cost = count * instances;
        draw_counts = uint2(count, instances);
    } else if (kind == 2) {
        uint3 g = min(src_args.Load3(src_at), uint3(65535u, 65535u, 65535u));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64u;
        groups = g;
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = src_args.Load3(src_at);
        // Ahead of its draw, a record over the most a dispatch runs is a
        // slot the title has not filled (the title leaves all
        // ones there): empty, and not counted below either.
        if (ahead && g.x > 65535u) {
            g = uint3(0, 0, 0);
        }
        // Diagnostics, in the budget buffer's next words: the largest
        // group count a mesh record asked for, and the groups asked in all.
        uint was;
        budget.InterlockedMax(4, g.x, was);
        budget.InterlockedAdd(8, min(g.x, 1u << 24), was);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128u;
        groups = g;
    }
    bool over = false;
    if (cost > 0) {
        uint before;
        budget.InterlockedOr(0, 0, before);
        [allow_uav_condition]
        while (before < BUDGET) {
            uint next = (BUDGET - before <= cost) ? BUDGET : (before + cost);
            uint prev;
            budget.InterlockedCompareExchange(0, before, next, prev);
            if (prev == before) break;
            before = prev;
        }
        over = before >= BUDGET || BUDGET - before < cost;
    }
    if (kind <= 1) {
        if (over) {
            draw_counts = uint2(0, 0);
        }
        dst_args.Store2(dst_at, draw_counts);
        for (uint off = 8; off < copy_end; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    } else {
        if (over) {
            groups = uint3(0, 0, 0);
        }
        dst_args.Store3(dst_at, groups);
        for (uint off = 12; off < copy_end; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    }
}
"#;

const BATCH_SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer Batch : register(b0) { uint first_job; uint job_count; uint unused0; uint unused1; };
// Packed 32-byte job descriptors:
//   j0 = uint4(kind, stride, records, src_offset)
//   j1 = uint4(dst_offset, 0, 0, 0)
ByteAddressBuffer jobs : register(t0);
ByteAddressBuffer src_args : register(t1);
RWByteAddressBuffer dst_args : register(u0);
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
static const uint GROUPS = 1u << 20;
static const uint PAST = 0xffffffffu;
groupshared uint costs[64];
groupshared uint single_run;

uint compute_clamped(uint kind, uint src_at, out uint2 draw_counts, out uint3 groups) {
    draw_counts = uint2(0, 0);
    groups = uint3(0, 0, 0);
    [branch]
    if (kind <= 1) {
        uint count = min(src_args.Load(src_at), 1u << 24);
        uint instances = min(src_args.Load(src_at + 4), (1u << 26) / max(count, 1u));
        draw_counts = uint2(count, instances);
        return count * instances;
    } else if (kind == 2) {
        uint3 g = min(src_args.Load3(src_at), uint3(65535u, 65535u, 65535u));
        if (g.z != 0) {
            if (g.x * g.y > GROUPS / max(g.z, 1u)) {
                g = uint3(0, 0, 0);
            }
        }
        groups = g;
        return g.x * g.y * g.z * 64u;
    } else {
        uint3 g = src_args.Load3(src_at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        groups = g;
        return g.x * g.y * g.z * 128u;
    }
}

void write_record(uint kind, uint stride, uint src_at, uint dst_at, bool over, uint2 draw_counts, uint3 groups) {
    if (kind <= 1) {
        if (over) {
            draw_counts.x = 0;
        }
        dst_args.Store2(dst_at, draw_counts);
        for (uint off = 8; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    } else {
        if (over) {
            groups.x = 0;
        }
        dst_args.Store3(dst_at, groups);
        for (uint off = 12; off < stride; off += 4) {
            dst_args.Store(dst_at + off, src_args.Load(src_at + off));
        }
    }
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint lane : SV_GroupIndex) {
    uint left = 0;
    if (lane == 0) {
        left = BUDGET - min(budget.Load(0), BUDGET);
    }
    uint j = first_job;
    uint end = first_job + job_count;
    while (j < end) {
        // Fast path: clamp up to 64 consecutive single-record jobs in one wave.
        // Guard root-SRV loads with `[branch]` so inactive tail lanes never
        // load past the end of `jobs` (root descriptors have no HW bounds check).
        uint4 j0 = uint4(0, 0, 0, 0);
        uint4 j1 = uint4(0, 0, 0, 0);
        [branch]
        if (j + lane < end) {
            j0 = jobs.Load4((j + lane) * 32);
            j1 = jobs.Load4((j + lane) * 32 + 16);
        }
        costs[lane] = j0.z;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0) {
            uint run = 0;
            // Avoid `&&` here: FXC evaluates both sides, which would read
            // `costs[64]` past the 64-element groupshared array.
            [loop]
            while (run < 64) {
                if (costs[run] != 1) {
                    break;
                }
                run++;
            }
            single_run = run;
        }
        GroupMemoryBarrierWithGroupSync();
        uint run = single_run;
        if (run > 0) {
            uint cost = 0;
            uint2 draw_counts = uint2(0, 0);
            uint3 groups = uint3(0, 0, 0);
            [branch]
            if (lane < run) {
                cost = compute_clamped(j0.x, j0.w, draw_counts, groups);
            }
            costs[lane] = cost;
            GroupMemoryBarrierWithGroupSync();
            if (lane == 0) {
                for (uint i = 0; i < run; i++) {
                    if (costs[i] > left) {
                        costs[i] = PAST;
                        left = 0;
                    } else {
                        left -= costs[i];
                    }
                }
            }
            GroupMemoryBarrierWithGroupSync();
            [branch]
            if (lane < run) {
                write_record(j0.x, j0.y, j0.w, j1.x, costs[lane] == PAST, draw_counts, groups);
            }
            j += run;
        } else {
            uint4 m0 = jobs.Load4(j * 32);
            uint4 m1 = jobs.Load4(j * 32 + 16);
            uint tiles = m0.z / 64 + min(m0.z % 64, 1u);
            for (uint tile = 0; tile < tiles; tile++) {
                uint record = tile * 64 + lane;
                uint src_at = m0.w + record * m0.y;
                uint dst_at = m1.x + record * m0.y;
                uint cost = 0;
                uint2 draw_counts = uint2(0, 0);
                uint3 groups = uint3(0, 0, 0);
                [branch]
                if (record < m0.z) {
                    cost = compute_clamped(m0.x, src_at, draw_counts, groups);
                }
                costs[lane] = cost;
                GroupMemoryBarrierWithGroupSync();
                if (lane == 0) {
                    for (uint i = 0; i < 64; i++) {
                        if (costs[i] > left) {
                            costs[i] = PAST;
                            left = 0;
                        } else {
                            left -= costs[i];
                        }
                    }
                }
                GroupMemoryBarrierWithGroupSync();
                [branch]
                if (record < m0.z) {
                    write_record(m0.x, m0.y, src_at, dst_at, costs[lane] == PAST, draw_counts, groups);
                }
            }
            j += 1;
        }
    }
    if (lane == 0) {
        budget.Store(0, BUDGET - left);
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match compile(SOURCE, s!("clamp")) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn batch_clamp() -> Option<&'static Clamp> {
    static BATCH: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    BATCH
        .get_or_init(|| match compile(BATCH_SOURCE, s!("batch_clamp")) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no batched argument clamp: falling back to inline");
                None
            }
        })
        .as_ref()
}

fn compile(source: &str, _name: PCSTR) -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let bytes = crate::dxc::compile(source, "main", "cs_5_1")?;
    make_clamp(device, &bytes)
}

/// Whether indirect argument clamps are hoisted into the list's submission
/// prelude (`VICTORY_HOIST_INDIRECT=0` records the inline clamp before each
/// draw, as before).
pub(crate) fn hoisting() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HOIST_INDIRECT").is_none_or(|v| v != "0"))
}

/// Compute passes recorded to clamp indirect arguments: one per draw of the
/// inline clamp, one per run of jobs of the batched prelude. The dispatch
/// count the hoisted clamp saves, for its test.
static CLAMP_PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`CLAMP_PASSES`] now.
#[cfg(test)]
pub(crate) fn clamp_passes() -> u64 {
    CLAMP_PASSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The clamp's root signature and pipeline over compiled `bytes`.
fn make_clamp(device: &ID3D12Device, bytes: &[u8]) -> windows::core::Result<Clamp> {
    // SAFETY: the root signature is inside the shader; the description
    // points at live bytes and never releases its pointer.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Minimum record byte stride for a supported PC indirect argument `kind`.
fn min_stride(kind: u32) -> Option<u32> {
    match kind {
        0 => Some(16),
        1 => Some(20),
        2 | 10 => Some(12),
        _ => None,
    }
}

const HOIST_CHUNK: u64 = 256 << 10;

struct Job {
    src_gpu: u64,
    dst_gpu: u64,
}

struct JobUpload {
    _buffer: ID3D12Resource,
    mapped: usize,
    gpu: u64,
    capacity: usize,
}

// SAFETY: `mapped` is a persistent upload mapping written only under the
// list's `HoistedClamps` mutex while the list is open for recording.
unsafe impl Send for JobUpload {}

/// Indirect argument clamps hoisted from a [`HostList`]'s initial non-writing
/// prefix into its submission prelude. Each hoisted draw reads the title's
/// argument buffer through an SRV (never transitioning or mutating it) and
/// writes clamped records into the list's own scratch chunks, which move
/// `COMMON -> UNORDERED_ACCESS -> INDIRECT_ARGUMENT` in the prelude and back
/// to `COMMON` once at [`Self::end`] (or [`HostList::close`]).
pub(crate) struct HoistedClamps {
    hoistable: bool,
    restored: bool,
    jobs: Vec<Job>,
    payload: Vec<[u32; 8]>,
    upload: Option<JobUpload>,
    chunks: Vec<ID3D12Resource>,
    used: usize,
    at: u64,
    kept: Vec<ID3D12Resource>,
}

// SAFETY: D3D12 resources are free-threaded; mutated under the list's mutex.
unsafe impl Send for HoistedClamps {}

impl Default for HoistedClamps {
    fn default() -> Self {
        Self {
            hoistable: true,
            restored: false,
            jobs: Vec::new(),
            payload: Vec::new(),
            upload: None,
            chunks: Vec::new(),
            used: 0,
            at: 0,
            kept: Vec::new(),
        }
    }
}

impl HoistedClamps {
    pub(crate) fn reset(&mut self) {
        self.hoistable = true;
        self.restored = false;
        self.jobs.clear();
        self.payload.clear();
        self.used = 0;
        self.at = 0;
        self.kept.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    fn min_stride(kind: u32) -> Option<u32> {
        match kind {
            0 => Some(16),
            1 => Some(20),
            2 | 10 => Some(12),
            _ => None,
        }
    }

    fn byte_len(gpu: u64, (kind, stride, records): (u32, u32, u32)) -> Option<u64> {
        let min = Self::min_stride(kind)?;
        if records == 0 || stride < min || !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
            return None;
        }
        let len = u64::from(stride).checked_mul(u64::from(records))?;
        let aligned = len.checked_next_multiple_of(16)?;
        (aligned <= HOIST_CHUNK).then_some(len)
    }

    pub(crate) fn can_accept(&self, gpu: u64, spec: (u32, u32, u32)) -> bool {
        self.hoistable
            && u32::try_from(self.jobs.len()).is_ok()
            && batch_clamp().is_some()
            && Self::byte_len(gpu, spec).is_some()
    }

    fn keep_once(&mut self, resource: &ID3D12Resource) {
        let raw = resource.as_raw();
        if !self.kept.iter().any(|r| r.as_raw() == raw) {
            self.kept.push(resource.clone());
        }
    }

    fn alloc_room(&mut self, bytes: u64) -> Option<(ID3D12Resource, u64)> {
        let aligned = bytes.next_multiple_of(16);
        if aligned > HOIST_CHUNK {
            return None;
        }
        if self.used > 0 && self.at + aligned <= HOIST_CHUNK {
            let at = self.at;
            self.at += aligned;
            return Some((self.chunks[self.used - 1].clone(), at));
        }
        if self.used == self.chunks.len() {
            // Device-local: the chunks are written as unordered access and read
            // as indirect arguments by the card (a host-visible shared heap is
            // neither, on a real D3D12 driver).
            let buffer = host()?.uav_buffer(HOIST_CHUNK)?;
            crate::host::name(&buffer, || "hoisted indirect arguments".into());
            self.chunks.push(buffer);
        }
        let i = self.used;
        self.used += 1;
        self.at = aligned;
        Some((self.chunks[i].clone(), 0))
    }

    fn ensure_upload(&mut self, needed_jobs: usize) -> bool {
        if self.upload.as_ref().is_some_and(|u| u.capacity >= needed_jobs) {
            return true;
        }
        let Some(host) = host() else {
            return false;
        };
        let capacity = needed_jobs.next_power_of_two().max(256);
        let Some(bytes) = (capacity as u64).checked_mul(32) else {
            return false;
        };
        let Some((buffer, data)) = host.upload_buffer(bytes) else {
            return false;
        };
        crate::host::name(&buffer, || format!("hoisted clamp jobs ({capacity})"));
        // SAFETY: a live upload buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if self.upload.is_some() && !self.payload.is_empty() {
            // SAFETY: `data` has room for `capacity >= self.payload.len()` 32-byte jobs.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.payload.as_ptr().cast::<u8>(),
                    data,
                    self.payload.len() * 32,
                );
            }
        }
        self.upload = Some(JobUpload {
            _buffer: buffer,
            mapped: data as usize,
            gpu,
            capacity,
        });
        true
    }

    pub(crate) fn push(
        &mut self,
        src_buf: &ID3D12Resource,
        gpu: u64,
        spec: (u32, u32, u32),
    ) -> Option<(ID3D12Resource, u64)> {
        if !self.can_accept(gpu, spec) {
            return None;
        }
        let len = Self::byte_len(gpu, spec)?;
        let slot = self.jobs.len();
        if !self.ensure_upload(slot + 1) {
            return None;
        }
        let (dst, dst_at) = self.alloc_room(len)?;
        // SAFETY: live D3D12 buffers.
        let (buf_gpu, dst_gpu) = unsafe { (src_buf.GetGPUVirtualAddress(), dst.GetGPUVirtualAddress()) };
        let (src_gpu, src_offset) = match gpu.checked_sub(buf_gpu) {
            Some(rel) if rel.checked_add(len).is_some_and(|e| e <= u64::from(u32::MAX)) => (buf_gpu, rel as u32),
            _ => (gpu, 0),
        };
        self.keep_once(src_buf);
        let (kind, stride, records) = spec;
        let words = [kind, stride, records, src_offset, dst_at as u32, 0, 0, 0];
        if let Some(u) = &self.upload {
            // SAFETY: `ensure_upload(slot + 1)` guarantees `[slot * 32 .. (slot + 1) * 32)`
            // lies within the persistent mapped upload buffer.
            unsafe {
                (u.mapped as *mut [u32; 8]).add(slot).write(words);
            }
        }
        self.payload.push(words);
        self.jobs.push(Job { src_gpu, dst_gpu });
        Some((dst, dst_at))
    }

    /// Ends the hoistable prefix on `list`: transitions every used scratch
    /// chunk back from `INDIRECT_ARGUMENT` to `COMMON` once, and blocks
    /// further hoisting on this list until `reset()`.
    pub(crate) fn end(&mut self, list: &ID3D12GraphicsCommandList) {
        self.hoistable = false;
        if self.restored || self.used == 0 {
            return;
        }
        self.restored = true;
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = self.chunks[..self.used]
            .iter()
            .map(|b| {
                transition(
                    b,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                    D3D12_RESOURCE_STATE_COMMON,
                )
            })
            .collect();
        // SAFETY: an open list and live scratch chunks in `INDIRECT_ARGUMENT`.
        unsafe { list.ResourceBarrier(&barriers) };
    }

    /// Records the batched hoisted clamp pass(es) into `prelude`, moving each
    /// used scratch chunk `COMMON -> UNORDERED_ACCESS -> INDIRECT_ARGUMENT`.
    pub(crate) fn record_prelude(
        &self,
        prelude: &ID3D12GraphicsCommandList,
        budget_resource: &ID3D12Resource,
        budget_gpu: u64,
    ) -> bool {
        if self.jobs.is_empty() || self.used == 0 {
            return false;
        }
        let (Some(c), Some(upload)) = (batch_clamp(), &self.upload) else {
            return false;
        };
        let mut pre = Vec::with_capacity(self.used + 1);
        // Earlier lists' shader writes to the title's argument buffers first.
        pre.push(uav_barrier_of(None));
        for chunk in &self.chunks[..self.used] {
            pre.push(transition(
                chunk,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            ));
        }
        // SAFETY: an open prelude list; root signature, PSO, job buffer,
        // budget buffer, title source buffers (`kept`) and scratch chunks all
        // live through the submission.
        unsafe {
            prelude.ResourceBarrier(&pre);
            prelude.SetComputeRootSignature(&c.root);
            prelude.SetPipelineState(&c.pso);
            prelude.SetComputeRootShaderResourceView(1, upload.gpu);
            prelude.SetComputeRootUnorderedAccessView(4, budget_gpu);
            let mut run_start = 0usize;
            while run_start < self.jobs.len() {
                let key = (self.jobs[run_start].src_gpu, self.jobs[run_start].dst_gpu);
                let mut run_end = run_start + 1;
                while run_end < self.jobs.len()
                    && (self.jobs[run_end].src_gpu, self.jobs[run_end].dst_gpu) == key
                {
                    run_end += 1;
                }
                if run_start > 0 {
                    // Full UAV barrier between runs: keeps both `budget` and
                    // adjacent 16-byte-aligned slices of the same scratch
                    // chunk coherent on wide-cache-line GPUs.
                    prelude.ResourceBarrier(&[uav_barrier_of(None)]);
                }
                let constants = [run_start as u32, (run_end - run_start) as u32, 0, 0];
                prelude.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                prelude.SetComputeRootShaderResourceView(2, key.0);
                prelude.SetComputeRootUnorderedAccessView(3, key.1);
                CLAMP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                prelude.Dispatch(1, 1, 1);
                run_start = run_end;
            }
            let mut post = Vec::with_capacity(self.used + 1);
            post.push(uav_barrier_of(Some(budget_resource)));
            for chunk in &self.chunks[..self.used] {
                post.push(transition(
                    chunk,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                ));
            }
            prelude.ResourceBarrier(&post);
        }
        true
    }
}

/// Whether `spec` at `gpu` can be hoisted into `list`'s prelude.
pub(crate) fn can_hoist(list: &HostList, gpu: u64, spec: (u32, u32, u32)) -> bool {
    list.hoisted().can_accept(gpu, spec)
}

/// Queues `spec` at `gpu` in `src_buf` to be clamped in `list`'s prelude into
/// one of `list`'s own scratch chunks. Returns `(chunk, offset)` ready to be
/// read in `INDIRECT_ARGUMENT`, or `None` if hoisting is unavailable.
pub(crate) fn hoist(
    list: &HostList,
    src_buf: &ID3D12Resource,
    gpu: u64,
    spec: (u32, u32, u32),
) -> Option<(ID3D12Resource, u64)> {
    list.budget()?;
    list.hoisted().push(src_buf, gpu, spec)
}

/// The record's argument bytes (its payload): the least stride of `kind`.
fn payload(kind: u32) -> Option<u64> {
    min_stride(kind).map(u64::from)
}

/// The scratch span `records` records need: `(records - 1) * stride +
/// payload`, the last record ending at its argument payload (the title's
/// buffers are tight, so `stride * records` read past the last record's
/// arguments into memory the title did not allocate).
fn required_bytes((kind, stride, records): (u32, u32, u32)) -> Option<u64> {
    let payload = payload(kind)?;
    if records == 0 {
        return Some(0);
    }
    let last = u64::from(records - 1).checked_mul(u64::from(stride))?;
    last.checked_add(payload).filter(|&bytes| bytes <= u64::from(u32::MAX))
}

/// Validates record alignment and returns the scratch byte span
/// ([`required_bytes`]).
fn validate_layout(
    layout: (u32, u32, u32),
    src_offset: u64,
    count_offset: Option<u64>,
) -> Option<u64> {
    let (kind, stride, _) = layout;
    let min = min_stride(kind)?;
    if stride < min || !stride.is_multiple_of(4) || !src_offset.is_multiple_of(4) {
        return None;
    }
    if count_offset.is_some_and(|off| !off.is_multiple_of(4)) {
        return None;
    }
    required_bytes(layout)
}

/// Records, in `list`, the non-destructive clamp of up to `records` records of
/// `kind` (PC argument numbering) `stride` bytes apart from `src` (and bounded
/// by the 32-bit count in `count` when given) into a device-local scratch
/// buffer, leaving that scratch buffer in `INDIRECT_ARGUMENT` at offset `0`.
/// Replaces the list's compute root signature and pipeline (callers bind
/// theirs after). `None` if the clamp or alignment is not available.
pub(crate) fn bound(
    list: &HostList,
    src: (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    spec: (u32, u32, u32),
) -> Option<ID3D12Resource> {
    let bytes = validate_layout(spec, src.1, count.map(|(_, off)| off))?;
    let (c, budget, dst) = (clamp()?, list.budget()?, list.indirect_scratch(bytes)?);
    record_clamp(c, &list.list, &dst, budget, src, count, spec).then_some(dst)
}

/// [`bound`] for a window of records clamped ahead of the draws that will
/// read them (`crate::draw`'s windows; no count buffer). The records past
/// the ones the title filled are whatever the buffer holds, and each
/// record clamped charges the list's budget when it is clamped: a slot of
/// all ones, clamped to the most a mesh dispatch runs (65,535 groups),
/// charged a sixteenth of the budget, so sixteen of them emptied every
/// draw of the list after them (run 93: "the largest group count asked for
/// 4294967295", 43 to 153 such records a frame). Here a record that asks
/// for more than can run is left empty and charges nothing; a record
/// clamped alone is clamped and charged as before.
pub(crate) fn bound_ahead(
    list: &HostList,
    src: (&ID3D12Resource, u64),
    spec: (u32, u32, u32),
) -> Option<ID3D12Resource> {
    let bytes = validate_layout(spec, src.1, None)?;
    let (c, budget, dst) = (clamp()?, list.budget()?, list.indirect_scratch(bytes)?);
    record_clamp_as(c, &list.list, &dst, budget, src, None, spec, true).then_some(dst)
}

fn record_clamp(
    c: &Clamp,
    l: &ID3D12GraphicsCommandList,
    dst: &ID3D12Resource,
    budget: u64,
    src: (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    spec: (u32, u32, u32),
) -> bool {
    record_clamp_as(c, l, dst, budget, src, count, spec, false)
}

/// [`record_clamp`]; `ahead`: the records are a window clamped ahead of
/// their draws ([`bound_ahead`]).
#[allow(clippy::too_many_arguments)]
fn record_clamp_as(
    c: &Clamp,
    l: &ID3D12GraphicsCommandList,
    dst: &ID3D12Resource,
    budget: u64,
    (src, src_offset): (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    (kind, stride, records): (u32, u32, u32),
    ahead: bool,
) -> bool {
    let Some(bytes) = validate_layout(
        (kind, stride, records),
        src_offset,
        count.map(|(_, off)| off),
    ) else {
        return false;
    };
    // SAFETY: live D3D12 buffer resources.
    let (src_base, dst_gpu) = unsafe { (src.GetGPUVirtualAddress(), dst.GetGPUVirtualAddress()) };
    if !src_base.is_multiple_of(4) || !dst_gpu.is_multiple_of(4) || !budget.is_multiple_of(4) {
        return false;
    }
    let (src_gpu, args_offset) = match src_offset
        .checked_add(bytes)
        .filter(|&end| end <= u64::from(u32::MAX))
    {
        Some(_) => (src_base, src_offset as u32),
        None => {
            let Some(rebased) = src_base.checked_add(src_offset) else {
                return false;
            };
            (rebased, 0)
        }
    };
    let (count_gpu, has_count, count_offset) = match count {
        Some((buf, off)) => {
            // SAFETY: a live D3D12 buffer resource.
            let base = unsafe { buf.GetGPUVirtualAddress() };
            if !base.is_multiple_of(4) {
                return false;
            }
            match off.checked_add(4).filter(|&end| end <= u64::from(u32::MAX)) {
                Some(_) => (base, 1u32, off as u32),
                None => {
                    let Some(rebased) = base.checked_add(off) else {
                        return false;
                    };
                    (rebased, 1u32, 0u32)
                }
            }
        }
        None => (src_gpu, 0u32, 0u32),
    };
    let constants = [kind, stride, records, args_offset, has_count | (u32::from(ahead) << 1), count_offset];
    // SAFETY: an open list; our root signature and pipeline; live source,
    // count, scratch destination, and budget buffers. `src` and `count` stay
    // in `COMMON` / `GENERIC_READ` via implicit buffer promotion while `dst`
    // transitions `COMMON -> UNORDERED_ACCESS -> INDIRECT_ARGUMENT`.
    // Diagnostics: counted; `VICTORY_BARRIER_TEST=indirect` leaves the two
    // waits for every earlier shader write out (the state changes stay).
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::IndirectBounded, 1);
    #[cfg(feature = "diagnostics")]
    let waits = crate::copystat::barrier_test() & 2 == 0;
    #[cfg(not(feature = "diagnostics"))]
    let waits = true;
    let first = [
        uav_barrier(),
        transition(
            dst,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
    ];
    let second = [
        uav_barrier(),
        transition(
            dst,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        ),
    ];
    let skip = usize::from(!waits);
    unsafe {
        // Earlier shader writes (the title's argument and count passes) first.
        l.ResourceBarrier(&first[skip..]);
        if records > 0 {
            CLAMP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            l.SetComputeRootSignature(&c.root);
            l.SetPipelineState(&c.pso);
            l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, src_gpu);
            l.SetComputeRootShaderResourceView(2, count_gpu);
            l.SetComputeRootUnorderedAccessView(3, dst_gpu);
            l.SetComputeRootUnorderedAccessView(4, budget);
            l.Dispatch(records.div_ceil(64), 1, 1);
        }
        l.ResourceBarrier(&second[skip..]);
    }
    true
}

/// After indirect execution (or if preparation failed after [`bound`]):
/// returns the scratch buffer to the common state for the next call.
pub(crate) fn done(list: &HostList, scratch: &ID3D12Resource) {
    done_on(&list.list, scratch);
}

fn done_on(list: &ID3D12GraphicsCommandList, scratch: &ID3D12Resource) {
    // SAFETY: an open list and a live scratch buffer in INDIRECT_ARGUMENT.
    unsafe {
        list.ResourceBarrier(&[transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    uav_barrier_of(None)
}

fn uav_barrier_of(resource: Option<&ID3D12Resource>) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(resource.map(|r| {
                    // SAFETY: a borrowed pointer; the barrier never releases it.
                    unsafe { std::mem::transmute_copy(r) }
                })),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    const WORKER_CS: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer hits : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint prev;
    hits.InterlockedAdd(0, 1, prev);
}
"#;

    struct WarpHarness {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        submitted: u64,
        event: HANDLE,
        clamp: Clamp,
        worker: Clamp,
    }

    impl WarpHarness {
        fn new() -> Option<Self> {
            // SAFETY: plain DXGI WARP query and D3D12 test harness setup.
            unsafe {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
                let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    return None;
                }
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                let device = device?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                let clamp = compile(SOURCE, s!("clamp")).expect("clamp shader must compile on WARP");
                let worker = compile_cs(&device, WORKER_CS).expect("worker shader must compile");
                Some(Self {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    submitted: 0,
                    event,
                    clamp,
                    worker,
                })
            }
        }

        /// CPU-mapped L0 write-back buffer matching `Shadow`'s heap type.
        fn shared_buffer(&self, size: u64) -> (ID3D12Resource, *mut u8, u64) {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(4),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain buffer creation and persistent CPU mapping.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
                let buffer = buffer.unwrap();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                let gpu = buffer.GetGPUVirtualAddress();
                (buffer, data.cast(), gpu)
            }
        }

        /// Device-local `DEFAULT`-heap UAV scratch buffer.
        fn default_uav(&self, size: u64) -> ID3D12Resource {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(64 << 10),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: plain device-local buffer creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
            }
            buffer.unwrap()
        }

        fn signature(&self, kind: u32, stride: u32) -> ID3D12CommandSignature {
            let arg = D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
                ..Default::default()
            };
            let desc = D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: stride,
                NumArgumentDescs: 1,
                pArgumentDescs: &arg,
                NodeMask: 0,
            };
            let mut sig: Option<ID3D12CommandSignature> = None;
            // SAFETY: valid single-argument command signature description.
            unsafe {
                self.device
                    .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig)
                    .unwrap();
            }
            sig.unwrap()
        }

        fn submit_and_wait(&mut self) {
            self.submitted += 1;
            // SAFETY: closing, executing, waiting on fence, and resetting our list.
            unsafe {
                self.list.Close().unwrap();
                let cmd: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(cmd)]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence
                    .SetEventOnCompletion(self.submitted, self.event)
                    .unwrap();
                WaitForSingleObject(self.event, INFINITE);
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }
    }

    fn compile_cs(device: &ID3D12Device, source: &str) -> windows::core::Result<Clamp> {
        make_clamp(device, &crate::dxc::compile(source, "main", "cs_5_1")?)
    }

    /// The compare of copies in the card's memory (`crate::host`), on the
    /// software adapter. Told 1 it flags the pieces whose bytes are not
    /// what was hashed last and leaves the hashes: a piece a shader changed
    /// and a later one changed back then looks untouched, which is right
    /// only if the piece was fetched after both. Told 2 (submissions sent
    /// ahead with the relay, whose pieces are fetched flight by flight) it
    /// stores the hash it made: each compare flags what changed since the
    /// compare before it, also a change back.
    #[test]
    fn warp_card_compare_stores_the_hash_it_made_when_told() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        let compare = compile_cs(&warp.device, crate::host::CARD_COMPARE).expect("the card compare must compile on WARP");
        const PIECE: usize = 64 << 10;
        const PIECES: u32 = 3;
        let size = PIECE * PIECES as usize;
        let (_now, now_ptr, now_gpu) = warp.shared_buffer(size as u64);
        let (_hashes, _, hashes_gpu) = warp.shared_buffer(16 * u64::from(PIECES));
        let (_differs, differs_ptr, differs_gpu) = warp.shared_buffer(64);
        let content: Vec<u8> = (0..size).map(|i| (i * 31 + i / 251) as u8).collect();
        // SAFETY: the mapping holds `size` bytes.
        unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), now_ptr, size) };
        // One pass over the three pieces; what it left in `differs` (7s
        // where it stored nothing).
        let run = |warp: &mut WarpHarness, mode: u32| -> [u32; 3] {
            // SAFETY: our open list, live buffers, the compare's own root
            // signature; the mappings hold what is read and written.
            unsafe {
                for piece in 0..PIECES as usize {
                    differs_ptr.cast::<u32>().add(piece).write_volatile(7);
                }
                warp.list.SetComputeRootSignature(&compare.root);
                warp.list.SetPipelineState(&compare.pso);
                let constants = [0u32, 0, size as u32, PIECES, 0, mode];
                warp.list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                warp.list.SetComputeRootShaderResourceView(1, now_gpu);
                warp.list.SetComputeRootUnorderedAccessView(2, hashes_gpu);
                warp.list.SetComputeRootUnorderedAccessView(3, differs_gpu);
                warp.list.Dispatch(PIECES.div_ceil(64), 1, 1);
            }
            warp.submit_and_wait();
            // SAFETY: as above.
            [0, 1, 2].map(|piece| unsafe { differs_ptr.cast::<u32>().add(piece).read_volatile() })
        };
        let at = PIECE + 1234;
        // SAFETY (each): inside the mapping.
        let set = |value: u8| unsafe { now_ptr.add(at).write_volatile(value) };
        // 0: the hashes are stored, nothing is flagged.
        assert_eq!(run(&mut warp, 0), [7, 7, 7]);
        assert_eq!(run(&mut warp, 1), [0, 0, 0]);
        // A change in the second piece: flagged, and with 1 flagged again
        // (the hash stays the old one).
        set(content[at] ^ 0x40);
        assert_eq!(run(&mut warp, 1), [0, 1, 0]);
        assert_eq!(run(&mut warp, 1), [0, 1, 0]);
        // Changed back: with 1 the piece looks untouched.
        set(content[at]);
        assert_eq!(run(&mut warp, 1), [0, 0, 0]);
        // With 2 the change is flagged once, and the hash is the new one.
        set(content[at] ^ 0x40);
        assert_eq!(run(&mut warp, 2), [0, 1, 0]);
        assert_eq!(run(&mut warp, 2), [0, 0, 0]);
        // Changed back: flagged, where 1 saw nothing.
        set(content[at]);
        assert_eq!(run(&mut warp, 2), [0, 1, 0]);
        assert_eq!(run(&mut warp, 2), [0, 0, 0]);
    }

    /// The last record's span is its payload: a tight title buffer asks for
    /// `(records - 1) * stride + payload` bytes, never a whole last stride
    /// (which read past the buffer and past the scratch).
    #[test]
    fn resolved_bytes_of_a_tight_buffer_end_at_the_last_payload() {
        for (kind, payload) in [(0u32, 16u64), (1, 20), (2, 12), (10, 12)] {
            for (stride, records) in [(payload as u32, 1u32), (32, 1), (32, 7), (24, 4096)] {
                let bytes = required_bytes((kind, stride, records)).unwrap();
                assert_eq!(bytes, u64::from(records - 1) * u64::from(stride) + payload);
            }
            assert_eq!(required_bytes((kind, 32, 0)), Some(0));
        }
        // A layout whose last payload overflows 32 bits is refused, not clamped.
        assert_eq!(required_bytes((2, 12, u32::MAX)), None);
    }

    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    #[test]
    fn rejects_unaligned_or_undersized_layouts() {
        assert_eq!(validate_layout((0, 16, 4), 0, None), Some(64));
        assert_eq!(validate_layout((1, 20, 4), 64, Some(128)), Some(80));
        assert_eq!(validate_layout((2, 12, 0), 0, Some(0)), Some(0));
        // Tight: the last record ends at its payload, not at a whole stride.
        assert_eq!(validate_layout((10, 16, 2), 8, None), Some(28));
        assert_eq!(validate_layout((2, 24, 4096), 0, None), Some(4095 * 24 + 12));
        assert_eq!(validate_layout((0, 16, 1), 0, None), Some(16));
        // Undersized strides for each kind.
        assert_eq!(validate_layout((0, 12, 1), 0, None), None);
        assert_eq!(validate_layout((1, 16, 1), 0, None), None);
        assert_eq!(validate_layout((2, 8, 1), 0, None), None);
        assert_eq!(validate_layout((10, 8, 1), 0, None), None);
        // Unknown kind.
        assert_eq!(validate_layout((3, 16, 1), 0, None), None);
        // Unaligned stride, source offset, or count offset.
        assert_eq!(validate_layout((0, 18, 1), 0, None), None);
        assert_eq!(validate_layout((0, 16, 1), 2, None), None);
        assert_eq!(validate_layout((0, 16, 1), 0, Some(6)), None);
        // Total byte size exceeding 32-bit ByteAddressBuffer indexing.
        assert_eq!(validate_layout((1, 20, u32::MAX), 0, None), None);
    }

    /// WARP D3D12 execution test proving:
    /// (a) `countBuffer` with a small count (`3`) and `4,093` garbage tail slots
    ///     (`max = 4096`) does NOT blow `BUDGET` or zero valid dispatches in
    ///     this or subsequent `ExecuteIndirect` calls in the same list,
    /// (b) `src_args` (and `count_buf`) are NOT mutated in-place, and
    /// (c) full `stride` bytes (`stride = 24` > `12`) are copied for active
    ///     records into `dst_args` at offset `0`.
    #[test]
    fn warp_count_buffer_ignores_garbage_tail_and_preserves_source() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        const KIND_DISPATCH: u32 = 2;
        const STRIDE: u32 = 24; // 6 dwords: 3 dispatch group counts + 3 extra payload dwords
        const MAX: u32 = 4096;
        const ARGS_OFFSET: usize = 64; // non-zero offset with sentinel prefix
        const COUNT_OFFSET_1: usize = 128;
        const COUNT_OFFSET_2: usize = 192;

        let src_bytes = ARGS_OFFSET + (STRIDE as usize) * (MAX as usize) + 64;
        let (src_buf, src_ptr, _) = warp.shared_buffer(src_bytes as u64);
        let (count_buf, count_ptr, _) = warp.shared_buffer(512);
        let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (_hits_buf, hits_ptr, hits_gpu) = warp.shared_buffer(256);
        let (readback_buf, readback_ptr, _) = warp.shared_buffer((STRIDE * 8) as u64);
        let dst_buf = warp.default_uav(u64::from(STRIDE) * u64::from(MAX));
        let sig = warp.signature(KIND_DISPATCH, STRIDE);

        // Fill the entire source buffer with non-zero sentinel bytes, then
        // populate 3 valid records at `ARGS_OFFSET` and 4,093 huge garbage
        // records in `3..MAX` that would exhaust `BUDGET` ~2,000x over if read.
        let mut initial_src = vec![0xA5u8; src_bytes];
        let write_record = |buf: &mut [u8], idx: usize, words: [u32; 6]| {
            let base = ARGS_OFFSET + idx * (STRIDE as usize);
            for (w, val) in words.into_iter().enumerate() {
                buf[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
            }
        };
        write_record(
            &mut initial_src,
            0,
            [3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666],
        );
        write_record(
            &mut initial_src,
            1,
            [5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC],
        );
        // Record 2 exceeds 65,535 in X -> clamped to (65535, 1, 1).
        write_record(
            &mut initial_src,
            2,
            [100_000, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567],
        );
        for slot in 3..MAX as usize {
            // Each garbage slot would clamp to 65535 * 16 * 1 = 1,048,560 groups
            // (`cost = 67,107,840 ~= BUDGET / 2`), exhausting `BUDGET` immediately!
            write_record(
                &mut initial_src,
                slot,
                [0xFFFF_FFFF, 16, 1, 0xDEAD_BEEF, 0xBAD0_C0DE, slot as u32],
            );
        }

        // Second indirect call's source records (at offset 0 of a second buffer,
        // or later in `count_buf`): put 2 active records + garbage tail in a
        // second range to run back-to-back in the SAME command list.
        let mut initial_count = vec![0x5Au8; 512];
        initial_count[COUNT_OFFSET_1..COUNT_OFFSET_1 + 4].copy_from_slice(&3u32.to_le_bytes());
        initial_count[COUNT_OFFSET_2..COUNT_OFFSET_2 + 4].copy_from_slice(&2u32.to_le_bytes());

        // SAFETY: buffers are mapped with exact capacities above.
        unsafe {
            std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, src_bytes);
            std::ptr::copy_nonoverlapping(initial_count.as_ptr(), count_ptr, 512);
            (budget_ptr as *mut u32).write_volatile(0);
            (hits_ptr as *mut u32).write_volatile(0);
            std::ptr::write_bytes(readback_ptr, 0, (STRIDE * 8) as usize);
        }

        // Record Call #1 (`eff_count = 3` out of `MAX = 4096`), copy the first
        // 5 records of `dst_buf` out for inspection, then record Call #2
        // (`eff_count = 2` out of `MAX = 4096`) reusing `dst_buf` in the same list!
        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, ARGS_OFFSET as u64),
            Some((&count_buf, COUNT_OFFSET_1 as u64)),
            (KIND_DISPATCH, STRIDE, MAX),
        ));
        // SAFETY: open command list, valid worker PSO/root and buffers.
        unsafe {
            warp.list.SetComputeRootSignature(&warp.worker.root);
            warp.list.SetPipelineState(&warp.worker.pso);
            warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            warp.list.ExecuteIndirect(
                &sig,
                MAX,
                &dst_buf,
                0,
                Some(&count_buf),
                COUNT_OFFSET_1 as u64,
            );
        }
        done_on(&warp.list, &dst_buf);

        // Snapshot `dst_buf` after Call #1 (from `COMMON`, copy 5 * STRIDE bytes,
        // then return `dst_buf` to `COMMON` before Call #2).
        // SAFETY: open command list; `dst_buf` returned to `COMMON` by `done_on`.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list
                .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, u64::from(STRIDE * 5));
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }

        // Call #2 in the SAME command list (`eff_count = 2` -> records 0 and 1:
        // 6 + 5 = 11 groups). Would run empty if Call #1's tail blew `BUDGET`!
        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, ARGS_OFFSET as u64),
            Some((&count_buf, COUNT_OFFSET_2 as u64)),
            (KIND_DISPATCH, STRIDE, MAX),
        ));
        // SAFETY: open command list, valid worker PSO/root and buffers.
        unsafe {
            warp.list.SetComputeRootSignature(&warp.worker.root);
            warp.list.SetPipelineState(&warp.worker.pso);
            warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            warp.list.ExecuteIndirect(
                &sig,
                MAX,
                &dst_buf,
                0,
                Some(&count_buf),
                COUNT_OFFSET_2 as u64,
            );
        }
        done_on(&warp.list, &dst_buf);

        warp.submit_and_wait();

        // SAFETY: GPU execution finished; read mapped host results.
        let (hits, spent, after_src, after_count, clamped_words) = unsafe {
            let hits = (hits_ptr as *const u32).read_volatile();
            let spent = (budget_ptr as *const u32).read_volatile();
            let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), src_bytes).to_vec();
            let after_count = std::slice::from_raw_parts(count_ptr.cast_const(), 512).to_vec();
            let words = std::slice::from_raw_parts(
                readback_ptr.cast::<u32>().cast_const(),
                (STRIDE as usize / 4) * 5,
            )
            .to_vec();
            (hits, spent, after_src, after_count, words)
        };

        // Call #1: (3*2*1) + (5*1*1) + (65535*1*1) = 65,546 groups.
        // Call #2: (3*2*1) + (5*1*1) = 11 groups.
        let expected_groups = (6 + 5 + 65_535) + (6 + 5);
        assert_eq!(
            hits, expected_groups,
            "valid indirect dispatches must all execute"
        );
        assert_eq!(
            spent,
            expected_groups * 64,
            "budget must reflect only active records below countBuffer"
        );
        assert_eq!(
            after_src, initial_src,
            "src_args must never be mutated in-place"
        );
        assert_eq!(
            after_count, initial_count,
            "count_buf must never be mutated"
        );

        // Check `dst_args` records 0..3: clamped group counts + preserved extra stride words.
        assert_eq!(
            &clamped_words[0..6],
            &[3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666]
        );
        assert_eq!(
            &clamped_words[6..12],
            &[5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC]
        );
        assert_eq!(
            &clamped_words[12..18],
            &[65_535, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567]
        );
        // Inactive slot 3 was never copied from `src_args`.
        assert_ne!(clamped_words[21], 0xDEAD_BEEF);
    }

    /// WARP D3D12 test proving `BUDGET` saturates without 32-bit unsigned
    /// wraparound across >64 huge indexed-draw records (`count = None`), while
    /// preserving `StartIndexLocation`, `BaseVertexLocation`,
    /// `StartInstanceLocation`, and custom stride padding, and leaving
    /// `src_args` untouched.
    /// A window of mesh records clamped ahead: the filled ones come out as
    /// they are and are charged, the slots of all ones come out empty and
    /// charge nothing; clamped as single records the same slots are clamped
    /// to the most and spend the budget.
    #[test]
    fn warp_window_leaves_unfilled_slots_empty_and_uncharged() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        const KIND_MESH: u32 = 10;
        const STRIDE: u32 = 32;
        const RECORDS: u32 = 16;
        let filled = [5u32, 0, 519];
        let total = (STRIDE * RECORDS) as usize;
        let mut src = vec![0xffu8; total];
        for (i, groups) in filled.into_iter().enumerate() {
            let words = [groups, 1, 1, 0xAB00 + i as u32];
            for (w, word) in words.into_iter().enumerate() {
                let at = i * STRIDE as usize + w * 4;
                src[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        for ahead in [true, false] {
            let (src_buf, src_ptr, _) = warp.shared_buffer(total as u64);
            let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
            let (readback_buf, readback_ptr, _) = warp.shared_buffer(total as u64);
            let dst_buf = warp.default_uav(total as u64);

            // SAFETY: mapped buffers of these sizes.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), src_ptr, total);
                std::ptr::write_bytes(budget_ptr, 0, 256);
                std::ptr::write_bytes(readback_ptr, 0, total);
            }
            assert!(record_clamp_as(
                &warp.clamp,
                &warp.list,
                &dst_buf,
                budget_gpu,
                (&src_buf, 0),
                None,
                (KIND_MESH, STRIDE, RECORDS),
                ahead,
            ));
            done_on(&warp.list, &dst_buf);
            // SAFETY: an open list; the scratch buffer is copied for the check.
            unsafe {
                warp.list.ResourceBarrier(&[transition(
                    &dst_buf,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                warp.list.CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, total as u64);
                warp.list.ResourceBarrier(&[transition(
                    &dst_buf,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COMMON,
                )]);
            }
            warp.submit_and_wait();
            // SAFETY: the work is done; mapped buffers.
            let (spent, largest, out) = unsafe {
                (
                    (budget_ptr as *const u32).read_volatile(),
                    (budget_ptr as *const u32).add(1).read_volatile(),
                    std::slice::from_raw_parts(readback_ptr.cast::<u32>().cast_const(), total / 4).to_vec(),
                )
            };
            for (i, groups) in filled.into_iter().enumerate() {
                let rec = &out[i * 8..i * 8 + 4];
                assert_eq!(rec, [groups, u32::from(groups > 0).max(1).min(1), 1, 0xAB00 + i as u32], "filled record {i}, ahead {ahead}");
            }
            let charged_filled = (5 + 519) * 128;
            if ahead {
                for i in filled.len()..RECORDS as usize {
                    assert_eq!(out[i * 8..i * 8 + 3], [0, 0, 0], "unfilled slot {i} must come out empty");
                    if i + 1 == RECORDS as usize {
                        // The scratch span ends at the last record's argument
                        // payload (the title's buffers are tight): the words
                        // past it are outside the scratch and nothing is said
                        // about them (WARP hands out zeros there, RADV an
                        // earlier test's bytes).
                    } else {
                        assert_eq!(
                            out[i * 8 + 3..i * 8 + 8],
                            [u32::MAX; 5],
                            "the words after the record are copied as they are"
                        );
                    }
                }
                assert_eq!(spent, charged_filled, "unfilled slots charge nothing");
                assert_eq!(largest, 519, "and are not counted as asked for");
            } else {
                // Thirteen slots at 65,535 groups: sixteen would be all of it.
                assert_eq!(spent, charged_filled + 13 * 65535 * 128, "clamped one by one, each slot is charged the most");
                assert_eq!(largest, u32::MAX);
            }
        }
    }

    #[test]
    fn warp_budget_saturates_without_wraparound_and_preserves_draw_fields() {
        let Some(mut warp) = WarpHarness::new() else {
            return;
        };
        const KIND_INDEXED: u32 = 1;
        const STRIDE: u32 = 24; // 5 indexed-draw words + 1 trailing custom word
        const RECORDS: u32 = 128; // 2 waves of 64: old InterlockedAdd wrapped 2^32 at 64!
        const BUDGET_CAP: u32 = 1 << 27;

        let total_bytes = (STRIDE * RECORDS) as usize;
        let (src_buf, src_ptr, _) = warp.shared_buffer(total_bytes as u64);
        let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (readback_buf, readback_ptr, _) = warp.shared_buffer(total_bytes as u64);
        let dst_buf = warp.default_uav(total_bytes as u64);

        let mut initial_src = vec![0u8; total_bytes];
        for i in 0..RECORDS as usize {
            let words: [u32; 6] = [
                u32::MAX,                    // IndexCount -> clamps to 1 << 24
                u32::MAX,                    // InstanceCount -> clamps to 4 (cost = 1 << 26)
                1000 + i as u32,             // StartIndexLocation
                (-200i32 + i as i32) as u32, // BaseVertexLocation
                5000 + i as u32,             // StartInstanceLocation
                0xCAFE_0000 | (i as u32),    // Extra stride word
            ];
            let base = i * (STRIDE as usize);
            for (w, val) in words.into_iter().enumerate() {
                initial_src[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
            }
        }

        // SAFETY: mapped buffers have sufficient capacity.
        unsafe {
            std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, total_bytes);
            (budget_ptr as *mut u32).write_volatile(0);
            std::ptr::write_bytes(readback_ptr, 0, total_bytes);
        }

        assert!(record_clamp(
            &warp.clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, 0),
            None,
            (KIND_INDEXED, STRIDE, RECORDS),
        ));
        done_on(&warp.list, &dst_buf);

        // SAFETY: open command list; copy clamped scratch buffer for verification.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list
                .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, total_bytes as u64);
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
        warp.submit_and_wait();

        // SAFETY: GPU work completed.
        let (spent, after_src, out_words) = unsafe {
            let spent = (budget_ptr as *const u32).read_volatile();
            let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), total_bytes).to_vec();
            let out_words = std::slice::from_raw_parts(
                readback_ptr.cast::<u32>().cast_const(),
                total_bytes / 4,
            )
            .to_vec();
            (spent, after_src, out_words)
        };

        assert_eq!(
            spent, BUDGET_CAP,
            "budget must saturate at BUDGET without wrapping"
        );
        assert_eq!(after_src, initial_src, "src_args must remain untouched");

        let mut kept = 0usize;
        let mut zeroed = 0usize;
        for i in 0..RECORDS as usize {
            let rec = &out_words[i * 6..(i + 1) * 6];
            if rec[0] == (1 << 24) && rec[1] == 4 {
                kept += 1;
            } else {
                assert_eq!((rec[0], rec[1]), (0, 0), "excess draw must be zeroed");
                zeroed += 1;
            }
            assert_eq!(rec[2], 1000 + i as u32);
            assert_eq!(rec[3], (-200i32 + i as i32) as u32);
            assert_eq!(rec[4], 5000 + i as u32);
            // The scratch span ends at the last record's argument payload
            // (the title's buffers are tight): the stride word past it is
            // outside the scratch, so nothing is said about it (WARP hands
            // out zeros there, RADV an earlier test's bytes).
            if i + 1 < RECORDS as usize {
                assert_eq!(rec[5], 0xCAFE_0000 | (i as u32));
            }
        }
        assert_eq!((kept, zeroed), (2, (RECORDS as usize) - 2));
    }

    /// Exercises `HostList::indirect_scratch`, `bound`, `done`, `Host::execute`,
    /// and `HostList::reset` end-to-end on the host device.
    #[test]
    fn host_list_reuses_and_grows_scratch_across_submissions() {
        let Some(host) = super::host() else {
            return;
        };
        let Ok(list) = host.new_list() else {
            return;
        };
        let Some((src_buf, src_ptr)) = host.shared_buffer(256 << 10) else {
            return;
        };
        let Some((count_buf, count_ptr)) = host.shared_buffer(256) else {
            return;
        };
        // SAFETY: mapped host buffers.
        unsafe {
            std::ptr::write_bytes(src_ptr, 0x7F, 256 << 10);
            (count_ptr as *mut u32).write_volatile(2);
        }

        // First two calls fit inside the initial 64 KiB scratch buffer and
        // must reuse the exact same `ID3D12Resource`.
        let b1 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (0, 16, 64)).unwrap();
        done(&list, &b1);
        let b2 = bound(&list, (&src_buf, 64), Some((&count_buf, 0)), (1, 20, 128)).unwrap();
        done(&list, &b2);
        assert_eq!(
            b1.as_raw(),
            b2.as_raw(),
            "scratch buffer within capacity must be reused"
        );

        // A call needing >64 KiB grows the scratch buffer mid-list while keeping
        // `b1` alive until reset.
        let b3 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 5000)).unwrap();
        done(&list, &b3);
        assert_ne!(
            b3.as_raw(),
            b1.as_raw(),
            "larger request must grow scratch buffer"
        );

        list.close();
        host.execute(&[&list]);
        list.reset();

        // After reset, requests up to the grown high-water mark reuse `b3` with
        // zero new allocations.
        let b4 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 4096)).unwrap();
        done(&list, &b4);
        assert_eq!(
            b4.as_raw(),
            b3.as_raw(),
            "grown scratch buffer must persist across reset"
        );
        list.close();
        host.execute(&[&list]);
        list.reset();
    }

    /// Whether the hoisted tests should run here. They record into a real
    /// host list's prelude; CI's Windows D3D12 driver access-violates there
    /// (STATUS_ACCESS_VIOLATION), while the target this layer runs on,
    /// Wine + vkd3d-proton, does not.
    fn hoisted_here() -> bool {
        std::env::var_os("WINEPREFIX").is_some()
    }

    #[test]
    fn batch_argument_clamp_builds() {
        if !hoisted_here() {
            return;
        }
        if super::host().is_none() {
            return;
        }
        assert!(super::batch_clamp().is_some());
    }

    /// Reads `words` 32-bit words from `buf` at byte offset `at` after execution.
    fn read_words(buf: &super::ID3D12Resource, at: u64, words: usize) -> Vec<u32> {
        let Some(host) = super::host() else {
            return Vec::new();
        };
        let bytes = (words * 4) as u64;
        let (read, ptr) = host.shared_buffer(bytes).unwrap();
        let list = host.new_list().unwrap();
        // SAFETY: open list; the chunk is in `INDIRECT_ARGUMENT` (hoisted, or
        // `done` left it there); the readback is a mapped shared buffer.
        unsafe {
            list.list.ResourceBarrier(&[super::transition(
                buf,
                super::D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                super::D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyBufferRegion(&read, 0, buf, at, bytes);
            list.list.ResourceBarrier(&[super::transition(
                buf,
                super::D3D12_RESOURCE_STATE_COPY_SOURCE,
                super::D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            )]);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: GPU completed; mapped shared buffer.
        unsafe {
            let base = ptr.cast::<u32>();
            (0..words).map(|i| base.add(i).read_volatile()).collect()
        }
    }

    /// Hoisted batched clamping matches inline per-draw clamping word-for-word
    /// across >64 single-record draws, mixed kinds (`0, 1, 2, 10`), a multi-
    /// record (>64) draw, and mid-wave `BUDGET` exhaustion — while leaving the
    /// title's source buffer completely unmodified.
    #[test]
    fn hoisted_clamps_match_inline_bounds_and_budget_order() {
        if !hoisted_here() {
            return;
        }
        let Some(host) = super::host() else {
            return;
        };
        const SRC_BYTES: usize = 64 << 10;
        let (src, src_ptr) = host.shared_buffer(SRC_BYTES as u64).unwrap();
        let mut initial = vec![0xA5u8; SRC_BYTES];
        let put = |buf: &mut [u8], off: usize, v: u32| {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };

        struct Spec {
            off: u64,
            kind: u32,
            stride: u32,
            records: u32,
        }
        let mut specs = Vec::new();
        let mut cur = 64usize;
        // 75 consecutive single-record jobs (spanning >1 full 64-lane wave).
        for i in 0..75u32 {
            let (kind, stride) = match i % 4 {
                0 => (0u32, 16u32),
                1 => (1u32, 20u32),
                2 => (2u32, 12u32),
                _ => (10u32, 12u32),
            };
            match kind {
                0 => {
                    put(&mut initial, cur, 5000 + i * 11);
                    put(&mut initial, cur + 4, 3);
                    put(&mut initial, cur + 8, 10 + i);
                    put(&mut initial, cur + 12, 20 + i);
                }
                1 => {
                    put(&mut initial, cur, 7000 + i * 13);
                    put(&mut initial, cur + 4, 2);
                    put(&mut initial, cur + 8, 100 + i);
                    put(&mut initial, cur + 12, (-30i32 + i as i32) as u32);
                    put(&mut initial, cur + 16, 200 + i);
                }
                2 => {
                    put(&mut initial, cur, 10 + (i % 7));
                    put(&mut initial, cur + 4, 2);
                    put(&mut initial, cur + 8, 1);
                }
                _ => {
                    // First mesh tests 65,535 clamp; remaining stay small so BUDGET crosses in the tail.
                    put(&mut initial, cur, if i == 3 { 70_000 } else { 8 + (i % 5) });
                    put(&mut initial, cur + 4, 4); // clamps to 1
                    put(&mut initial, cur + 8, 3); // clamps to 1
                }
            }
            specs.push(Spec {
                off: cur as u64,
                kind,
                stride,
                records: 1,
            });
            cur += stride as usize;
        }
        // Multi-record (>64) indexed draw.
        let multi_off = cur;
        for r in 0..70u32 {
            let b = multi_off + (r * 20) as usize;
            put(&mut initial, b, 3000 + r);
            put(&mut initial, b + 4, 2);
            put(&mut initial, b + 8, r);
            put(&mut initial, b + 12, r * 2);
            put(&mut initial, b + 16, r * 3);
        }
        specs.push(Spec {
            off: multi_off as u64,
            kind: 1,
            stride: 20,
            records: 70,
        });
        cur += 70 * 20;
        // 80 heavy indexed draws that exhaust BUDGET mid-wave (4,000,000 each).
        for i in 0..80u32 {
            put(&mut initial, cur, 1_000_000);
            put(&mut initial, cur + 4, 4);
            put(&mut initial, cur + 8, 700 + i);
            put(&mut initial, cur + 12, 800 + i);
            put(&mut initial, cur + 16, 900 + i);
            specs.push(Spec {
                off: cur as u64,
                kind: 1,
                stride: 20,
                records: 1,
            });
            cur += 20;
        }

        // SAFETY: mapped `SRC_BYTES` shared buffer; live buffer.
        let base_gpu = unsafe {
            std::ptr::copy_nonoverlapping(initial.as_ptr(), src_ptr, SRC_BYTES);
            src.GetGPUVirtualAddress()
        };

        // Hoisted path.
        let list_hoisted = host.new_list().unwrap();
        let mut hoisted_locs = Vec::with_capacity(specs.len());
        for s in &specs {
            let loc = super::hoist(
                &list_hoisted,
                &src,
                base_gpu + s.off,
                (s.kind, s.stride, s.records),
            )
            .unwrap();
            hoisted_locs.push(loc);
        }
        list_hoisted.close();
        host.execute(&[&list_hoisted]);

        // Inline reference path (one submission per readback, sharing one list budget).
        let list_inline = host.new_list().unwrap();
        let (rb_inline, rb_ptr) = host.shared_buffer(SRC_BYTES as u64).unwrap();
        let mut rb_offsets = Vec::with_capacity(specs.len());
        let mut rb_cur = 0u64;
        for s in &specs {
            let out = super::bound(&list_inline, (&src, s.off), None, (s.kind, s.stride, s.records)).unwrap();
            let bytes = u64::from(s.stride * s.records);
            // SAFETY: open list; `out` is in `INDIRECT_ARGUMENT` after `bound`.
            unsafe {
                list_inline.list.ResourceBarrier(&[super::transition(
                    &out,
                    super::D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                    super::D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list_inline.list.CopyBufferRegion(&rb_inline, rb_cur, &out, 0, bytes);
                list_inline.list.ResourceBarrier(&[super::transition(
                    &out,
                    super::D3D12_RESOURCE_STATE_COPY_SOURCE,
                    super::D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                )]);
            }
            super::done(&list_inline, &out);
            rb_offsets.push((rb_cur, bytes as usize));
            rb_cur += bytes;
        }
        list_inline.close();
        host.execute(&[&list_inline]);

        // SAFETY: GPU completed; verify title copy untouched and outputs identical.
        let after_src = unsafe { std::slice::from_raw_parts(src_ptr.cast_const(), SRC_BYTES).to_vec() };
        assert_eq!(after_src, initial, "title argument buffer must never be modified");

        let mut tail_kept = 0usize;
        let mut tail_zeroed = 0usize;
        for (idx, ((dst, at), (rb_off, bytes))) in hoisted_locs.iter().zip(rb_offsets.iter()).enumerate() {
            let got = read_words(dst, *at, *bytes / 4);
            // SAFETY: within mapped `rb_inline`.
            let expected = unsafe {
                let p = rb_ptr.add(*rb_off as usize).cast::<u32>();
                (0..*bytes / 4).map(|i| p.add(i).read_volatile()).collect::<Vec<_>>()
            };
            assert_eq!(got, expected, "job {idx} hoisted output must match inline bound()");
            if idx >= 76 {
                if got[0] == 1_000_000 {
                    tail_kept += 1;
                } else {
                    assert_eq!(got[0], 0);
                    tail_zeroed += 1;
                }
            }
        }
        assert!(tail_kept > 0 && tail_zeroed > 0, "tail must cross BUDGET mid-wave");
    }

    /// Multiple source buffers and >256 KiB chunk rollover preserve strict
    /// draw-order budget saturation across dispatch runs and reuse both chunks
    /// and upload buffer across `HostList::reset()`.
    #[test]
    fn hoisted_clamps_across_multiple_buffers_and_chunks_preserve_budget_order() {
        if !hoisted_here() {
            return;
        }
        use windows::core::Interface;
        let Some(host) = super::host() else {
            return;
        };
        let list = host.new_list().unwrap();
        let (src_a, ptr_a) = host.shared_buffer(4096).unwrap();
        let (src_b, ptr_b) = host.shared_buffer(4096).unwrap();
        // SAFETY: mapped 4096-byte shared buffers; live buffers.
        let (gpu_a, gpu_b) = unsafe {
            let write = |p: *mut u8, off: usize, count: u32, inst: u32, tag: u32| {
                let w = p.add(off).cast::<u32>();
                w.write(count);
                w.add(1).write(inst);
                w.add(2).write(tag);
                w.add(3).write(0);
                w.add(4).write(0);
            };
            write(ptr_a, 0, 0, 1, 0xAAAA);
            write(ptr_a, 32, 1 << 22, 4, 0x1111); // BUDGET / 8
            write(ptr_b, 32, 1 << 22, 4, 0x2222); // BUDGET / 8
            (src_a.GetGPUVirtualAddress(), src_b.GetGPUVirtualAddress())
        };

        for cycle in 0..2u32 {
            let mut probes = Vec::new();
            for i in 0..4u32 {
                let (s, g) = if i % 2 == 0 { (&src_a, gpu_a) } else { (&src_b, gpu_b) };
                probes.push(super::hoist(&list, s, g + 32, (1, 20, 1)).unwrap());
            }
            // Spill across 256 KiB chunk boundary (8,192 * 32B = 256 KiB).
            let mut spilled = false;
            let first_chunk = probes[0].0.as_raw();
            for _ in 0..8_250 {
                let (c, _) = super::hoist(&list, &src_a, gpu_a, (1, 20, 1)).unwrap();
                if c.as_raw() != first_chunk {
                    spilled = true;
                }
            }
            assert!(spilled, "must span at least two 256 KiB hoisted scratch chunks");
            for i in 0..6u32 {
                let (s, g) = if i % 2 == 0 { (&src_a, gpu_a) } else { (&src_b, gpu_b) };
                probes.push(super::hoist(&list, s, g + 32, (1, 20, 1)).unwrap());
            }
            list.close();
            host.execute(&[&list]);

            for (idx, (dst, at)) in probes.iter().enumerate() {
                let w = read_words(dst, *at, 5);
                if idx < 8 {
                    assert_eq!((w[0], w[1]), (1 << 22, 4), "cycle {cycle} probe {idx} must fit");
                } else {
                    assert_eq!(w[0], 0, "cycle {cycle} probe {idx} must zero past BUDGET");
                }
            }
            list.reset();
        }
    }

    /// Re-submitting the same closed hoisted list twice re-clamps cleanly into
    /// its own scratch chunks without mutating the title's source buffer.
    #[test]
    fn hoisted_list_resubmitted_twice_reclamps_cleanly() {
        if !hoisted_here() {
            return;
        }
        let Some(host) = super::host() else {
            return;
        };
        let list = host.new_list().unwrap();
        let (src, ptr) = host.shared_buffer(256).unwrap();
        // SAFETY: mapped buffer; live buffer.
        let gpu = unsafe {
            let p = ptr.cast::<u32>();
            p.write(u32::MAX);
            p.add(1).write(u32::MAX);
            p.add(2).write(42);
            p.add(3).write(7);
            p.add(4).write(99);
            src.GetGPUVirtualAddress()
        };
        let (dst, at) = super::hoist(&list, &src, gpu, (1, 20, 1)).unwrap();
        list.close();
        for _ in 0..2 {
            host.execute(&[&list]);
            assert_eq!(read_words(&dst, at, 5), vec![1 << 24, 4, 42, 7, 99]);
            // SAFETY: title buffer untouched.
            assert_eq!(unsafe { ptr.cast::<u32>().read_volatile() }, u32::MAX);
        }
    }

    /// An earlier list's compute write to a title argument buffer is visible
    /// to the hoisted prelude of the following list.
    #[test]
    fn hoisted_prelude_orders_after_earlier_list_compute_write() {
        if !hoisted_here() {
            return;
        }
        let Some(host) = super::host() else {
            return;
        };
        const WRITER: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    dst.Store3(0, uint3(90000, 1, 1));
}
"#;
        let writer = super::compile(WRITER, super::s!("writer")).unwrap();
        let (src, ptr) = host.shared_buffer(256).unwrap();
        // SAFETY: mapped buffer; live buffer.
        let gpu = unsafe {
            std::ptr::write_bytes(ptr, 0, 256);
            src.GetGPUVirtualAddress()
        };
        let write_list = host.new_list().unwrap();
        // SAFETY: open list, live compute pipeline and UAV buffer.
        unsafe {
            write_list.list.SetComputeRootSignature(&writer.root);
            write_list.list.SetPipelineState(&writer.pso);
            write_list.list.SetComputeRootUnorderedAccessView(0, gpu);
            write_list.list.Dispatch(1, 1, 1);
        }
        write_list.close();

        let hoisted_list = host.new_list().unwrap();
        let (dst, at) = super::hoist(&hoisted_list, &src, gpu, (2, 12, 1)).unwrap();
        hoisted_list.close();

        host.execute(&[&write_list, &hoisted_list]);
        assert_eq!(read_words(&dst, at, 3), vec![65_535, 1, 1]);
    }

    /// The dispatch count the hoisted clamp saves: the inline clamp records
    /// one compute pass per draw, the batched prelude one per run of draws
    /// reading the same argument buffer into the same scratch chunk.
    #[test]
    fn hoisted_clamps_cost_one_pass_per_run_of_draws() {
        if !hoisted_here() {
            return;
        }
        let Some(host) = super::host() else {
            return;
        };
        const DRAWS: usize = 1024;
        const STRIDE: u32 = 20;
        let (src, ptr) = host.shared_buffer((DRAWS * STRIDE as usize) as u64).unwrap();
        // SAFETY: mapped buffer; live buffer.
        let base_gpu = unsafe {
            for i in 0..DRAWS {
                let p = ptr.add(i * STRIDE as usize).cast::<u32>();
                p.write(300);
                p.add(1).write(1);
                p.add(2).write(0);
                p.add(3).write(0);
                p.add(4).write(0);
            }
            src.GetGPUVirtualAddress()
        };

        let list = host.new_list().unwrap();
        let before = super::clamp_passes();
        for i in 0..DRAWS {
            let out = super::bound(&list, (&src, (i * STRIDE as usize) as u64), None, (1, STRIDE, 1)).unwrap();
            super::done(&list, &out);
        }
        let inline = super::clamp_passes() - before;

        let (budget, budget_gpu) = {
            list.budget().unwrap();
            let (res, gpu) = list.budget_resource().unwrap();
            (res.clone(), gpu)
        };
        let mut hoisted_locs = Vec::with_capacity(DRAWS);
        for i in 0..DRAWS {
            hoisted_locs.push(
                super::hoist(&list, &src, base_gpu + (i * STRIDE as usize) as u64, (1, STRIDE, 1)).unwrap(),
            );
        }
        let prelude = host.new_list().unwrap();
        let before = super::clamp_passes();
        assert!(list.hoisted().record_prelude(&prelude.list, &budget, budget_gpu));
        let hoisted = super::clamp_passes() - before;

        assert_eq!(inline, DRAWS as u64, "the inline clamp records a pass per draw");
        assert_eq!(hoisted, 1, "the batched prelude records one pass for the run");
    }

    /// Release benchmark comparing inline `bound()` + `done()` vs prelude-
    /// hoisted batched clamping across 9,000 indirect indexed draws.
    ///
    /// Run with:
    /// `cargo test --release -p victory-gpu --locked -- --ignored --nocapture benchmark_hoisted_vs_inline_indirect_clamps`
    #[test]
    #[ignore = "benchmark"]
    fn benchmark_hoisted_vs_inline_indirect_clamps() {
        let Some(host) = super::host() else {
            return;
        };
        let list_inline = host.new_list().unwrap();
        let list_hoisted = host.new_list().unwrap();
        const DRAWS: usize = 9_000;
        const STRIDE: u32 = 20;
        let (src, ptr) = host.shared_buffer((DRAWS * STRIDE as usize) as u64).unwrap();
        // SAFETY: mapped buffer; live buffer.
        let base_gpu = unsafe {
            for i in 0..DRAWS {
                let p = ptr.add(i * STRIDE as usize).cast::<u32>();
                p.write(300);
                p.add(1).write(1);
                p.add(2).write(0);
                p.add(3).write(0);
                p.add(4).write(0);
            }
            src.GetGPUVirtualAddress()
        };

        let run_inline = || {
            for i in 0..DRAWS {
                let out = super::bound(&list_inline, (&src, (i * STRIDE as usize) as u64), None, (1, STRIDE, 1)).unwrap();
                super::done(&list_inline, &out);
            }
            list_inline.close();
            host.execute(&[&list_inline]);
            list_inline.reset();
        };
        let run_hoisted = || {
            for i in 0..DRAWS {
                let _ = super::hoist(&list_hoisted, &src, base_gpu + (i * STRIDE as usize) as u64, (1, STRIDE, 1)).unwrap();
            }
            list_hoisted.close();
            host.execute(&[&list_hoisted]);
            list_hoisted.reset();
        };

        for _ in 0..3 {
            run_inline();
            run_hoisted();
        }
        const ITERS: u32 = 20;
        let t0 = std::time::Instant::now();
        for _ in 0..ITERS {
            run_inline();
        }
        let inline_ms = t0.elapsed().as_secs_f64() * 1e3 / f64::from(ITERS);

        let t1 = std::time::Instant::now();
        for _ in 0..ITERS {
            run_hoisted();
        }
        let hoisted_ms = t1.elapsed().as_secs_f64() * 1e3 / f64::from(ITERS);

        eprintln!(
            "{DRAWS} indirect draw clamps: inline {inline_ms:.2} ms/frame vs hoisted {hoisted_ms:.2} ms/frame ({:.1}x)",
            inline_ms / hoisted_ms.max(1e-6)
        );
    }
}
