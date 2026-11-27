//! Draws on the host GPU (decision 0007, step 3). A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

/// Whether an overflowing rectangle vertex range drops the draw
/// (`VICTORY_RECTANGLE_GUARD=0` keeps the old arithmetic, which wrapped).
fn rectangle_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_RECTANGLE_GUARD").map_or(true, |v| v != "0"))
}

fn rectangle_vertex_range(v: VertexView, count: u32, first: u32) -> Option<(u64, u64, u32)> {
    rectangle_vertex_range_guarded(v, count, first, rectangle_guard())
}

fn rectangle_vertex_range_guarded(
    v: VertexView,
    count: u32,
    first: u32,
    guarded: bool,
) -> Option<(u64, u64, u32)> {
    if v.address == 0 || v.stride == 0 {
        return None;
    }
    if !guarded {
        // The old arithmetic: the title's range wraps instead of being refused.
        let stride = u64::from(v.stride);
        let from = v
            .address
            .wrapping_add(u64::from(first).wrapping_mul(stride));
        let rects = u64::from(count / 3);
        let src = rects.wrapping_mul(3).wrapping_mul(stride);
        let dst = rects.wrapping_mul(6).wrapping_mul(stride) as u32;
        return Some((from, src, dst));
    }
    let stride = u64::from(v.stride);
    let end_bytes = (u64::from(first) + u64::from(count)).checked_mul(stride)?;
    if end_bytes > u64::from(v.size) {
        return None;
    }
    let from = v.address.checked_add(u64::from(first) * stride)?;
    let rects = u64::from(count / 3);
    let src_bytes = rects.checked_mul(3)?.checked_mul(stride)?;
    from.checked_add(src_bytes)?;
    let dst_bytes = u32::try_from(rects.checked_mul(6)?.checked_mul(stride)?).ok()?;
    Some((from, src_bytes, dst_bytes))
}

fn expanded_rectangle_vertices(vertices: u32) -> Option<u32> {
    expanded_rectangle_vertices_guarded(vertices, rectangle_guard())
}

fn expanded_rectangle_vertices_guarded(vertices: u32, guarded: bool) -> Option<u32> {
    if !guarded {
        return Some((vertices / 3).wrapping_mul(6));
    }
    (vertices / 3).checked_mul(6)
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

/// Whether a list that holds the title's heaps `heaps` needs a sampler heap
/// of ours as well: the title binds none, and every root signature we make
/// indexes the sampler heap directly, which D3D12 refuses without a heap of
/// that kind bound (the debug layer's id 1419; NVIDIA removes the device).
fn wants_our_samplers(heaps: [usize; 2], on: bool) -> bool {
    heaps[1] == 0 && on
}

/// `VICTORY_SAMPLER_HEAP=0` stops binding a sampler heap of ours, the way
/// back if a title's own sampler binds come out wrong with one bound.
fn fallback_samplers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SAMPLER_HEAP").is_none_or(|v| v != "0"))
}

/// Host states of a depth target, and of a texture shaders read.
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

thread_local! {
    /// While a draw is prepared (`State::bind`, then its targets' states):
    /// the depth target the draw only tests (0: none, or one it writes),
    /// and whether a view its shaders read is of that same texture.
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    /// While a stage's tables are walked (`State::tables`): whether that
    /// stage's shader can store to buffers (`Translated::stores`).
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// While `State::early_textures` runs the texture pass: the
    /// milliseconds and the bytes that run may take, in place of the
    /// pass's own per frame.
    static PASS_BUDGET: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct StructuredBinding {
    slot: u8,
    entry: u32,
    stride: u32,
}

/// `HostShader` does not expose per-slot DXIL binding kinds and
/// `UsedResource::stride` is console metadata that can be nonzero for raw
/// views, so structured aliasing is scoped to verified fullscreen pairs whose
/// vertex shader declares a 64-byte structured SRV at root table 0, entry 0
/// while the title binds a raw buffer SRV.
fn fullscreen_structured_alias(vertex_hash: &str, pixel_hash: &str) -> Option<StructuredBinding> {
    match (vertex_hash, pixel_hash) {
        ("f7f331c8c5b3a0d8", "3bfe730f7e24ee1a") // Bink video
        | ("80cb958cde505e15", "f9bf13c6c099ccec") // Final sRGB write
        => Some(StructuredBinding {
            slot: 0,
            entry: 0,
            stride: 64,
        }),
        _ => None,
    }
}

/// The milliseconds of every frame the pass that uploads textures picked
/// by index takes before a shader that picks by index has asked for it
/// (`VICTORY_EARLY_TEXTURES_MS`, 12; 0: never, as before), and the
/// milliseconds it may take just before a draw of the title's decoration
/// bake (`VICTORY_BAKE_TEXTURES_MS`, 1500; 0: nothing special).
///
/// The pass ran only from the first world frame on, where it met some
/// 24,000 slots at once and needed 60-80 frames for them. The title bakes
/// its characters' skin and decorations into textures of its own while it
/// loads, 159-1,165 frames before that (runs 156-174), with a pixel shader
/// that picks its sources by index: whatever was not on the card then was
/// baked as nothing, once, for the run. On the console a texture is there
/// when its bytes are.
fn early_budgets() -> (u64, u64) {
    static MS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        (
            ms("VICTORY_EARLY_TEXTURES_MS", 12),
            ms("VICTORY_BAKE_TEXTURES_MS", 1500),
        )
    })
}

/// The pixel shaders of the title's decoration bake (the title:
/// `PS_BakeDecorations_*`), by the first 16 hex digits of their hash as the
/// frame listings print it. They draw into a 256x256 corner of 2048x2048
/// targets and name no texture in a table.
const BAKE_PIXEL_SHADERS: [&str; 4] = [
    "c0a7fe77b1862e10",
    "0d1575a396dfe513",
    "b7cfd8dda91ebb6b",
    "280967ad158cee0b",
];

/// Since the start: early runs of the texture pass, and those of them
/// made for a bake draw.
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether a buffer range a shader names through an unordered access view
/// is given to shaders as written (snapshot before the submission, compare
/// and write back after) only if that shader's code can store to buffers.
/// The title binds buffers its shaders only read through such views, and
/// every range declared costs a snapshot and a compare of its bytes: 3.97
/// GB a frame in 31 submissions, 0.49 s of snapshots and 0.28 s of
/// comparing (run 28). On by default; `VICTORY_WRITE_SKIP=0` declares
/// every such range, as before.
fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_SKIP").is_none_or(|v| v != "0"))
}

/// A run of indirect records clamped in one pass ([`windows`]): the host
/// copy they came from (its number), their kind and stride, the offset of
/// the first in that copy, how many, and the list's scratch buffer that
/// holds them clamped, record `i` at `i * step`.
#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    /// How far apart the records are in the copy, and in the scratch
    /// buffer: what the title walks them by ([`windows`]).
    step: u32,
    base: u64,
    records: u32,
    scratch: ID3D12Resource,
}

/// The most records clamped in one pass ([`windows`]).
const WINDOW_MOST: u64 = 64;

/// Whether indirect draws that read record after record of one argument
/// buffer are clamped a window at a time (`VICTORY_INDIRECT_WINDOW=1`; off
/// by default).
///
/// The title issues its mesh draws one record per call, each from the next
/// slot of the same argument buffer: 3,460 a frame in gameplay, and before
/// each our clamp ran as a compute dispatch of its own between two waits,
/// so the card went from drawing to compute and back for every record. In
/// the timing run of the lit scene (run 79) a shadow draw of next to
/// nothing cost 14-16 microseconds, 4,126 records a frame.
///
/// With the switch, when a draw asks for a record a short step after the
/// one drawn before, the clamp takes that record and the ones that step
/// apart after it (16, then twice as many each time, [`WINDOW_MOST`] at
/// most, never past the buffer) into
/// the list's scratch buffer in one pass, and the draws that follow read
/// their record there with no pass of ours, until one of these: a compute
/// dispatch, a direct draw, an indirect call with a count buffer or more
/// than one record, a draw whose shaders can store to buffers, a record
/// outside the window, another host list.
///
/// The step is the one the title is seen to take, not the stride its
/// command signature names: it draws one record a call, for which the
/// stride means nothing, and the title names 12 bytes (the three
/// words of a mesh dispatch) for records it keeps 32 bytes apart (2,173 of
/// a frame's 3,349 mesh records in run 87's listed frame; 283 are 16
/// apart, which ends a window). Walked by the named stride the window
/// never opened: run 87, with the switch, still clamped 5,068 records a
/// frame one at a time.
///
/// What it costs the guard against garbage records: nothing against a
/// hang (every record is bounded as before and charged to the list's
/// budget as before), but the budget is charged when a window is clamped,
/// so also for records of the window's tail that are never drawn. Those
/// hold garbage in this title (slots of all ones: run 93 spent lists'
/// budgets on them and emptied the draws after), so in a window a record
/// that asks for more than can run is left empty and charges nothing
/// (`crate::indirect::bound_ahead`); a tail record that looks real is
/// charged what it asks, a few thousand of 2^27. And arguments the title's CPU side changes are seen as
/// before, at the next submission.
fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_WINDOW").is_some_and(|v| v == "1"))
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    depth: usize,
    /// The host state the bound depth view needs ([`depth_state`]).
    depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the render target and depth handles, the
    /// viewports, the scissors, the blend factor and the stencil reference.
    topology_set: bool,
    target_handles: Vec<u64>,
    depth_handle: u64,
    targets_set: bool,
    /// The render target views and the depth view the host list has, once
    /// it has any ([`State::set_targets`]): per view the host texture (0:
    /// a null view) and what its descriptor says.
    host_targets: Option<(Vec<TargetKey>, Option<TargetKey>)>,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`windows`]).
    window: Option<Window>,
    /// What the host list has: pipeline, root signature, heaps.
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    /// The heaps the host list has, or none if it has never been given
    /// any: a list that binds nothing is not a list that bound `[0, 0]`,
    /// and the heap block (and the sampler heap of ours) must run for it.
    host_heaps: Option<[usize; 2]>,
    /// Tables made afresh in this list: (table, slot count, contents) → first
    /// dynamic slot.
    fresh_tables: std::collections::HashMap<(u64, u32, u64), u32>,
    /// The dynamic slots the list holds: title heap, first, count; given
    /// back when the list has run, is reset or released.
    dynamic: Vec<(usize, u32, u32)>,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The list ran and its dynamic slots were given back.
    tables_gone: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        for &(heap, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

impl State {
    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame of the opening cutscene
    /// asks for 12,500 tables; held until the resets they ran the 131,072
    /// slots out, and up to 4,900 draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("a list ran again after its descriptor tables were given back")
            });
        }
        for (heap, first, n) in self.dynamic.drain(..) {
            crate::descriptor::release_dynamic(heap, first, n);
        }
        self.fresh_tables.clear();
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }
}

/// An experiment on the title's scissor rectangles (`VICTORY_SCISSOR_TEST=full`):
/// they are all widened to the whole target. The title's room at
/// the start of the prologue lost its walls in five runs of seven, and in
/// those runs the player's body and the world outside were cut by one
/// rectangle around the window (run 85, frame 2046): one run with this on
/// says whether the walls are drawn and cut away by such a rectangle, or
/// not drawn at all. Off by default: a rectangle the title sets is part of
/// its picture.
fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SCISSOR_TEST").as_deref() == Ok("full"))
}

/// An experiment on mesh draws (`VICTORY_MESH_TEST`): 1 for `skip` (none
/// is issued), 2 for `scissor` (each runs its shaders but rasterises one
/// pixel), 0 otherwise. It tells a card hung by a mesh shader from one hung
/// by what the shader's triangles cover.
fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

/// `VICTORY_MESH_GRID=0`: the older mesh dispatch call, one group across
/// `y` and `z` (off means the caller's whole grid is forwarded; on by
/// default).
fn mesh_grid() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_MESH_GRID").as_deref() != Ok("0"))
}

/// `VICTORY_MESH_TEST=keep:<hex digits>[:<hex digits>]`: only the mesh
/// draws whose mesh shader's hash ends in one of the first digits (and
/// whose digit before it is one of the second, if given) are issued: a
/// search for the shaders that stop the card.
fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("VICTORY_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((
            parts.next()?.to_owned(),
            parts.next().unwrap_or("").to_owned(),
        ))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = p.name[at + 5..].as_bytes();
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15])
        && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// (no host pipeline) since the last call.
pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}
/// A root SRV's dynamic slot in a list, and the host copy it shows.
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `VICTORY_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Frames after a whole upload of a texture picked by index at which its
/// memory is looked at again, each counted from the look before.
const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

/// Looks at uploaded textures picked by index that found the memory as it
/// was, and that found it changed (uploaded again), since the start.
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether textures picked by index are looked at again after their upload
/// (`VICTORY_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK").is_none_or(|v| v != "0"))
}
/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// Times a placed texture's slot was kept as it was because its memory was
/// unmapped and it showed a texture already ([`keep_contents`]).
pub(crate) static HEAP_TEXTURES_KEPT: AtomicU64 = AtomicU64::new(0);

/// Whether a slot whose texture's memory was unmapped keeps the texture it
/// shows (`VICTORY_TEXTURE_KEEP=0` nulls it: it draws black).
fn keep_contents() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Live (crate::switches): a nulled slot samples black, a kept one whatever it last held.
    crate::switches::b(
        "VICTORY_TEXTURE_KEEP",
        *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_KEEP").is_none_or(|v| v != "0")),
    )
}
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Marks a draw of pipeline `pipeline` as not run on the host: counted by
/// whether its host pipeline can still arrive, and, with the diagnostics
/// feature, marked in the trace and counted with its reason.
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    #[cfg(feature = "diagnostics")]
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// What a render target or depth view of the host is made from: the host
/// texture, and [`crate::descriptor::RtvRecord::key`].
type TargetKey = (usize, (usize, [u32; 7]));

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB
/// (cut at their buffer's end), other views as all that is left of their
/// buffer. (They were taken as 1 MiB: 1,452 of the 2,021 mesh vertex
/// streams of a cutscene frame reach further, and what lay past the first
/// megabyte was never brought up to date - meshes without bodies, exploded
/// triangles. `VICTORY_ROOT_VIEW_MB` sets a limit again.)
fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow.left(address).min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    list.fill_scratch(address as *const u8, len)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// Whether the code of a program, whose used descriptors are `used`, names
/// the unordered view of `rule` as an entry of a table (`crate::ambient`).
fn names_view(used: &[UsedResource], rule: &crate::ambient::Rule) -> bool {
    used.iter().any(|u| {
        u.kind == ResourceType::Uav
            && u.root == RootType::Table
            && u.slot == rule.slot
            && u.table_offset_dwords == rule.offset
    })
}

/// A barrier over every unordered access: what a dispatch wrote before a
/// stand-in's clear reads, and the clear before what comes after it.
fn uav_barrier(texture: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed pointer, as `host::transition` takes it:
                // the barrier never releases it, and the list keeps the host
                // copy alive until it has run.
                pResource: unsafe { std::mem::transmute_copy(texture) },
            }),
        },
    }
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self
            .vertex
            .iter_mut()
            .skip(start as usize)
            .take(count as usize)
        {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// Render targets and depth, set on the host list at once, unless it has
    /// the same ones already: the title sets the same targets again and
    /// again, each time a host view per target and a call of the driver
    /// (25,662 times in a frame of another fork). `VICTORY_BIND_CACHE=0`
    /// sets them every time.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::host::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut views = Vec::with_capacity(handles.len());
        let mut keys = Vec::with_capacity(handles.len());
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let Some(texture) = crate::resource::host_resource(record.resource) else {
                // A typed null view (or a target without a host copy) still
                // takes its slot: left out, the next live target would move
                // into it and the pixel shader's outputs reach the wrong
                // targets.
                // The description is the title's where it gave one, else a
                // null view every device renders to (`RtvRecord::slot_desc`).
                views.push((None, Some(record.slot_desc())));
                keys.push((0, record.key()));
                continue;
            };
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            if !crate::resource::host_use_allowed(texture, false) {
                // The host does not take the render target use of this
                // texture's format: its host copy was made without that use
                // (`resource::taken_flags`), and a view of it would be
                // refused. A null view takes its slot: the pixel shader's
                // writes to it are dropped.
                once!(
                    format = unsafe { texture.GetDesc() }.Format.0,
                    "render target of a format the host cannot render to"
                );
                views.push((None, Some(record.slot_desc())));
                keys.push((0, record.key()));
                continue;
            }
            views.push((Some(texture), record.desc()));
            keys.push((texture.as_raw() as usize, record.key()));
        }
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptor::dsv_record(depth).and_then(|record| {
            let texture = crate::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::resource::depth_and_unordered(record.resource) {
                crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            Some((texture, record))
        });
        let now = (keys, dsv.as_ref().map(|(texture, record)| (texture.as_raw() as usize, record.key())));
        if crate::bindcache::same(self.host_targets.as_ref(), &now) {
            return;
        }
        self.host_targets = Some(now);
        let rtvs: Vec<_> = views.iter().map(|(texture, desc)| host.rtv(*texture, desc.as_ref())).collect();
        let dsv = dsv.map(|(texture, record)| host.dsv(texture, record.depth_desc().as_ref()));
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::draw::depth_bounds(list, min, max);
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, vertex and index buffers are set by every
    /// draw anyway. The dynamic descriptor slots and the tables made in
    /// them stay: the parts before the cut have not run yet, and they are
    /// given back when the title's list is reset.
    /// shortcut: the render target views are made again from the handles'
    /// descriptors as they are now, not as they were when the title set
    /// the targets (a descriptor rewritten in between names another view).
    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = None;
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        self.host_targets = None;
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
        }
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            // The host's own ([`Self::host_word`]): no root argument.
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// A user-data dword the host computes for a translated shader
    /// (`victory_shader::emit::USER_MEMORY_BASE`, `USER_VIEW_ADDRESS`).
    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use victory_shader::emit::{USER_GROUP_BASE, USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS};
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| if dword == 0 { m.base } else { m.count });
        }
        if kind == USER_GROUP_BASE {
            // The first group of the dispatch being bound: 0 until a split
            // dispatch sets it before each run but the first ([`State::dispatch`]).
            return 0;
        }
        if kind == USER_VIEW_BIAS {
            // The view begins on 16 bytes of the copy the address finds
            // (`root_uav`, `root_srv_table`).
            // shortcut: found again here, not taken from the list's slot.
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        // The title address of the buffer view at that dword of the table.
        let address = match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::buffer_range(&view))
                .map_or(0, |range| range.0),
            _ => 0,
        };
        if address == 0 {
            once!(
                slot,
                dword,
                "a shader takes the address of a buffer view that is not there: it reads 0"
            );
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    /// For a shader that reads title memory by address
    /// ([`crate::memory`]): its directory, fault notes and view slots,
    /// with the pages of the root views the list has set made ready.
    fn memory_ready(
        &self,
        list: &HostList,
        compute: bool,
        writes: bool,
    ) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        for root in self.roots_ref(compute) {
            match root {
                Root::View(address) => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        crate::memory::ready(list, heap, &addresses, &constants, writes)
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(
                mesh,
                "a draw or mesh dispatch with another kind of pipeline"
            );
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        if self.depth != 0 {
            // A depth texture shaders also write: sync what they wrote
            // before binding, marking depth as written only when depth writes
            // are enabled.
            list.twin_use(self.depth as This, false, self.depth_state != DEPTH_READ);
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        for (slot, (view, &v)) in views.iter_mut().zip(&self.vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                StrideInBytes: v.stride,
            };
        }
        // SAFETY: an open list; the views name live host buffers.
        unsafe {
            if count > 0 {
                l.IASetVertexBuffers(0, Some(&views[..count]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        if self.depth != 0 {
            // A view that writes neither plane keeps the texture readable:
            // the depth read state, together with the shader read states
            // if this draw's shaders read the same texture (the write state
            // put it out of the state those views need, and a move to it
            // and back for every such draw).
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let l = &list.list;
        // Textures picked by index, before anything asks for them.
        self.early_textures(list, p, compute);
        if list.take_plane_clobbered() {
            self.replay(list);
        }
        // Heaps first: a root signature with directly indexed heaps must be
        // set after them.
        if self.host_heaps != Some(self.heaps) {
            if self.heaps[0] == 0 || crate::descriptor::heap(self.heaps[0] as This).host().is_none() {
                once!("no view heap to bind");
                return false;
            }
            if self.heaps[1] != 0 && crate::descriptor::heap(self.heaps[1] as This).host().is_none() {
                once!("no host sampler heap to bind");
                return false;
            }
            if self.heaps[1] == 0 && !fallback_samplers_on() {
                once!("no sampler heap to bind");
                return false;
            }
            let mut heaps: Vec<Option<ID3D12DescriptorHeap>> = self
                .heaps
                .iter()
                .filter(|&&h| h != 0)
                .filter_map(|&h| crate::descriptor::heap(h as This).host())
                .map(|h| Some(h.heap.clone()))
                .collect();
            // A list that binds a view heap and no sampler heap — a compute
            // list that only reads views — while the signatures we make
            // index the sampler heap directly: a heap of that kind must be
            // bound before such a signature is set.
            if wants_our_samplers(self.heaps, fallback_samplers_on()) {
                match crate::descriptor::fallback_samplers() {
                    Some(h) => {
                        tracing::debug!("the title binds no sampler heap: binding ours");
                        heaps.push(Some(h.heap.clone()));
                    }
                    None => {
                        once!("no sampler heap of ours to bind");
                        return false;
                    }
                }
            }
            heaps.iter().flatten().for_each(|h| list.keep(h));
            // SAFETY: an open list and shader-visible heaps.
            unsafe { l.SetDescriptorHeaps(&heaps) };
            self.host_heaps = Some(self.heaps);
            self.host_roots = [0; 2];
        }
        // The depth target a draw only tests, for the views of it that the
        // draw's shaders read (`readable`).
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = p.pso.as_raw() as usize;
            // The title may release the pipeline before it submits the list.
            list.bind_pipeline(&p.name, &p.pso, &p.root);
        }
        let pixel_hash = (!compute)
            .then(|| {
                p.stages
                    .iter()
                    .find(|s| s.shader.stage == "pixel")
                    .map(|s| s.shader.hash.as_str())
            })
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            STORES.set(t.stores);
            let alias = (stage.shader.stage == "vertex")
                .then(|| {
                    pixel_hash.and_then(|ps| fullscreen_structured_alias(&stage.shader.hash, ps))
                })
                .flatten();
            let made = self.tables(
                list,
                &t.used,
                &t.host.root_indexed_views,
                compute,
                alias.as_slice(),
            );
            STORES.set(true);
            if made.iter().any(|m| m.1 == u32::MAX) {
                once!("a draw whose descriptor table found no free slots is dropped");
                return false;
            }
            // What the shader stores through root views and raw addresses is
            // copied back into title memory afterwards.
            let writes = (t.stores || !write_skip())
                && !crate::pipeline::builder_quarantined(self.pipeline as This);
            // Title memory by address: what the shader finds it by. A builder
            // stage's writes through it stay in the copies
            // ([`crate::pipeline::builder_map_host`]): #1198's rule for the
            // map, and the writes run 98 killed the title with. The root UAVs
            // below still copy back: that is a diff against the submission's
            // snapshot (`shadow::gpu_writes`), and a page only the map's copies
            // hold is unchanged there, so nothing is written over it.
            let memory = if t
                .host
                .root_srvs
                .contains(&victory_shader::emit::SLOT_MEMORY)
            {
                let writes = writes && !crate::pipeline::builder_map_host(self.pipeline as This);
                let Some(ready) = self.memory_ready(list, compute, writes) else {
                    once!(
                        "a shader that reads title memory by address without a view heap to hold its views: the draw is dropped"
                    );
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data {
                let mut values = vec![0u32; dwords as usize];
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    let fresh = made
                        .iter()
                        .find(|m| m.0 == u.slot && u.root == RootType::Table);
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == victory_shader::emit::SLOT_MEMORY {
                        // Ours: the directory of title memory by address.
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the directory lives as
                            // long as the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, m.directory);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, m.directory);
                                }
                            }
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_STRIDES {
                        // Ours, not the title's: the shader picks buffers
                        // from the view heap by an index it computes.
                        match self.heap_buffers(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => {
                                once!("a shader picks buffers from a view heap that is not set")
                            }
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // A table left unset is undefined: a null view.
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    // The shader adds the bytes a raw view begins early by.
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(victory_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let Some(mut gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            victory_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == victory_shader::emit::SLOT_FAULTS {
                    // Ours: where shaders note the pages they found empty.
                    if let Some(m) = &memory {
                        // SAFETY: an open list; the buffer lives as long as
                        // the process and the list holds it.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, m.faults);
                            } else {
                                l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                            }
                        }
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(
                            slot,
                            "a shader writes through a root view the title did not set"
                        );
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        // If a table/resource SRV triggered an inline depth-plane pass,
        // restore title graphics/compute bindings and re-bind cleanly once.
        if list.take_plane_clobbered() {
            self.replay(list);
            return self.bind(list, p, compute);
        }
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        true
    }

    /// Binds a root UAV: a raw unordered access view of the title buffer's
    /// host copy from `address` on, as a one-view table (a null view
    /// without an address, without a host copy shaders can write, or at an
    /// address not on 16 bytes), or as a root descriptor with
    /// `VICTORY_ROOT_SRV_DIRECT=1` (the dispatch is dropped then if there
    /// is nothing to bind). With `writes`, what the shader stored is copied
    /// back into title memory when the list has run. False if the draw
    /// must be dropped.
    /// shortcut: a root view has no size: the view, the refresh and the
    /// copy back cover all that is left of the title's buffer
    /// (`VICTORY_ROOT_UAV_MB` sets a limit for all three).
    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("VICTORY_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                once!("a root UAV with nothing to bind: the draw is dropped");
                return false;
            };
            ready(a, &s);
            // From the 16 bytes below: the shader adds the rest.
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // SAFETY: an open list; the address is in a live buffer.
            unsafe {
                if compute {
                    list.list.SetComputeRootUnorderedAccessView(param, gpu);
                } else {
                    list.list.SetGraphicsRootUnorderedAccessView(param, gpu);
                }
            }
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let (index, shadow) = match self.root_uav_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, index, 1));
                if let Some((a, s)) = &shadow {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    // Raw views start on 16 bytes: from the 16 bytes
                    // below, and the shader adds the rest to its offsets
                    // (`victory_shader::emit::USER_VIEW_BIAS`). (An address
                    // not on 16 bytes got a null view at first: the
                    // builder's counters are at such addresses.)
                    let bias = offset & 15;
                    host.raw_uav_at(
                        index,
                        &s.buffer,
                        offset - bias,
                        s.left(*a).min(limit) + bias,
                    );
                }
                if shadow.is_none() {
                    host.null_raw_uav_at(index);
                }
                self.root_uav_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                (index, shadow)
            }
        };
        if let Some((a, s)) = &shadow {
            ready(*a, s);
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// Binds a root SRV as a one-view table
    /// ([`crate::pipeline::host::bounded_root_srvs`]): a raw view of the
    /// title buffer's host copy from `address` to the copy's end, or a null
    /// view (no address, no host copy, or an address not on 16 bytes). One
    /// dynamic slot per address per list, given back with the list's
    /// others. False if no slot could be had or no view heap is bound.
    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        // A view that begins early is another view than one that does not
        // (title addresses stay below bit 63).
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let (index, shadow) = match self.root_srv_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, index, 1));
                self.root_srv_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                match &shadow {
                    Some((a, s)) => {
                        // SAFETY: a live buffer.
                        let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                        let offset = s.gpu(*a) - base;
                        if biased {
                            // From the 16 bytes below; the shader adds the
                            // rest (`victory_shader::emit::USER_VIEW_BIAS`).
                            let bias = offset & 15;
                            host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                        } else if offset.is_multiple_of(16) {
                            host.raw_at(index, &s.buffer, offset, s.left(*a));
                        } else {
                            // Raw views start on 16 bytes; one from the
                            // aligned start would shift every read.
                            once!(
                                address = format_args!("{a:#x}"),
                                "a root SRV not on 16 bytes reads zeros"
                            );
                            host.null_raw_at(index);
                        }
                    }
                    None => host.null_raw_at(index),
                }
                (index, shadow)
            }
        };
        // As for root descriptors (`root_view`): all that is left of the
        // buffer is brought up to date. The first 1 MiB only left what lay
        // past it stale (most mesh vertex streams reach further).
        if let Some((a, s)) = &shadow {
            s.refresh(list, *a, s.left(*a));
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list, as textures have: no title resource is there.
        const ONCE: usize = usize::MAX - 0x5e;
        if !list.first_use(ONCE) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran up to 51 times a
        // frame over 81,000 views and 4,100 host copies, 18 s of every
        // frame (2026-11-24, run 11).
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        let started = std::time::Instant::now();
        // Per host heap, on the heap: the host copy and range each slot's
        // buffer view reads, kept from pass to pass, and the frame its pass
        // last ran in (`crate::buffersweep`). Only the slots written since
        // the last pass are looked at again (83,000 slots a pass otherwise,
        // 0.1 s at best); all of them every `full_every` passes, because a
        // view of a released resource goes stale without a write, and a slot
        // written before its memory had a host copy is only made then.
        let frame = crate::frame::current();
        let mut sweep = host.buffer_sweep();
        if !sweep.frame_due(frame) {
            return Some(gpu);
        }
        let changed = host.buffer_changes();
        // A slot first seen before its memory had a host copy is asked again
        // for a few passes: a full scan alone would leave a dynamic index
        // reading a stale or missing view until `full_every` came round.
        // `VICTORY_HEAP_RETRY=0` goes back to waiting for the full scan.
        let retry = crate::buffersweep::retry();
        let (_, all) = sweep.pass(crate::buffersweep::full_every());
        let slots = if all {
            sweep.ranges.clear();
            sweep.pending.clear();
            host.buffer_slots()
        } else {
            let mut slots = changed;
            slots.extend(sweep.pending.keys().copied());
            slots
        };
        // What this pass could not make ready, for the next one.
        let mut again: std::collections::HashMap<u32, u8> = std::collections::HashMap::new();
        // Whether the device was there before the pass: a view that removes
        // it is named below.
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mut alive =
            !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
        for (n, &index) in slots.iter().enumerate() {
            sweep.ranges.remove(&index);
            let Some(handle) = heap.handle(index) else {
                continue;
            };
            // The budget a slot has left: a new one where this pass is the
            // one that found it, else what the pass before it stored.
            let left = sweep.pending.get(&index).copied().unwrap_or(retry);
            let Some(view) = host.mirror(index, handle, true) else {
                if left > 0 {
                    again.insert(index, left - 1);
                }
                continue;
            };
            // Every 64 views while the device lives (a cheap query).
            if alive
                && n % 64 == 63
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                alive = false;
                GONE.store(true, Ordering::Relaxed);
                let last: Vec<String> = slots[n - 63..=n]
                    .iter()
                    .filter_map(|&i| {
                        crate::descriptor::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}"))
                    })
                    .map(|s| s.chars().take(200).collect())
                    .collect();
                tracing::error!(
                    why,
                    slot = index,
                    "the host device went while buffer views of the heap were made; one of: {last:#?}"
                );
                crate::host::log_removal_context();
            }
            // The copy the slot shows (`buffer_copy`), not the one the
            // view's range alone would find.
            let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(&view) else {
                // No host copy yet: the memory may be written and read in
                // this very list, so ask again next pass, until the budget
                // runs out and the full scan takes it.
                if left > 0 {
                    again.insert(index, left - 1);
                }
                continue;
            };
            #[cfg(feature = "diagnostics")]
            if host.shows(index, &shadow.buffer) == Some(false) {
                HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
            }
            sweep.ranges.insert(index, (shadow, address, address + len));
        }
        sweep.pending = again;
        let views = sweep.ranges.len() as u32;
        // Per host copy, the span its picked views cover.
        let mut spans: std::collections::HashMap<
            usize,
            (std::sync::Arc<crate::shadow::Shadow>, u64, u64),
        > = std::collections::HashMap::new();
        for (shadow, from, to) in sweep.ranges.values() {
            let key = std::sync::Arc::as_ptr(shadow) as usize;
            let span = spans
                .entry(key)
                .or_insert_with(|| (shadow.clone(), *from, *to));
            span.1 = span.1.min(*from);
            span.2 = span.2.max(*to);
        }
        let bytes: u64 = spans.values().map(|s| s.2 - s.1).sum();
        // Memory that is not write-watched is copied whole by every refresh.
        // It was once refreshed only every 120 frames (unmapped pages read
        // one by one cost 14 GB a frame in run 11, before they were
        // remembered): streamed geometry then drew from stale buffers, and
        // the garbage ended runs (the card stopped answering 14-90 frames
        // into the opening cutscene; with every frame it ran on). The pass
        // costs some 30 ms a frame this way.
        // `VICTORY_UNWATCHED_FRAMES` sets the interval (1: every frame).
        static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let every = *EVERY.get_or_init(|| {
            std::env::var("VICTORY_UNWATCHED_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1)
        });
        for (shadow, from, to) in spans.values() {
            // By the copy's number: a copy made where one let go was
            // (`shadow::let_go_idle`) has never been refreshed. It waits out
            // the interval only while its views read inside the span its last
            // refresh covered (`crate::buffersweep`).
            if every > 1
                && shadow.unwatched()
                && !sweep.due(shadow.id(), frame, every, *from, *to)
            {
                continue;
            }
            shadow.refresh(list, *from, to - from);
        }
        sweep.end_pass();
        drop(sweep);
        self.heap_textures(list, heap, host);
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        // The first pass and every growth of the set by half.
        let seen = LOGGED.load(Ordering::Relaxed);
        if u64::from(views) > seen + seen / 2 {
            LOGGED.store(u64::from(views), Ordering::Relaxed);
            tracing::info!(
                slots = slots.len(),
                views,
                copies = spans.len(),
                mb = bytes >> 20,
                ms = started.elapsed().as_millis(),
                "buffer views of the heap made ready for shaders that pick them by index"
            );
        }
        Some(gpu)
    }

    /// Runs the pass that uploads textures picked by index
    /// ([`State::heap_textures`]) where nothing has asked for it yet
    /// ([`early_budgets`]): once a frame for a few milliseconds, at the
    /// frame's first draw with a view heap set, and with a larger budget
    /// of its own before every draw whose pixel shader is one of the
    /// title's bake programs. The uploads are recorded into `list` ahead
    /// of the draw being bound, as the world's pass records them.
    fn early_textures(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let (early_ms, bake_ms) = early_budgets();
        if compute || (early_ms == 0 && bake_ms == 0) {
            return;
        }
        let bake = bake_ms > 0
            && p.stages
                .iter()
                .any(|s| BAKE_PIXEL_SHADERS.contains(&s.shader.hash.as_str()));
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        // The milliseconds this frame's bake draws have taken so far: a
        // frame has several, and together they get `bake_ms`.
        static BAKED_MS: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        let first = FRAME.swap(frame, Ordering::Relaxed) != frame;
        if first {
            BAKED_MS.store(0, Ordering::Relaxed);
        }
        let left = bake_ms.saturating_sub(BAKED_MS.load(Ordering::Relaxed));
        let budget = if bake && left > 0 {
            // What a bake may read must be there now: up to 2 GiB.
            (left, 2 << 30)
        } else if first && early_ms > 0 {
            (early_ms, 64 << 20)
        } else {
            return;
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        let bake = bake && left > 0;
        let n = EARLY_TEXTURE_PASSES[usize::from(bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if bake && n.is_power_of_two() {
            let [slots, due, ..] = HEAP_TEXTURES_QUEUE
                .each_ref()
                .map(|c| c.load(Ordering::Relaxed));
            tracing::info!(
                frame,
                bakes = n,
                queued_at_the_last_pass = slots,
                due_at_the_last_pass = due,
                uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                "a draw of the title's decoration bake: textures picked by index are uploaded first (VICTORY_BAKE_TEXTURES_MS=0: not)"
            );
        }
        let began = std::time::Instant::now();
        PASS_BUDGET.set(Some(budget));
        self.heap_textures(list, heap, host);
        PASS_BUDGET.set(None);
        if bake {
            BAKED_MS.fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    }

    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// shortcut: uploaded once, and again only when the title rewrites the
    /// slot. A placed texture not all mapped yet is uploaded with what is
    /// there and looked at again (1, 2, 4, ... frames later, at most about
    /// a second): uploaded again when more of it is mapped. Pages mapped
    /// in place of others, and contents changed with no descriptor write,
    /// are missed (hook the page-mapping events for those).
    /// `VICTORY_HEAP_TEXTURES=0` turns it off.
    fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_TEXTURES").is_none_or(|v| v != "0")) {
            return;
        }
        let memory = crate::host::host().and_then(|h| h.video_memory());
        // Bytes of texture memory staged per frame: scaled from the card's VRAM
        // budget, and `VICTORY_TEXTURE_STAGE_MB` overrides it (raises it, so a
        // backlog of tens of thousands of textures — a scene's first minutes —
        // drains in fewer frames; or lowers it).
        static PER_FRAME_MB: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let per_frame: u64 = match *PER_FRAME_MB.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MB")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        }) {
            Some(mb) => mb << 20,
            None => staging_budget_per_frame(memory.map(|[budget, ..]| budget)),
        };
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static SPENT: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let room = crate::resource::room(memory);
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        let mut refused = 0;
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        host.texture_slots_remapped(frame);
        let (due, census) = host.texture_slots_due(frame, 4096);
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: four minutes at the opening cutscene's
        // half a frame per second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now
            .saturating_sub(PASSED.swap(now, Ordering::Relaxed))
            .max(1);
        let most = (4000 / took).clamp(16, 256);
        // What a table entry of a texture a shader samples says.
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        // Also within a time per pass: the pass runs on a thread that
        // records the frame, and making and staging 192 MB of textures took
        // 1.6-2.2 s of each of the world's first twenty frames (run 20).
        // The rest waits for the next frames, as it does past the bytes.
        static SPAN_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let span = std::time::Duration::from_millis(*SPAN_MS.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(250)
        }));
        // A backlog (a scene's first minutes: 22,000 slots due, 2-3 minutes of black
        // surfaces at 192 MB a frame) gets three times the bytes and the time until it
        // drains; `VICTORY_TEXTURE_CATCHUP=0` keeps the plain budget.
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP
            .get_or_init(|| std::env::var_os("VICTORY_TEXTURE_CATCHUP").is_none_or(|v| v != "0"));
        let (per_frame, span) = if catchup && due.len() >= 1024 {
            (per_frame * 3, span * 3)
        } else {
            (per_frame, span)
        };
        // A run made early (`State::early_textures`) has a budget of its
        // own: its bytes on top of what the frame has spent so far.
        // (Not `most`: that is the longest wait between looks, used below.)
        let (span, bytes_at_most) = match PASS_BUDGET.get() {
            Some((ms, bytes)) => (
                std::time::Duration::from_millis(ms),
                SPENT.load(Ordering::Relaxed).saturating_add(bytes),
            ),
            None => (span, per_frame),
        };
        let began = std::time::Instant::now();
        for (index, write) in due {
            if SPENT.load(Ordering::Relaxed) >= bytes_at_most || began.elapsed() > span {
                break;
            }
            // A placed texture whose memory is not mapped (not streamed in
            // yet, or streamed out again: the heap keeps tens of thousands
            // of such views) gets no host texture; looked at again soon (a
            // title writes a texture's view before its pages are mapped,
            // and the pass runs early in a frame). One of which some is
            // mapped (a texture streaming in: its smaller mips lie at the
            // start of its memory and arrive first) is uploaded with what
            // is there, and again when more is. (It was uploaded again
            // every 120 frames, changed or not; and asked by its first
            // page alone, one whose first page is the one missing counted
            // as not there.)
            // A slot uploaded whole is looked at again a few times
            // (`RECHECK_FRAMES`): the title writes a texture's view, and
            // may fill the texture's memory itself only afterwards, with
            // no descriptor write and nothing from the runtime to tell of
            // it. The host copy then kept what the memory held at the
            // first look for the rest of the run. The look reads 48 KiB;
            // only a changed texture is uploaded again.
            // `VICTORY_HEAP_RECHECK=0`: uploaded once, as before.
            if let Some((at, len, print, looks, uploaded)) = host.texture_slot_checked(index) {
                if crate::texture::fingerprint(at, len) == print {
                    HEAP_TEXTURES_RECHECKED[0].fetch_add(1, Ordering::Relaxed);
                    match RECHECK_FRAMES.get(usize::from(looks) + 1) {
                        Some(wait) => host.texture_slot_recheck(
                            index,
                            write,
                            frame + wait,
                            (at, len),
                            print,
                            looks + 1,
                            uploaded,
                        ),
                        None => host.texture_slot_done(index, write, None),
                    }
                    continue;
                }
                let n = HEAP_TEXTURES_RECHECKED[1].fetch_add(1, Ordering::Relaxed);
                if n < 200 || n.is_multiple_of(1000) {
                    tracing::warn!(
                        slot = index,
                        address = format_args!("{at:#x}"),
                        bytes = len,
                        frames_after_upload = frame.saturating_sub(uploaded),
                        looks,
                        changed = n + 1,
                        "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                    );
                }
            }
            let mut partly = None;
            if let Some(View::Placed(false, resource, desc)) =
                heap.handle(index).and_then(crate::descriptor::view)
            {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let len = crate::resource::placed_bytes(&resource).unwrap_or(1);
                let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                if mapped == 0 {
                    HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                    // Its memory went away. A slot that shows a texture
                    // already keeps it: the title streams a texture out of
                    // that address only until it streams the next one in,
                    // and a slot left as it was shows the texture that was
                    // there (stale for a moment) where a null slot draws
                    // black (`VICTORY_TEXTURE_KEEP=0` nulls it, as before).
                    if keep_contents() && host.texture_slot_has_contents(index) {
                        HEAP_TEXTURES_KEPT.fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, most);
                        continue;
                    }
                    // Streamed out: what the slot showed is let go.
                    host.texture_slot_gone(index);
                    host.texture_slot_later(index, write, frame, most);
                    continue;
                }
                if mapped < len {
                    if mapped <= host.texture_slot_mapped(index) {
                        HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, most);
                        continue;
                    }
                    if !crate::texture::committed(address as *const u8, 1) {
                        HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                    }
                    partly = Some(mapped);
                }
            }
            // A texture object the host renders to or writes holds the
            // host's own contents: nothing to bring up to date, and this
            // pass is no use of it (its host copy is made when something
            // uses it, and its slots then: `descriptor::make_waiting`).
            if let Some(View::Shader(object, _)) =
                heap.handle(index).and_then(crate::descriptor::view)
                && crate::resource::buffer_bytes(object as This).is_none()
                && !crate::resource::title_fills(object as This)
            {
                host.texture_slot_done(index, write, None);
                continue;
            }
            // From here on a host texture is made: not without room. The
            // slot stays due.
            if !room {
                refused += 1;
                if refused > 256 {
                    break;
                }
                continue;
            }
            let view = heap
                .handle(index)
                .and_then(|handle| host.mirror(index, handle, true));
            let placed = view
                .as_ref()
                .and_then(crate::descriptor::host::placed_texture);
            let resource = match placed.as_ref().or(view.as_ref()) {
                Some(&View::Shader(resource, _)) => resource as This,
                _ => {
                    host.texture_slot_done(index, write, None);
                    continue;
                }
            };
            let title_fills =
                crate::resource::host_resource(resource).is_some_and(crate::texture::title_written);
            if !title_fills {
                // a render target, a depth or unordered access texture: the host's own
                host.texture_slot_done(index, write, None);
                continue;
            }
            // A texture that has a staging buffer now is one draws read
            // through a table: they upload it, and check it at every
            // submission that samples it.
            let tabled = crate::texture::is_staged(resource);
            if let Some(view) = &view {
                readable(list, sampled.kind, view);
            }
            // A texture view the host refuses removes the device: name it.
            static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !GONE.load(Ordering::Relaxed)
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                GONE.store(true, Ordering::Relaxed);
                let text: String = format!("{view:?}").chars().take(300).collect();
                tracing::error!(
                    why,
                    slot = index,
                    "the host device went while textures picked by index were made ready; last: {text}"
                );
                crate::host::log_removal_context();
                return;
            }
            let (bytes, whole) = crate::texture::staged(resource).unwrap_or((0, true));
            // Not all of it there: looked at again next frame, and from
            // then on as a slot with nothing new (a placed texture: until
            // more of it is mapped; a texture object at an address the
            // title mapped: again in 120 frames, as before).
            let again = match partly {
                Some(mapped) => Some((frame + 1, mapped)),
                // A texture not all mapped: looked at again as a staged
                // texture's own re-read does (`partial_frames`), not in 120
                // frames as before.
                None => (!whole).then_some((frame + crate::texture::partial_frames(), 0)),
            };
            if again.is_some() {
                HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
            }
            if tabled {
                // Nothing was staged here, so nothing is counted or spent,
                // and the staging buffer stays: taking it away made the
                // next draw that reads the texture through a table stage,
                // detile and upload all of it once more.
                HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                host.texture_slot_done(index, write, again);
                continue;
            }
            // Uploaded: the list holds the staging buffer until it has run;
            // a table read, or the next look at a texture not all mapped
            // yet, makes it anew. (Kept for those until now: every partly
            // streamed texture of the heap held a staging buffer of its full
            // size for good.)
            let memory = if again.is_none() && heap_recheck() {
                crate::texture::staged_memory(resource)
            } else {
                None
            };
            crate::texture::unstage(resource);
            SPENT.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
            match memory {
                // Whole and uploaded: looked at again (see above).
                Some((at, len)) => {
                    let print = crate::texture::fingerprint(at, len);
                    host.texture_slot_recheck(
                        index,
                        write,
                        frame + RECHECK_FRAMES[0],
                        (at, len),
                        print,
                        0,
                        frame,
                    );
                }
                None => host.texture_slot_done(index, write, again),
            }
        }
    }

    /// For the frame listing, after a dropped dispatch's line: its group
    /// counts and the compute root arguments it was recorded with (root
    /// constants as hex dwords, root views with the title buffer they
    /// point into). A program that does not translate has no bindings to
    /// list; these say which memory it would have read and written (the
    /// ray-tracing structure builder passes its buffers this way).
    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    /// The most groups the host runs in one dimension of a dispatch. The
    /// console's own limit is higher (a builder pass asks for 77,500 in
    /// `x`): a dispatch beyond it is run as several ([`State::dispatch`]).
    const MOST_GROUPS: u32 = 65_535;

    /// The most groups a mesh dispatch runs in all (2^22, the host's grid
    /// limit): a piece of a split mesh dispatch is cut to it as well.
    const MOST_MESH_GROUPS: u32 = 1 << 22;

    /// Where a compute or mesh stage's group-ID base is bound, if the stage
    /// reads its group id: its user-data root parameter and the dword in it
    /// (`victory_shader::emit::USER_GROUP_BASE`).
    fn group_base_slot(p: &HostPipeline) -> Option<(u32, u32)> {
        for stage in &p.stages {
            let Some((param, _)) = stage.user_data else {
                continue;
            };
            let t = stage.translated();
            if let Some(u) = t
                .host
                .user_data
                .iter()
                .find(|u| u.root == RootType::Other(victory_shader::emit::USER_GROUP_BASE))
            {
                return Some((param, u.index));
            }
        }
        None
    }

    /// Sets the group-ID base of the next run of a split dispatch.
    fn set_group_base(list: &HostList, compute: bool, (param, dword): (u32, u32), base: u32) {
        // SAFETY: an open list whose root signature has that parameter.
        unsafe {
            if compute {
                list.list.SetComputeRoot32BitConstant(param, base, dword);
            } else {
                list.list.SetGraphicsRoot32BitConstant(param, base, dword);
            }
        }
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        // It may write the arguments a window holds clamped.
        self.window = None;
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_skipped(self.pipeline as This);
            return;
        }
        // The ray-tracing structure builder's stages run all or none.
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_design_why(
                    "a stage of the ray-tracing structure builder (VICTORY_BVH_BUILDER is not 1)"
                        .to_owned(),
                );
                self.list_dropped_roots([x, y, z]);
            }
            #[cfg(not(feature = "diagnostics"))]
            crate::pipeline::note_design();
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
            #[cfg(feature = "diagnostics")]
            self.list_dropped_roots([x, y, z]);
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            return;
        }
        if !self.bind(list, p, true) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            // A stage of the structure builder that cannot be bound is a
            // dropped stage: the others stop too.
            if crate::pipeline::is_builder(self.pipeline as This) {
                crate::pipeline::builder_off("one of its dispatches could not be bound");
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        // The host runs at most 65,535 groups in a dimension. A console
        // dispatch over that runs as several, each with the group id where it
        // starts (`VICTORY_GROUP_SPLIT=0`: the first 65,535 groups only).
        match Self::group_base_slot(p) {
            Some(slot) if x > Self::MOST_GROUPS => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::info!(
                        x,
                        runs = x.div_ceil(Self::MOST_GROUPS),
                        "a dispatch asks for more groups in x than the host runs: it runs in pieces, each told where its \
                         groups start (VICTORY_GROUP_SPLIT=0: the first 65,535 groups only)"
                    );
                });
                let mut at = 0;
                while at < x {
                    let n = (x - at).min(Self::MOST_GROUPS);
                    if at != 0 {
                        Self::set_group_base(list, true, slot, at);
                    }
                    // SAFETY: an open list with everything set.
                    unsafe { list.list.Dispatch(n, y, z) };
                    at += n;
                }
            }
            _ => {
                if x > Self::MOST_GROUPS {
                    once!(
                        x,
                        "a dispatch asks for more groups in x than the host runs and its program reads no group id: \
                         the groups after 65,535 do not run"
                    );
                }
                // No group id to move: the groups after the host's limit
                // cannot be run at all (a count over it the host does not
                // give its driver either).
                // SAFETY: an open list with everything set.
                unsafe { list.list.Dispatch(x.min(Self::MOST_GROUPS), y, z) };
            }
        }
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
        self.stand_ins(list, None);
        self.reflection(list, p);
        if self.light_floor(list, p) {
            // Our pass set its own pipeline, root signature and heap: the next bind sets the title's again.
            self.host_pipeline = 0;
            self.host_roots = [0; 2];
            self.host_heaps = None;
        }
    }

    /// The stand-ins `VICTORY_AMBIENT` sets for the bound program
    /// (`crate::ambient`), after a dispatch ran: how many views were
    /// cleared, `None` if no rule names the program. `indirect`: the
    /// records a dispatch may run at most, and its count's buffer and
    /// offset if it has one ([`Self::stand_ins_of`]).
    fn stand_ins(
        &mut self,
        list: &HostList,
        indirect: Option<(u32, Option<(ID3D12Resource, u64)>)>,
    ) -> Option<usize> {
        let rules = crate::ambient::rules();
        if rules.is_empty() {
            return None;
        }
        let p = crate::pipeline::host_pipeline(self.pipeline as This)?;
        let stage = p.stages.first()?;
        let program = stage.shader.hash.as_str();
        if !rules.iter().any(|r| r.program == program) {
            return None;
        }
        let used = &stage.translated().used;
        // The stand-ins follow shader work: no records at most (or none
        // known to run), nothing to stand in for.
        let (count, offset) = match indirect {
            None => return Some(self.stand_in(list, program, used)),
            Some((0, _)) => return None,
            Some((_, Some((count, offset)))) => (count, offset),
            Some((_, None)) => return Some(self.stand_in(list, program, used)),
        };
        let Some(word) = crate::host::host().and_then(|h| h.predicate_word()) else {
            once!("ambient stand-in: no word to gate them on, so an indirect dispatch gets no clears");
            for _ in rules.iter().filter(|r| r.program == program) {
                crate::ambient::note(false);
            }
            return None;
        };
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_COPY_DEST as COPY_DEST;
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_PREDICATION as PREDICATION;
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
        const COMMON: u32 = 0;
        // SAFETY: an open list; a count read where the execution reads it,
        // into the low half of our own word (zero elsewhere). The copy
        // promotes the word to `COPY_DEST`, where the barrier finds it.
        unsafe {
            list.list.CopyBufferRegion(word, 0, &count, offset, 4);
            list.list.ResourceBarrier(&[crate::host::transition(
                word,
                COPY_DEST,
                PREDICATION,
            )]);
            list.list.SetPredication(word, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
        }
        let filled = self.stand_in(list, program, used);
        // SAFETY: as above.
        unsafe {
            list.list.SetPredication(
                None::<&ID3D12Resource>,
                0,
                D3D12_PREDICATION_OP_EQUAL_ZERO,
            );
            list.list.ResourceBarrier(&[crate::host::transition(
                word,
                PREDICATION,
                State(COMMON as i32),
            )]);
        }
        Some(filled)
    }

    /// The views of the rules that name `program`, whose code uses `used`,
    /// cleared to their values ([`Self::clear_uav`]): how many were.
    fn stand_in(&mut self, list: &HostList, program: &str, used: &[UsedResource]) -> usize {
        let mut filled = 0;
        for rule in crate::ambient::rules()
            .iter()
            .filter(|r| r.program == program)
        {
            let first = !rule.seen.swap(true, Ordering::Relaxed);
            let uavs = || -> Vec<String> {
                used.iter()
                    .filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Table)
                    .map(|u| format!("s{}+{}", u.slot, u.table_offset_dwords))
                    .collect()
            };
            if !names_view(used, rule) {
                if first {
                    tracing::warn!(program, view = %rule.view(), views = ?uavs(), "ambient stand-in: the program has no such unordered view in a table");
                }
                crate::ambient::note(false);
                continue;
            }
            let view = self.roots_ref(true).get(usize::from(rule.slot)).and_then(
                |root| match root {
                    Root::Table(table) => {
                        crate::descriptor::view(*table + u64::from(rule.offset) * 4)
                    }
                    _ => None,
                },
            );
            let Some(View::Unordered(resource, _, words)) = view else {
                if first {
                    tracing::warn!(program, view = %rule.view(), "ambient stand-in: not an unordered view of a resource there");
                }
                crate::ambient::note(false);
                continue;
            };
            let resource = resource as This;
            // The formats of the view and of the texture it views, the
            // title's; the size, for the log.
            let own = crate::resource::host_resource(resource).map(|texture| {
                // SAFETY: a live resource, read for its description only.
                unsafe { texture.GetDesc() }
            });
            let Some(own) = own else {
                if first {
                    tracing::warn!(program, view = %rule.view(), "ambient stand-in: no host texture there");
                }
                crate::ambient::note(false);
                continue;
            };
            if crate::resource::buffer_bytes(resource).is_some() {
                if first {
                    tracing::warn!(program, view = %rule.view(), "ambient stand-in: a view of a buffer, which a clear of a view does not take");
                }
                crate::ambient::note(false);
                continue;
            }
            if own.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 {
                if first {
                    tracing::warn!(program, view = %rule.view(), "ambient stand-in: a depth texture, whose views go to a colour twin and take no clear of this kind");
                }
                crate::ambient::note(false);
                continue;
            }
            let (width, height, own) = (own.Width, own.Height, own.Format.0 as u32);
            // The title's own description of the view, unless it leaves the
            // format to the resource: then the resource's, whole.
            let words = words.filter(|words| words[0] != 0);
            let format = words.map_or(own, |words| words[0]);
            let fill = crate::ambient::fill(rule.value, format, own);
            if first {
                tracing::info!(
                    program,
                    view = %rule.view(),
                    views = ?uavs(),
                    format,
                    texture_format = own,
                    size = ?(width, height),
                    value = ?rule.value,
                    ?fill,
                    "ambient stand-in: first dispatch of the program"
                );
            }
            let recorded = match fill {
                Ok(crate::ambient::Fill::Float(rgba)) => {
                    self.clear_uav(list, resource, words, rgba.map(f32::to_bits), true)
                }
                Ok(crate::ambient::Fill::Uint(fill)) => {
                    self.clear_uav(list, resource, words, fill, false)
                }
                Err(why) => {
                    if first {
                        tracing::warn!(program, view = %rule.view(), format, texture_format = own, "ambient stand-in: {why}");
                    }
                    false
                }
            };
            crate::ambient::note(recorded);
            filled += usize::from(recorded);
        }
        filled
    }

    /// After a dispatch of `p`: the stand-in for ray-traced reflections
    /// ([`crate::reflection`]), if it is on and `p` is its program.
    /// What it records binds nothing (a transition and a render target
    /// clear): the title's bindings stay.
    fn reflection(&self, list: &HostList, p: &HostPipeline) {
        let Some(stage) = p.stages.first().filter(|_| p.compute) else {
            return;
        };
        let Some(mode) = crate::reflection::pass(&stage.shader.hash) else {
            return;
        };
        // Every table UAV the program names is kept, a null one included:
        // which image is the program's radiance image is decided by its own
        // table (see `reflection::target_of`), never by what happens to be bound.
        let uavs: Vec<crate::reflection::Uav> = stage
            .translated()
            .used
            .iter()
            .filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Table)
            .map(|u| {
                let view = match self.roots_ref(true).get(usize::from(u.slot)) {
                    Some(Root::Table(table)) => {
                        crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4)
                    }
                    _ => None,
                };
                match view {
                    Some(View::Unordered(r, _, desc)) => (u.slot, u.table_offset_dwords, r, desc),
                    _ => (u.slot, u.table_offset_dwords, 0, None),
                }
            })
            .collect();
        crate::reflection::after(list, mode, &stage.shader.hash, &uavs);
    }

    /// After a dispatch of `p`: the least indirect light, where
    /// `VICTORY_AMBIENT_FLOOR` names that program's ([`crate::ambient_floor`]).
    /// The view is the one the console shader addresses (root parameter and
    /// table offset), so it holds across translator versions.
    fn light_floor(&self, list: &HostList, p: &HostPipeline) -> bool {
        if !crate::ambient_floor::enabled() {
            return false;
        }
        let Some(stage) = p.stages.first() else {
            return false;
        };
        let rules = crate::ambient_floor::rules(&stage.shader.hash);
        if rules.is_empty() {
            return false;
        }
        let Some(translated) = stage.shader.translated() else {
            return false;
        };
        let view = |kind: ResourceType, (slot, offset): (u8, u32)| {
            translated.used.iter().find(|u| {
                u.kind == kind
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            crate::descriptor::view(table + u64::from(offset) * 4)
        };
        let mut ran = false;
        for rule in rules {
            let light = match view(ResourceType::Uav, rule.light) {
                Some(View::Unordered(r, ..)) => r,
                _ => 0,
            };
            if light == 0 {
                once!("a light the console shader does not address");
                continue;
            }
            let depth = match rule.depth.map(|d| view(ResourceType::Srv, d)) {
                Some(Some(View::Shader(r, _))) => Some(r as This),
                _ => None,
            };
            ran |= crate::ambient_floor::raise(list, rule, light as This, depth);
        }
        ran
    }

    /// Clears the whole of the texture `resource`, seen as the view the
    /// title described (`words`, as [`Host::uav`] takes them; `None`: its
    /// own format, whole), to `values` (a float clear's bits when `floats`):
    /// our heap set for the clear and the title's again after it, the
    /// texture in the state it was found in. False if the clear could not be
    /// recorded.
    fn clear_uav(
        &mut self,
        list: &HostList,
        resource: This,
        words: Option<[u32; 10]>,
        values: [u32; 4],
        floats: bool,
    ) -> bool {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_UNORDERED_ACCESS as UNORDERED;
        let Some(host) = crate::host::host() else {
            return false;
        };
        let (Some(heap), Some((cpu, gpu))) = (host.uav_heap(), host.uav(resource, words)) else {
            return false;
        };
        let Some(texture) = crate::resource::host_resource(resource) else {
            return false;
        };
        // The state the list saw it in, also when that is the unordered one
        // already (a dispatch reads and writes it): the clear is still owed
        // a barrier of its own then.
        let Some(before) = list.state_of(resource) else {
            return false;
        };
        let unordered = before == UNORDERED.0 as u32;
        // SAFETY: an open list, a live texture, our own descriptors and the
        // heap they are in (the title's heaps are set again by the next
        // bind, which this drops from the cache).
        unsafe {
            list.list.SetDescriptorHeaps(&[Some(heap)]);
            if !unordered {
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    D3D12_RESOURCE_STATES(before as i32),
                    UNORDERED,
                )]);
            }
            // What the dispatch wrote is ordered before the clear, and the
            // clear before what reads the view next.
            list.list.ResourceBarrier(&[uav_barrier(texture)]);
            if floats {
                list.list.ClearUnorderedAccessViewFloat(
                    gpu,
                    cpu,
                    texture,
                    &values.map(f32::from_bits),
                    &[],
                );
            } else {
                list.list
                    .ClearUnorderedAccessViewUint(gpu, cpu, texture, &values, &[]);
            }
            list.list.ResourceBarrier(&[uav_barrier(texture)]);
            if !unordered {
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    UNORDERED,
                    D3D12_RESOURCE_STATES(before as i32),
                )]);
            }
        }
        if !unordered {
            // The tracked state follows the barriers above: back to `before`.
            let _ = list.transition(resource, before);
        }
        // The title's own heaps are not those of the clear: the next bind
        // sets them again, and `None` is what a list that has bound
        // nothing holds.
        self.host_heaps = None;
        true
    }

    /// What a draw or dispatch runs, for the device-removed report    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (with the view at that heap index, if one is), the host addresses
    /// of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        // What it draws into (a dispatch writes through its views below).
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(
                text,
                " targets {targets:?} depth {:?} state [{}]",
                described(self.depth),
                p.state
            );
            // The title's scissor and viewport at this draw (the first of
            // each): the title draws what it sees through a
            // portal (a window, from inside a room) under the portal's
            // rectangle, and the listing did not say which draws those are.
            if let Some(r) = self.scissors.first() {
                let _ = write!(
                    text,
                    " scissor [{} {} {} {}]",
                    r.left, r.top, r.right, r.bottom
                );
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage
                .root_cbvs
                .iter()
                .chain(&stage.root_srvs)
                .chain(&stage.root_uavs)
            {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        text
    }

    /// A mesh draw under `VICTORY_MESH_TEST=scissor` rasterises one pixel:
    /// the scissors before it (`on`), the title's after it.
    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    /// Runs a mesh dispatch with the caller's three-dimensional group grid.
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        self.window = None;
        // The older call took one group across `y` and `z`.
        let (x, y, z) = if mesh_grid() { (x, y, z) } else { (x, 1, 1) };
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x * y * z, 0);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}x{y}x{z}"), false));
                    self.mesh_scissor(list, true);
                    match crate::pipeline::host_pipeline(self.pipeline as This).and_then(Self::group_base_slot) {
                        Some(slot) if x > Self::MOST_GROUPS => {
                            static ONCE: std::sync::Once = std::sync::Once::new();
                            ONCE.call_once(|| {
                                tracing::info!(
                                    x,
                                    runs = x.div_ceil(Self::MOST_GROUPS),
                                    "a mesh dispatch asks for more groups in x than the host runs: it runs in pieces, \
                                     each told where its groups start (VICTORY_GROUP_SPLIT=0: the first 65,535 groups only)"
                                );
                            });
                            // A mesh dispatch's pieces are bounded by the
                            // grid's own limit too, in all three dimensions.
                            let grid = y.saturating_mul(z).max(1);
                            let piece = (Self::MOST_MESH_GROUPS / grid).clamp(1, Self::MOST_GROUPS);
                            let mut at = 0;
                            while at < x {
                                let n = (x - at).min(piece);
                                if at != 0 {
                                    Self::set_group_base(list, false, slot, at);
                                }
                                // SAFETY: an open list with everything set.
                                unsafe { l6.DispatchMesh(n, y, z) };
                                at += n;
                            }
                        }
                        _ => {
                            if x > Self::MOST_GROUPS {
                                once!(
                                    x,
                                    "a mesh dispatch asks for more groups in x than the host runs and its program \
                                     reads no group id: the groups after 65,535 do not run"
                                );
                            }
                            // As above: no group id to move, and a count
                            // over the limit the host does not hand its
                            // driver either.
                            // SAFETY: an open list with everything set.
                            unsafe { l6.DispatchMesh(x.min(Self::MOST_GROUPS), y, z) };
                        }
                    }
                    self.mesh_scissor(list, false);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            }
        }
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Returns those tables' new byte offsets by root
    /// parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        compute: bool,
        aliases: &[StructuredBinding],
    ) -> Vec<(u8, u32)> {
        let tables = |u: &&UsedResource| u.root == RootType::Table;
        let mut fresh: Vec<u8> = used
            .iter()
            .filter(tables)
            .filter(|u| u.kind == ResourceType::Cbv)
            .map(|u| u.slot)
            .chain(aliases.iter().map(|a| a.slot))
            .collect();
        // Tables reaching past a host heap smaller than the title's (the
        // host's limit, 1,000,000 views on NVIDIA) are made afresh too: the
        // title's slots there have no host slot of their own.
        for u in used.iter().filter(tables) {
            if fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                fresh.push(u.slot);
            }
        }
        fresh.sort_unstable();
        fresh.dedup();
        for u in used.iter().filter(tables) {
            if !fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        let mut made = Vec::new();
        for slot in fresh {
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            let entries: Vec<&UsedResource> = used
                .iter()
                .filter(tables)
                .filter(|u| u.slot == slot)
                .collect();
            let table_aliases: Vec<StructuredBinding> =
                aliases.iter().copied().filter(|a| a.slot == slot).collect();
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            let mut selected: Vec<u32> = indexed
                .iter()
                .filter(|index| index.table_slot == slot)
                .map(|index| {
                    let constant = self.user_value(
                        compute,
                        index.constant_slot,
                        RootType::Constants,
                        index.constant_dword,
                    );
                    // The shader adds i32 byte offsets and shifts by 5.
                    index.byte_offset.wrapping_add(constant << index.shift) / 32
                })
                .collect();
            selected.sort_unstable();
            selected.dedup();
            let n = entries
                .iter()
                .map(|u| u.table_offset_dwords / 8)
                .chain(selected.last().copied())
                .chain(table_aliases.iter().map(|a| a.entry))
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            // The table and what its entries hold (record words and alias
            // shapes).
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    unsafe { (at as *const [u32; 3]).read_unaligned() }.hash(&mut h);
                    // A placed UAV can gain a host texture without its title
                    // descriptor changing. Keep old slots intact for earlier
                    // draws and allocate a new table only when that changes.
                    crate::descriptor::view(at)
                        .and_then(|view| crate::descriptor::host::placed_uav_identity(&view))
                        .hash(&mut h);
                }
                table_aliases.hash(&mut h);
                (table, n, h.finish())
            };
            if let Some(&first) = self.fresh_tables.get(&key) {
                for alias in &table_aliases {
                    if !entries.iter().any(|u| {
                        u.kind != ResourceType::Cbv && u.table_offset_dwords / 8 == alias.entry
                    }) {
                        let handle = table + u64::from(alias.entry) * 32;
                        if let Some(view) = crate::descriptor::view(handle)
                            && let Some(kind) = view_kind(&view)
                        {
                            readable(list, kind, &view);
                        }
                    }
                }
                for u in entries.iter().filter(|u| u.kind != ResourceType::Cbv) {
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptor::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for k in selected {
                    if entries.iter().any(|u| u.table_offset_dwords / 8 == k) {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            self.fresh_tables.insert(key, first);
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, first, n));
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                let used = entries.iter().find(|u| u.table_offset_dwords / 8 == k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let Some((gpu, to)) = list.scratch(u64::from(size)) else {
                            host.make_null(index);
                            continue;
                        };
                        list.defer_fill(to, address as *const u8, size as usize);
                        host.constant_at(index, gpu, size);
                    }
                    Some(view) => {
                        let aliased = table_aliases.iter().any(|a| {
                            a.entry == k && host.make_structured_at(index, &view, a.stride)
                        });
                        if !aliased {
                            host.make_at(index, &view);
                        }
                        let kind = used.map(|u| u.kind).or_else(|| {
                            selected
                                .binary_search(&k)
                                .ok()
                                .and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror(title + k, table + u64::from(k) * 32, true);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if used.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            made.push((slot, first * 32));
        }
        made
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        // A pipeline with the rectangle geometry stage draws the title's
        // vertices as they are; any other gets its rectangles expanded.
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        // Rectangles are expanded into six vertices each, a count that can
        // overflow as the title's vertex count is not checked beforehand.
        let count = if rects {
            expanded_rectangle_vertices(vertices)
        } else {
            Some(vertices)
        };
        let ran = ready && (!rects || (count.is_some() && self.rectangles(list, vertices, first)));
        if let Some(count) = count.filter(|_| ran) {
            // Rectangles were expanded into their own vertex buffers.
            let from = if rects { 0 } else { first };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }

    /// Whether the bound pipeline completes rectangles itself, in the
    /// geometry stage rectangle pipelines get (`HostPipeline::rects`).
    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let n = (count / 3) as usize;
        // Where the `POSITION` element is read from, if the layout has one:
        // every slot of a rectangle takes its right angle from there, so that
        // the vertices it completes line up between buffers. Found once per
        // draw, read at submission like the vertices themselves.
        let position = p.inputs.iter().find(|e| e.position && !e.per_instance);
        let position_src = position.and_then(|e| {
            let v = self.vertex[e.slot as usize];
            rectangle_vertex_range(v, count, first)
                .filter(|&(from, src_bytes, _)| crate::shadow::find(from, src_bytes).is_some())
                .map(|(from, _, _)| (from, v.stride, e.offset, e.format))
        });
        // A `POSITION` range the checks refuse rejects the whole draw here,
        // before any slot registers a callback that would read it: the slots'
        // callbacks run at submission even when the draw is dropped after
        // them.
        if position.is_some() && position_src.is_none() {
            once!("rectangle vertices outside their buffer");
            return false;
        }
        let corners = std::sync::Arc::new(std::sync::OnceLock::new());
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let Some((from, src_bytes, dst_bytes)) = rectangle_vertex_range(v, count, first)
                .filter(|&(from, src_bytes, _)| crate::shadow::find(from, src_bytes).is_some())
            else {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            };
            let Some((gpu, to)) = list.scratch(u64::from(dst_bytes)) else {
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, v.stride as usize);
            let corners = corners.clone();
            list.defer_host(move || {
                // The vertices are read in place, at submission: no change
                // of mapping under the read (see
                // `crate::watch::title_memory_reading`).
                let _mapped = crate::watch::title_memory_reading();
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        std::slice::from_raw_parts(from as *const u8, src_bytes as usize),
                        std::slice::from_raw_parts_mut(to as *mut u8, dst_bytes as usize),
                    )
                };
                // Once for the whole draw, whichever slot runs first: the
                // `POSITION` buffer's bytes are the rectangle's corners.
                let corners = corners.get_or_init(|| match position_src {
                    // SAFETY: the `POSITION` element's buffer was checked
                    // the same way as this slot's, and it is read under the
                    // same guard.
                    Some((at, pstride, offset, format)) => unsafe {
                        let src = std::slice::from_raw_parts(at as *const u8, n * 3 * pstride as usize);
                        crate::primitive::right_corners(
                            src,
                            pstride as usize,
                            n,
                            (offset, format),
                        )
                    },
                    None => Vec::new(),
                });
                crate::primitive::expand_rects(src, dst, stride, &elements, corners);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: dst_bytes,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        // Indexed rectangles are drawn by the rectangle geometry stage
        // alone (nothing expands them on the CPU).
        let lost = self.topology == RECTANGLES && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }
}

impl State {
    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host.
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_skipped(self.pipeline as This);
            return false;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        // Indirect rectangles (the title's tiled lighting, 256
        // indirect draws a frame, makes their corners from the vertex
        // number) are completed by the pipeline's geometry stage: the
        // arguments stay the title's, three vertices a rectangle. They
        // cannot be expanded on the CPU, which does not know their count.
        // shortcut: in a pipeline without that stage, a rectangle with no
        // per-vertex inputs is drawn as its first triangle (three vertices,
        // half of it), and one with vertex buffers is not drawn.
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            why(&format!(
                "pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why("arguments outside title buffers");
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why("count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why("no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why("mesh draws are switched off (VICTORY_MESH_TEST=skip)");
            return false;
        }
        // A window of records clamped in one pass (`windows`): for single
        // records without a count buffer, drawn by shaders that store to no
        // buffer. Anything else ends the window: it uses the scratch buffer
        // or may write the arguments.
        const WINDOW_USE: usize = usize::MAX - 0x77;
        let offset = args.offset(arguments);
        let windowed = windows()
            && kind != DISPATCH
            && max == 1
            && count.is_none()
            && !p.stages.iter().any(|stage| stage.translated().stores);
        // Another host list (a new one after a cut, or this one after its
        // reset) has another scratch buffer, or this one's with old bytes.
        if !windowed || list.first_use(WINDOW_USE) {
            self.window = None;
        }
        let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
        let hit = self
            .window
            .as_ref()
            .filter(|w| same(w) && offset >= w.base)
            .filter(|w| {
                (offset - w.base) % u64::from(w.step) == 0
                    && (offset - w.base) / u64::from(w.step) < u64::from(w.records)
            })
            .map(|w| (w.scratch.clone(), offset - w.base));
        let (bounded, at) = match hit {
            Some(hit) => hit,
            None => {
                // A record a short step after the last window's last one
                // (the same step as that window's, if it had one): the
                // title walks the buffer, so the ones that step apart
                // after it are clamped with it.
                let (step, records) = match &self.window {
                    Some(w) if windowed && same(w) => {
                        let last = w.base + u64::from(w.records - 1) * u64::from(w.step);
                        let step = offset.saturating_sub(last);
                        let walks = step >= u64::from(stride)
                            && step <= 256
                            && step % 4 == 0
                            && (w.records == 1 || step == u64::from(w.step));
                        if walks {
                            let fit = (args.left(arguments) / step).min(WINDOW_MOST) as u32;
                            (
                                step as u32,
                                (if w.records == 1 { 16 } else { w.records * 2 })
                                    .min(fit)
                                    .max(1),
                            )
                        } else {
                            (stride, max)
                        }
                    }
                    _ => (stride, max),
                };
                args.refresh(
                    list,
                    arguments,
                    len.max(u64::from(step) * u64::from(records)),
                );
                if let Some((s, at)) = &counter {
                    s.refresh(list, *at, 4);
                }
                // Bounded first: the clamp binds a compute pipeline of its
                // own and copies clamped active records into a scratch
                // buffer at offset 0.
                let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                // More than the record asked for: a window clamped ahead,
                // whose unfilled slots must not spend the list's budget.
                let bounded = if windowed && records > 1 {
                    crate::indirect::bound_ahead(
                        list,
                        (&args.buffer, offset),
                        (kind, step, records),
                    )
                } else {
                    crate::indirect::bound(
                        list,
                        (&args.buffer, offset),
                        count_arg,
                        (kind, step, records),
                    )
                };
                let Some(bounded) = bounded else {
                    self.window = None;
                    why("not bounded (no clamp, or arguments not 4-byte aligned)");
                    return false;
                };
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
                self.window = windowed.then(|| Window {
                    copy: args.id(),
                    kind,
                    stride,
                    step,
                    base: offset,
                    records,
                    scratch: bounded.clone(),
                });
                (bounded, 0)
            }
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => self.bind(list, p, true),
            _ => false,
        };
        if !ran {
            crate::indirect::done(list, &bounded);
            why("prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(
                format!("indirect kind {kind} max {max} at {arguments:#x}"),
                kind == DISPATCH,
            )
        });
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &bounded,
                at,
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        crate::indirect::done(list, &bounded);
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
            // The stand-in follows the records that ran: none at most, none
            // with a count of zero.
            let count = counter
                .as_ref()
                .map(|(s, at)| (s.buffer.clone(), s.offset(*at)));
            self.stand_ins(list, Some((max, count)));
            self.reflection(list, p);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once.
fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() || !write_skip() {
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source));
            if !aliased
                && let Some(t) = crate::resource::host_resource(*resource as This)
                && crate::texture::title_written(t)
            {
                crate::texture::upload(list, *resource as This, t);
            }
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(*resource as This, false, false);
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if let Some(t) = crate::resource::host_resource(*resource as This) {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, desc), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            // Read by the title's CPU code: copied back into title memory.
            list.texture_written(*resource as This, desc.as_ref());
            // Unordered access to a depth texture is its colour twin's.
            if !list.twin_use(*resource as This, true, true) {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!(
                "{}x{} f{} flags {:#x} of {r:?}",
                d.Width, d.Height, d.Format.0, d.Flags.0
            )
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("STENCIL REF {reference:#x}")
    });
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

/// Per-frame texture staging byte budget scaled from the adapter's local VRAM
/// budget (`None` keeps the 192 MiB 12 GiB baseline).
fn staging_budget_per_frame(vram_budget: Option<u64>) -> u64 {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    match vram_budget {
        Some(budget) if budget <= 6 * GIB => 96 * MIB,
        Some(budget) if budget >= 20 * GIB => 384 * MIB,
        Some(budget) if budget > 12 * GIB => 256 * MIB,
        _ => 192 * MIB,
    }
}

#[cfg(test)]
mod tests {
    use super::names_view;
    use victory_shader::{ResourceType, RootType, UsedResource};

    /// A view a program names, as a table entry (the stand-in compares
    /// root parameter and offset in dwords, `crate::ambient`).
    fn used(kind: ResourceType, root: RootType, slot: u8, offset: u32) -> UsedResource {
        UsedResource {
            kind,
            root,
            table_offset_dwords: offset,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 4,
            slot,
        }
    }

    #[test]
    fn the_stand_in_names_a_view_of_a_table() {
        let rule = crate::ambient::parse("0123456789abcdef:s1+176:0.5,0.5,0.5")
            .unwrap()
            .remove(0);
        assert!(names_view(
            &[used(ResourceType::Uav, RootType::Table, 1, 176)],
            &rule
        ));
        // Another root parameter, offset or kind, or not a table: no.
        assert!(!names_view(
            &[used(ResourceType::Uav, RootType::Table, 2, 176)],
            &rule
        ));
        assert!(!names_view(
            &[used(ResourceType::Uav, RootType::Table, 1, 168)],
            &rule
        ));
        assert!(!names_view(
            &[used(ResourceType::Uav, RootType::Table, 1, 180)],
            &rule
        ));
        assert!(!names_view(
            &[used(ResourceType::Srv, RootType::Table, 1, 176)],
            &rule
        ));
        assert!(!names_view(
            &[used(ResourceType::Uav, RootType::Descriptor, 1, 176)],
            &rule
        ));
        assert!(!names_view(&[], &rule));
    }

    use super::{
        VertexView, expanded_rectangle_vertices_guarded, rectangle_vertex_range_guarded,
        staging_budget_per_frame, wants_our_samplers,
    };

    /// What a rectangle draw asks of the tile's vertex buffer with the guard
    /// on, which is what the environment gives unless `VICTORY_RECTANGLE_GUARD`
    /// is `0`; the tests do not read the switch.
    fn range(v: VertexView, count: u32, first: u32) -> Option<(u64, u64, u32)> {
        rectangle_vertex_range_guarded(v, count, first, true)
    }

    fn expanded(vertices: u32) -> Option<u32> {
        expanded_rectangle_vertices_guarded(vertices, true)
    }

    const RECTANGLE_VIEW: VertexView = VertexView {
        address: 0x1000_0000,
        size: 96,
        stride: 16,
    };

    #[test]
    fn rectangle_vertex_range_valid() {
        assert_eq!(range(RECTANGLE_VIEW, 3, 1), Some((0x1000_0010, 48, 96)));
    }

    #[test]
    fn rectangle_vertex_range_rejects_vertex_index_overflow() {
        assert_eq!(range(RECTANGLE_VIEW, 3, u32::MAX - 2), None);
        assert_eq!(range(RECTANGLE_VIEW, u32::MAX, 1), None);
    }

    #[test]
    fn rectangle_vertex_range_rejects_address_overflow() {
        let view = VertexView {
            address: u64::MAX - 15,
            ..RECTANGLE_VIEW
        };
        assert_eq!(range(view, 3, 1), None);
        assert_eq!(range(view, 3, 0), None);
    }

    #[test]
    fn rectangle_vertex_range_rejects_byte_offset_overflow() {
        let view = VertexView {
            size: u32::MAX,
            stride: u32::MAX,
            ..RECTANGLE_VIEW
        };
        assert_eq!(range(view, u32::MAX, u32::MAX), None);
    }

    #[test]
    fn rectangle_vertex_range_rejects_oversized_buffers() {
        for stride in [0x8000_0000, 0x4000_0000] {
            let view = VertexView {
                size: u32::MAX,
                stride,
                ..RECTANGLE_VIEW
            };
            assert_eq!(range(view, 3, 0), None);
        }
    }

    /// `VICTORY_RECTANGLE_GUARD=0`: the old arithmetic, whose range wraps.
    #[test]
    fn the_guard_switched_off_keeps_the_old_wrapping() {
        assert_eq!(
            rectangle_vertex_range_guarded(RECTANGLE_VIEW, 3, u32::MAX - 2, false),
            Some((
                0x1000_0000u64.wrapping_add(u64::from(u32::MAX - 2) * 16),
                48,
                96
            ))
        );
        let view = VertexView {
            address: u64::MAX - 15,
            ..RECTANGLE_VIEW
        };
        assert_eq!(
            rectangle_vertex_range_guarded(view, 3, 1, false).map(|r| r.0),
            Some(0u64)
        );
        assert_eq!(
            expanded_rectangle_vertices_guarded(u32::MAX, false),
            Some((u32::MAX / 3).wrapping_mul(6))
        );
    }

    #[test]
    fn expanded_rectangle_vertices_rejects_overflow() {
        let max_rects = u32::MAX / 6;
        assert_eq!(expanded(max_rects * 3), Some(max_rects * 6));
        assert_eq!(expanded((max_rects + 1) * 3), None);
        assert_eq!(expanded(u32::MAX), None);
    }

    #[test]
    fn staging_budget_scales_with_vram_budget() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        assert_eq!(staging_budget_per_frame(None), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(4 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(6 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(8 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(12 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(13 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(14 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(16 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(20 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(24 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(32 * GIB)), 384 * MIB);
    }

    /// A list that binds no sampler heap asks for ours: every signature we
    /// make indexes the sampler heap directly, which D3D12 refuses without
    /// one of that kind bound.
    #[test]
    fn a_list_without_a_sampler_heap_asks_for_ours() {
        // The title binds both kinds: nothing of ours.
        assert!(!wants_our_samplers([0x10, 0x20], true));
        // Views only (a compute list that reads views): ours goes after them.
        assert!(wants_our_samplers([0x10, 0], true));
        // Nothing at all: still one of each kind.
        assert!(wants_our_samplers([0, 0], true));
        // `VICTORY_SAMPLER_HEAP=0`: none of ours, whatever the title bound.
        assert!(!wants_our_samplers([0x10, 0], false));
        assert!(!wants_our_samplers([0, 0], false));
    }
}
