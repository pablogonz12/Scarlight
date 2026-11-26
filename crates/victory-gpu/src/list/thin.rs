//! List calls that only write down their arguments (`VICTORY_THIN=1`); what
//! they mean is worked out by threads of ours, beside the title's.
//!
//! Why: the title makes tens of thousands of list calls a frame, and each one
//! was translated inside the call, on the title's own recording threads: the
//! work of the recording was the frame's wall, and behind it the card stood
//! idle. On the console such a call writes a few words into the list's memory
//! and the card reads them when the list runs. So it is here: a call appends
//! its arguments (and copies of what they point at) to a stream of words;
//! threads of ours run the stream, call by call and in order, through the
//! list methods there were before, on a list of ours ([`Recording::back`]) that
//! the title never sees. They begin while the title still records (every
//! [`CHUNK`] words are handed over), several lists at a time. A submission
//! waits for its recordings' translation (and, with the line of
//! `crate::queue::behind`, goes on from there).
//!
//! What may be looked at later than before, and why that is all right: all
//! that the card of the console looks at when the list runs (descriptor heaps,
//! constants, upload memory, indirect arguments) must stay as it is until the
//! list has run, and no fence behind the list is set before. What D3D12 reads
//! at the call is copied at the call: the arrays and structures passed, and
//! the render target and depth views, whose slots may be written again right
//! after. Objects named (pipelines, resources) must live until the list has
//! run; a recording the title resets or releases without submitting it is not
//! run any further.
//!
//! Adapted from avatarmanz/VIctory-7900XTX `crates/victory-gpu/src/list/thin.rs`
//! (commit 3a99ee610, GPL-3.0-or-later).

use super::*;
use crate::descriptor::RtvRecord;
use std::mem::size_of;
use crate::draw::{IndexView, VertexView};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

/// Words after which a stream is handed to the translating threads.
const CHUNK: usize = 1024;

/// Whether list calls are only written down (`VICTORY_THIN=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        wanted() && crate::host::host().is_some() && {
            start();
            true
        }
    })
}

/// Whether it is asked for.
pub(crate) fn wanted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_THIN").is_some_and(|v| v == "1"))
}

/// What a title list keeps: the recording being written and the words of it
/// not handed over yet.
#[derive(Default)]
pub(super) struct Front {
    recording: Option<Arc<Recording>>,
    words: Vec<u64>,
    closed: bool,
    submitted: bool,
}

impl Drop for Front {
    fn drop(&mut self) {
        self.leave();
    }
}

impl Front {
    /// Lets go of the recording there is: one never submitted is not run any
    /// further.
    fn leave(&mut self) {
        if let Some(recording) = self.recording.take()
            && !self.submitted
        {
            recording.abandoned.store(true, Release);
        }
        self.words.clear();
    }

    /// Begins a recording, with `pipeline` bound.
    pub(super) fn begin(&mut self, kind: u32, pipeline: This) {
        self.leave();
        self.recording = Some(Arc::new(Recording::new(kind)));
        self.closed = false;
        self.submitted = false;
        self.put(RESET, &[pipeline as usize as u64]);
    }

    #[inline]
    fn put(&mut self, op: u64, args: &[u64]) {
        if self.closed || self.recording.is_none() {
            return;
        }
        self.words.push(op | (args.len() as u64) << 16);
        self.words.extend_from_slice(args);
        OPS.fetch_add(1, Relaxed);
        if self.words.len() >= CHUNK {
            self.hand_over();
        }
    }

    /// [`Self::put`] of some words and then the bytes of some things.
    fn put_with(&mut self, op: u64, args: &[u64], blobs: &[&[u8]]) {
        if self.closed || self.recording.is_none() {
            return;
        }
        let at = self.words.len();
        self.words.push(0);
        self.words.extend_from_slice(args);
        for blob in blobs {
            let from = self.words.len();
            self.words.resize(from + blob.len().div_ceil(8), 0);
            // SAFETY: the words just made hold the bytes; plain data.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    blob.as_ptr(),
                    self.words.as_mut_ptr().add(from).cast(),
                    blob.len(),
                )
            };
        }
        let len = (self.words.len() - at - 1) as u64;
        self.words[at] = op | len << 16;
        OPS.fetch_add(1, Relaxed);
        if self.words.len() >= CHUNK {
            self.hand_over();
        }
    }

    fn hand_over(&mut self) {
        let Some(recording) = &self.recording else { return };
        if self.words.is_empty() {
            return;
        }
        let fresh = held(&WORDS).pop().unwrap_or_else(|| Vec::with_capacity(CHUNK + 256));
        let words = std::mem::replace(&mut self.words, fresh);
        STREAMS.fetch_add(1, Relaxed);
        let schedule = {
            let mut inner = held(&recording.inner);
            inner.chunks.push_back(words);
            !std::mem::replace(&mut inner.scheduled, true)
        };
        if schedule {
            held(&READY).push_back(recording.clone());
            WORK.notify_one();
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.put(CLOSE, &[]);
        self.closed = true;
        self.hand_over();
    }
}

/// Streams' memory to use again.
static WORDS: Mutex<Vec<Vec<u64>>> = Mutex::new(Vec::new());
/// Recordings with words to translate.
static READY: Mutex<VecDeque<Arc<Recording>>> = Mutex::new(VecDeque::new());
static WORK: Condvar = Condvar::new();
/// Lists of ours nobody uses, closed.
static BACKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Since the last line of the log: calls written down; calls translated and
/// the microseconds that took (all threads); recordings translated; recordings
/// let go of in the middle, and streams handed over.
static OPS: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static STREAMS: AtomicU64 = AtomicU64::new(0);
static THREADS: AtomicU64 = AtomicU64::new(0);

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One recording of a title list: what was written of it and not translated
/// yet, and the list of ours it goes to.
pub(crate) struct Recording {
    kind: u32,
    inner: Mutex<Inner>,
    /// Translated to its end.
    done: AtomicBool,
    moved: Condvar,
    abandoned: AtomicBool,
}

#[derive(Default)]
struct Inner {
    chunks: VecDeque<Vec<u64>>,
    /// Streams translated (commands may name places in them until the end).
    kept: Vec<Vec<u64>>,
    /// It is among [`READY`] or being translated.
    scheduled: bool,
    /// The list of ours (0: none yet).
    back: usize,
    /// That list was closed by the stream.
    closed: bool,
}

impl Recording {
    fn new(kind: u32) -> Self {
        Recording {
            kind,
            inner: Mutex::default(),
            done: AtomicBool::new(false),
            moved: Condvar::new(),
            abandoned: AtomicBool::new(false),
        }
    }

    /// Whether it is translated to its end.
    pub(crate) fn done(&self) -> bool {
        self.done.load(Acquire)
    }

    /// Waits until it is translated.
    pub(crate) fn wait(&self) {
        let mut inner = held(&self.inner);
        while !self.done() {
            inner = self.moved.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The list of ours it went to (null: it was never begun).
    pub(crate) fn back(&self) -> This {
        held(&self.inner).back as This
    }

    /// Translates what there is.
    fn run(&self) {
        loop {
            let (chunk, back) = {
                let mut inner = held(&self.inner);
                let Some(chunk) = inner.chunks.pop_front() else {
                    inner.scheduled = false;
                    return;
                };
                if inner.back == 0 {
                    inner.back = back_list(self.kind) as usize;
                }
                (chunk, inner.back as This)
            };
            let began = std::time::Instant::now();
            let (calls, closed) = translate(back, &chunk, || self.abandoned.load(Acquire));
            STATS[0].fetch_add(calls, Relaxed);
            STATS[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
            let mut inner = held(&self.inner);
            inner.kept.push(chunk);
            if closed {
                inner.closed = true;
                let kept = std::mem::take(&mut inner.kept);
                self.done.store(true, Release);
                self.moved.notify_all();
                drop(inner);
                give_back(kept);
                STATS[2].fetch_add(1, Relaxed);
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        give_back(inner.chunks.drain(..).chain(std::mem::take(&mut inner.kept)).collect());
        if inner.back != 0 {
            if !inner.closed {
                // Left in the middle.
                STATS[3].fetch_add(1, Relaxed);
                super::close(inner.back as This);
            }
            held(&BACKS).push(inner.back);
        }
    }
}

fn give_back(streams: Vec<Vec<u64>>) {
    let mut words = held(&WORDS);
    for mut s in streams {
        if words.len() < 1024 && s.capacity() <= 16 * CHUNK {
            s.clear();
            words.push(s);
        }
    }
}

/// A list of ours, closed.
fn back_list(kind: u32) -> This {
    if let Some(back) = held(&BACKS).pop() {
        // SAFETY: a list of ours nobody uses.
        unsafe { (*(back as *mut List)).kind = kind };
        return back as This;
    }
    let list: This = Box::into_raw(super::boxed(kind, std::ptr::null(), false)).cast();
    super::close(list);
    list
}

fn start() {
    let threads = std::env::var("VICTORY_THIN_THREADS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(8usize)
        .clamp(1, 32);
    THREADS.store(threads as u64, Relaxed);
    for i in 0..threads {
        let worker = move || {
            loop {
                let recording = {
                    let mut ready = held(&READY);
                    loop {
                        match ready.pop_front() {
                            Some(r) => break r,
                            None => ready = WORK.wait(ready).unwrap_or_else(|e| e.into_inner()),
                        }
                    }
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| recording.run()))
                    .is_err()
                {
                    crate::host::fail(format_args!("a thread that translates lists panicked"));
                }
            }
        };
        if let Err(e) = std::thread::Builder::new()
            .name(format!("victory-list-{i}"))
            .spawn(worker)
        {
            tracing::warn!(error = %e, "a thread less to translate lists");
        }
    }
    tracing::info!(
        threads,
        "list calls are written down and translated by threads of ours (VICTORY_THIN=1)"
    );
}

/// The recordings of submitted lists, in order (one that was not begun is left
/// out), with the lists of ours they go to. Kept while they run: dropping it
/// gives the lists back.
pub(crate) struct Submitted {
    /// Kept alive while the lists run: let go, they give the lists back.
    #[allow(dead_code)]
    recordings: Vec<Arc<Recording>>,
}

// SAFETY: D3D12 objects are free-threaded, and the recordings are behind their
// own locks.
unsafe impl Send for Submitted {}

impl Submitted {
    /// The lists of ours to execute: every recording is translated first.
    ///
    /// The list of ours a recording goes to is made when its translation
    /// begins, which is after this was handed over: it is asked for here, not
    /// where the submission was closed (a recording whose list was not made
    /// yet has none, and asking there took that for good: the submission ran
    /// nothing and a texture the title filled kept zeros).
    pub(crate) fn ready(&self) -> Vec<This> {
        for recording in &self.recordings {
            recording.wait();
        }
        self.recordings.iter().map(|r| r.back()).collect()
    }
}

/// Closes the submitted lists' recordings (on the title's thread: it may reset
/// its lists at once) and hands them to whoever submits them.
pub(crate) fn submitted(lists: &[This]) -> Submitted {
    let recordings: Vec<Arc<Recording>> = lists
        .iter()
        .filter(|l| !l.is_null())
        .filter_map(|&l| {
            // SAFETY: the title passes its lists, which nobody records into now.
            let front = unsafe { &mut (*l.cast_mut().cast::<List>()).thin };
            front.close();
            front.submitted = true;
            front.recording.clone()
        })
        .collect();
    Submitted { recordings }
}

/// For the log: what was written down and translated since the last line.
pub(crate) fn log(frames: f64) {
    if !wanted() || !on() {
        return;
    }
    let ops = OPS.swap(0, Relaxed) as f64 / frames;
    let calls = STATS[0].swap(0, Relaxed) as f64 / frames;
    let us = STATS[1].swap(0, Relaxed) as f64 / frames;
    let recordings = STATS[2].swap(0, Relaxed) as f64 / frames;
    let left = STATS[3].swap(0, Relaxed) as f64 / frames;
    let streams = STREAMS.swap(0, Relaxed) as f64 / frames;
    tracing::info!(
        "{ops:.0} list calls/frame written down, {calls:.0} translated on {} threads \
         ({us:.0} us/frame of translating, {streams:.0} streams, {recordings:.0} recordings, \
         {left:.0} left unsubmitted)",
        THREADS.load(Relaxed)
    );
}

fn front<'a>(this: This) -> &'a mut Front {
    // SAFETY: the title passes one of our lists, and calls one at a time.
    unsafe { &mut (*this.cast_mut().cast::<List>()).thin }
}

/// A plain value as a word of a stream.
trait Word: Copy {
    fn word(self) -> u64;
    fn unword(w: u64) -> Self;
}
macro_rules! words {
    ($($t:ty),*) => {$(
        impl Word for $t {
            #[inline]
            fn word(self) -> u64 { self as u64 }
            #[inline]
            fn unword(w: u64) -> Self { w as $t }
        }
    )* };
}
words!(u8, u32, i32, u64, This);
impl Word for f32 {
    #[inline]
    fn word(self) -> u64 {
        u64::from(self.to_bits())
    }
    #[inline]
    fn unword(w: u64) -> Self {
        f32::from_bits(w as u32)
    }
}

/// The methods whose arguments are all plain values: the one the title calls,
/// and the call made of it later.
macro_rules! plain {
    ($($op:ident $name:ident = $was:path, ($($a:ident: $t:ty),*);)*) => {
        #[allow(clippy::upper_case_acronyms, non_camel_case_types)]
        #[repr(u64)]
        enum Plain { $($op),* , END }
        $(extern "system" fn $name(this: This $(, $a: $t)*) {
            front(this).put(Plain::$op as u64, &[$(Word::word($a)),*]);
        })*
        /// Whether `op` was one of them; done on `list`.
        #[inline]
        fn run_plain(op: u64, list: This, w: &[u64]) -> bool {
            match op {
                $(n if n == Plain::$op as u64 => {
                    let mut words = w.iter();
                    $(let $a: $t = Word::unword(words.next().copied().unwrap_or(0));)*
                    let _ = words;
                    $was(list $(, $a)*);
                })*
                _ => return false,
            }
            true
        }
    };
}

plain! {
    TOPOLOGY topology = super::state::topology, (v: u32);
    STENCIL_REF stencil_ref = super::state::stencil_ref, (v: u32);
    DEPTH_BOUNDS depth_bounds = super::state::depth_bounds, (min: f32, max: f32);
    COMPUTE_TABLE compute_table = super::state::compute_table, (slot: u32, handle: u64);
    GRAPHICS_TABLE graphics_table = super::state::graphics_table, (slot: u32, handle: u64);
    COMPUTE_CONSTANT compute_constant = super::state::compute_constant, (slot: u32, value: u32, offset: u32);
    GRAPHICS_CONSTANT graphics_constant = super::state::graphics_constant, (slot: u32, value: u32, offset: u32);
    COMPUTE_VIEW compute_view = super::state::compute_view, (slot: u32, address: u64);
    GRAPHICS_VIEW graphics_view = super::state::graphics_view, (slot: u32, address: u64);
    SET_PIPELINE set_pipeline = super::set_pipeline, (pipeline: This);
    DRAW draw = super::state::draw, (vertices: u32, instances: u32, first: u32, first_instance: u32);
    DRAW_INDEXED draw_indexed = super::state::draw_indexed, (indices: u32, instances: u32, first: u32, base: i32, first_instance: u32);
    DISPATCH dispatch = super::state::dispatch, (x: u32, y: u32, z: u32);
    DISPATCH_X dispatch_x = super::dispatch_x, (x: u32, y: u32, z: u32, flags: u32);
    DISPATCH_MESH dispatch_mesh = super::state::dispatch_mesh, (x: u32, y: u32, z: u32);
    DISPATCH_MESH_X dispatch_mesh_x = super::state::dispatch_mesh_x, (x: u32, flags: u64, indirect: u64, payload: u64);
    COPY_BUFFER copy_buffer_region = super::copy_buffer_region, (dst: This, dst_offset: u64, src: This, src_offset: u64, len: u64);
    COPY_RESOURCE copy_resource = super::copy_resource, (dst: This, src: This);
    COPY_RESOURCE_X copy_resource_x = super::copy_resource_x, (dst: This, src: This, flags: u32);
    COPY_MEMORY copy_memory = super::copy_memory, (dst: u64, src: u64, len: u64, flags: u32);
    RESOLVE resolve = super::resolve, (dst: This, dst_sub: u32, src: This, src_sub: u32, format: u32);
    RESOLVE_QUERIES resolve_queries = super::resolve_queries, (heap: This, kind: u32, first: u32, count: u32, buffer: This, offset: u64);
    EXECUTE_INDIRECT execute_indirect = super::execute_indirect, (signature: This, max: u32, arguments: This, offset: u64, count: This, count_offset: u64);
    WRITE32 write32 = super::write32, (address: u64, value: u32, flush: u32, flags: u32);
    WRITE64 write64 = super::write64, (address: u64, value: u64, flush: u32, flags: u32);
    WAIT_VALUE wait_value = super::wait_value, (address: u64, comparison: u32, reference: u64);
    WAIT_VALUE64 wait_value64 = super::wait_value64, (address: u64, comparison: u32, reference: u64);
    SYNC sync = super::sync, (flags: u32, control: u32, address: u64, size: u64);
}

const fn after(n: u64) -> u64 {
    Plain::END as u64 + n
}
const RESET: u64 = after(0);
const CLOSE: u64 = after(1);
const UPLOAD: u64 = after(2);
const BARRIER: u64 = after(3);
const BARRIER_GROUPS: u64 = after(4);
const VIEWPORTS: u64 = after(5);
const SCISSORS: u64 = after(6);
const BLEND_FACTOR: u64 = after(7);
const HEAPS: u64 = after(8);
const COMPUTE_CONSTANTS: u64 = after(9);
const GRAPHICS_CONSTANTS: u64 = after(10);
const INDEX_BUFFER: u64 = after(11);
const VERTEX_BUFFERS: u64 = after(12);
const TARGETS: u64 = after(13);
const CLEAR_RTV: u64 = after(14);
const CLEAR_DSV: u64 = after(15);
const COPY_TEXTURE: u64 = after(16);

/// The bytes of `count` things at `items` (none if null).
fn bytes_of<'a, T>(items: *const T, count: usize) -> &'a [u8] {
    if items.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: the title passes `count` items.
    unsafe { std::slice::from_raw_parts(items.cast(), count * size_of::<T>()) }
}

/// Where the things are in a stream (null for none).
fn at<T>(w: &[u64], from: usize, count: usize) -> *const T {
    if count == 0 || from >= w.len() {
        std::ptr::null()
    } else {
        w[from..].as_ptr().cast()
    }
}

/// [`at`] where an empty array is not null.
fn at_or<T>(w: &[u64], from: usize) -> *const T {
    // SAFETY: at most one past the end.
    unsafe { w.as_ptr().add(from.min(w.len())).cast() }
}

/// A view's record as it is now (zeros for no view), so that its slot may be
/// written again: the copy stands for the handle.
fn record(handle: u64) -> [u64; RECORD] {
    let mut words = [0u64; RECORD];
    if handle != 0 {
        // SAFETY: a descriptor slot the title passes (they are larger).
        unsafe {
            std::ptr::copy_nonoverlapping(
                handle as usize as *const u8,
                words.as_mut_ptr().cast(),
                size_of::<RtvRecord>(),
            )
        };
    }
    words
}
const RECORD: usize = size_of::<RtvRecord>().div_ceil(8);

/// Memory a list hands out ([`super::allocate_upload`]): kept until the list
/// of ours it went to is reset.
struct Upload(Vec<u8>);

extern "system" fn list_close(this: This) -> HRESULT {
    front(this).close();
    S_OK
}
extern "system" fn list_reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our lists.
    let kind = unsafe { (*this.cast::<List>()).kind };
    front(this).begin(kind, pipeline);
    S_OK
}
extern "system" fn list_reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    list_reset(this, allocator, pipeline)
}
extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size).ok().and_then(|s| s.checked_add(align)) else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let front = front(this);
    if front.closed || front.recording.is_none() {
        return std::ptr::null_mut();
    }
    let mut block = Upload(vec![0u8; len]);
    let start = block.0.as_mut_ptr();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let place = unsafe { start.add(start.align_offset(align)) };
    front.put(UPLOAD, &[Box::into_raw(Box::new(block)) as usize as u64]);
    place
}
extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    if !barriers.is_null() {
        front(this).put_with(BARRIER, &[u64::from(count)], &[bytes_of(barriers, count as usize)]);
    }
    0
}
extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    front(this).put(BARRIER_GROUPS, &[u64::from(count)]);
}
extern "system" fn viewports(this: This, count: u32, items: *const D3D12_VIEWPORT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(VIEWPORTS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
extern "system" fn scissors(this: This, count: u32, items: *const RECT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(SCISSORS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
    // SAFETY: the title passes four floats or null.
    let Some(factor) = (unsafe { factor.as_ref() }) else {
        return;
    };
    front(this).put_with(BLEND_FACTOR, &[1], &[bytes_of(factor, 1)]);
}
extern "system" fn heaps(this: This, count: u32, items: *const This) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(HEAPS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
fn constants(this: This, op: u64, slot: u32, count: u32, values: *const u32, offset: u32) {
    let count = if values.is_null() { 0 } else { count };
    front(this).put_with(
        op,
        &[u64::from(slot) | u64::from(offset) << 32, u64::from(count)],
        &[bytes_of(values, count as usize)],
    );
}
extern "system" fn compute_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, COMPUTE_CONSTANTS, slot, count, values, offset);
}
extern "system" fn graphics_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, GRAPHICS_CONSTANTS, slot, count, values, offset);
}
extern "system" fn index_buffer(this: This, view: *const IndexView) {
    let mut blob = [0u64; size_of::<IndexView>().div_ceil(8)];
    if !view.is_null() {
        // SAFETY: the title passes a view; the words hold its bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                view.cast::<u8>(),
                blob.as_mut_ptr().cast(),
                size_of::<IndexView>(),
            )
        };
    }
    front(this).put_with(INDEX_BUFFER, &[u64::from(!view.is_null())], &[bytes_of(blob.as_ptr(), blob.len())]);
}
extern "system" fn vertex_buffers(this: This, start: u32, count: u32, views: *const VertexView) {
    let present = !views.is_null();
    let count = if present { count } else { 0 };
    front(this).put_with(
        VERTEX_BUFFERS,
        &[u64::from(start) | u64::from(count) << 32, u64::from(present)],
        &[bytes_of(views, count as usize)],
    );
}
extern "system" fn targets(this: This, count: u32, handles: *const u64, one_range: i32, depth: *const u64) {
    let mut records = vec![0u64; (count as usize + 1) * RECORD];
    let mut present = 0u64;
    for i in 0..count as usize {
        // SAFETY: the title passes `count` handles, or the first of a range
        // of views of 128 bytes.
        let handle = unsafe {
            if one_range != 0 {
                if handles.is_null() {
                    0
                } else {
                    *handles + 128 * i as u64
                }
            } else if handles.is_null() {
                0
            } else {
                *handles.add(i)
            }
        };
        present |= u64::from(handle != 0) << i;
        records[i * RECORD..(i + 1) * RECORD].copy_from_slice(&record(handle));
    }
    // SAFETY: the title passes a handle or null.
    let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
    records[count as usize * RECORD..].copy_from_slice(&record(depth));
    let records = &records[..(count as usize + 1) * RECORD];
    front(this).put_with(
        TARGETS,
        &[u64::from(count), present, u64::from(depth != 0)],
        &[bytes_of(records.as_ptr(), records.len())],
    );
}
extern "system" fn clear_rtv(this: This, handle: u64, color: *const [f32; 4], count: u32, rects: *const Rect) {
    if handle == 0 || color.is_null() {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_RTV,
        &[u64::from(count)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(color, 1), bytes_of(rects, count as usize)],
    );
}
extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    if handle == 0 {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_DSV,
        &[u64::from(count), u64::from(flags) | u64::from(stencil) << 32, Word::word(depth)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(rects, count as usize)],
    );
}
extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    if dst.is_null() || src.is_null() {
        return;
    }
    let boxed = !source_box.is_null();
    front(this).put_with(
        COPY_TEXTURE,
        &[u64::from(x) | u64::from(y) << 32, u64::from(z) | u64::from(boxed) << 32],
        &[bytes_of(dst, 1), bytes_of(src, 1), bytes_of(source_box, usize::from(boxed))],
    );
}
#[allow(clippy::too_many_arguments)] // the method's own arguments
extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

/// Makes the calls of a stream on `list`: how many, and whether the last one
/// closed it. Once `left` says so only what must not be lost is done.
fn translate(list: This, stream: &[u64], left: impl Fn() -> bool) -> (u64, bool) {
    let (mut i, mut calls, mut closed) = (0, 0, false);
    let mut skip = left();
    while i < stream.len() {
        let (op, len) = (stream[i] & 0xffff, (stream[i] >> 16) as usize);
        if i + 1 + len > stream.len() {
            // A stream cut short (a recording let go of mid-word).
            break;
        }
        let w = &stream[i + 1..i + 1 + len];
        i += 1 + len;
        calls += 1;
        if calls % 64 == 0 {
            skip = left();
        }
        if op == UPLOAD {
            // SAFETY: made by `allocate_upload`, named once.
            let block = *unsafe { Box::from_raw(w[0] as usize as *mut Upload) };
            // SAFETY: a list of ours.
            let uploads = unsafe { &(*list.cast::<List>()).uploads };
            held(uploads).push(block.0);
            continue;
        }
        if skip && op != CLOSE && op != RESET {
            continue;
        }
        if run_plain(op, list, w) {
            continue;
        }
        let n = w.first().map_or(0, |&n| n as usize);
        match op {
            RESET => {
                // (Only whether there is an allocator is looked at.)
                let allocator: This =
                    std::ptr::NonNull::<u64>::dangling().as_ptr().cast_const().cast();
                super::reset(list, allocator, w.first().copied().unwrap_or(0) as usize as This);
            }
            CLOSE => {
                super::close(list);
                closed = true;
            }
            BARRIER => {
                super::barrier(list, n as u32, at_or(w, 1));
            }
            BARRIER_GROUPS => super::barrier_groups(list, n as u32, std::ptr::null()),
            VIEWPORTS => super::state::viewports(list, n as u32, at(w, 1, n)),
            SCISSORS => super::state::scissors(list, n as u32, at(w, 1, n)),
            BLEND_FACTOR => super::state::blend_factor(list, at(w, 1, 1)),
            HEAPS => super::state::heaps(list, n as u32, at(w, 1, n)),
            COMPUTE_CONSTANTS => super::state::compute_constants(
                list,
                w[0] as u32,
                w[1] as u32,
                at(w, 2, w[1] as usize),
                (w[0] >> 32) as u32,
            ),
            GRAPHICS_CONSTANTS => super::state::graphics_constants(
                list,
                w[0] as u32,
                w[1] as u32,
                at(w, 2, w[1] as usize),
                (w[0] >> 32) as u32,
            ),
            INDEX_BUFFER => super::state::index_buffer(list, at(w, 1, n)),
            VERTEX_BUFFERS => {
                let views = if w[1] != 0 { at_or(w, 2) } else { std::ptr::null() };
                super::state::vertex_buffers(list, w[0] as u32, (w[0] >> 32) as u32, views);
            }
            TARGETS => {
                let mut handles = [0u64; 8];
                let place = |k: usize| w[3 + k * RECORD..].as_ptr() as usize as u64;
                for (k, h) in handles.iter_mut().enumerate().take(n.min(8)) {
                    if w[1] >> k & 1 != 0 {
                        *h = place(k);
                    }
                }
                let depth = if w[2] != 0 { place(n) } else { 0 };
                super::state::targets(list, n as u32, handles.as_ptr(), 0, &depth);
            }
            CLEAR_RTV => {
                super::clear_rtv(
                    list,
                    w[1..].as_ptr() as usize as u64,
                    at_or(w, 1 + RECORD),
                    n as u32,
                    at(w, 1 + RECORD + 2, n),
                );
            }
            CLEAR_DSV => {
                super::clear_dsv(
                    list,
                    w[3..].as_ptr() as usize as u64,
                    w[1] as u32,
                    Word::unword(w[2]),
                    (w[1] >> 32) as u8,
                    n as u32,
                    at(w, 3 + RECORD, n),
                );
            }
            COPY_TEXTURE => {
                let step = size_of::<CopyLocation>().div_ceil(8);
                let boxed = (w[1] >> 32) as usize;
                super::copy_texture_region(
                    list,
                    at_or(w, 2),
                    w[0] as u32,
                    (w[0] >> 32) as u32,
                    w[1] as u32,
                    at_or(w, 2 + step),
                    at(w, 2 + 2 * step, boxed),
                );
            }
            _ => {}
        }
    }
    (calls, closed)
}

/// The table of a list whose calls are written down: that of the others with
/// the methods that record replaced (slots as in [`super::METHODS`]).
pub(super) const fn table(newer: bool) -> [*const (); 160] {
    let mut f = super::list_table(newer);
    let mut i = 0;
    while i < MINE.len() {
        let (slot, method) = MINE[i];
        f[if newer { super::newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    if newer {
        f[146] = barrier_groups as *const ();
        f[148] = allocate_upload as *const ();
    }
    f
}

const MINE: [(usize, *const ()); 50] = [
    (9, list_close as *const ()),
    (10, list_reset as *const ()),
    (12, draw as *const ()),
    (13, draw_indexed as *const ()),
    (14, dispatch as *const ()),
    (15, copy_buffer_region as *const ()),
    (16, copy_texture_region as *const ()),
    (17, copy_resource as *const ()),
    (19, resolve as *const ()),
    (20, topology as *const ()),
    (21, viewports as *const ()),
    (22, scissors as *const ()),
    (23, blend_factor as *const ()),
    (24, stencil_ref as *const ()),
    (25, set_pipeline as *const ()),
    (26, barrier as *const ()),
    (30, resolve_queries as *const ()),
    (32, heaps as *const ()),
    (35, compute_table as *const ()),
    (36, graphics_table as *const ()),
    (37, compute_constant as *const ()),
    (38, graphics_constant as *const ()),
    (39, compute_constants as *const ()),
    (40, graphics_constants as *const ()),
    (41, compute_view as *const ()),
    (42, graphics_view as *const ()),
    (43, compute_view as *const ()),
    (44, graphics_view as *const ()),
    (45, compute_view as *const ()),
    (46, graphics_view as *const ()),
    (47, index_buffer as *const ()),
    (48, vertex_buffers as *const ()),
    (50, targets as *const ()),
    (51, clear_dsv as *const ()),
    (52, clear_rtv as *const ()),
    (59, execute_indirect as *const ()),
    (60, copy_memory as *const ()),
    (68, depth_bounds as *const ()),
    (69, write32 as *const ()),
    (70, write64 as *const ()),
    (71, wait_value as *const ()),
    (72, wait_value64 as *const ()),
    (78, dispatch_x as *const ()),
    (83, copy_texture_region_x as *const ()),
    (84, copy_resource_x as *const ()),
    (98, dispatch_mesh_x as *const ()),
    (127, dispatch_mesh as *const ()),
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, list_reset_x as *const ()),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every method of the tables that records is written down too: what is
    /// left in both tables alike records nothing.
    #[test]
    fn every_recording_method_is_written_down() {
        const NOTHING: [usize; 16] = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 28, 29, 33, 34, 73, 99, 124,
        ];
        let mine: Vec<usize> = MINE.iter().map(|m| m.0).collect();
        for (slot, _) in super::super::METHODS.iter().chain(super::super::MORE_METHODS.iter()) {
            if NOTHING.contains(slot) {
                assert!(!mine.contains(slot), "slot {slot} records nothing and is written down");
            } else if !matches!(slot, 146 | 148) {
                assert!(mine.contains(slot), "slot {slot} records and is not written down");
            }
        }
        for &slot in &mine {
            assert!(
                super::super::METHODS
                    .iter()
                    .chain(super::super::MORE_METHODS.iter())
                    .any(|m| m.0 == slot),
                "slot {slot} is written down and not there"
            );
        }
    }

    /// A list of a kind, for tests of the stream (no host list).
    fn test_list() -> Box<List> {
        let mut list = super::super::boxed(0, std::ptr::null(), true);
        list.thin.begin(0, std::ptr::null());
        list
    }

    /// Each call writes exactly one record of its words: the count a
    /// submission carries, and the arguments come out as they went in.
    #[test]
    fn a_submission_records_one_record_a_call() {
        let mut list = test_list();
        let before = OPS.load(Relaxed);
        let this: This = (&mut *list as *mut List).cast();
        for n in 0..64u32 {
            draw(this, n, 1, 0, 0);
        }
        assert_eq!(OPS.load(Relaxed) - before, 64, "one record a draw");
        let viewport = D3D12_VIEWPORT {
            TopLeftX: 1.0,
            TopLeftY: 2.0,
            Width: 3.0,
            Height: 4.0,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        viewports(this, 1, &viewport);
        constants(this, COMPUTE_CONSTANTS, 3, 2, [9u32, 10].as_ptr(), 7);
        assert_eq!(OPS.load(Relaxed) - before, 66);
        let words = std::mem::take(&mut list.thin.words);
        // begin's RESET, 64 draws, one viewport, one constants block.
        assert_eq!((words[0] & 0xffff, words[0] >> 16), (RESET, 1));
        let mut i = 1 + 1;
        for n in 0..64u32 {
            assert_eq!((words[i] & 0xffff, words[i] >> 16), (Plain::DRAW as u64, 4));
            assert_eq!(<u32 as Word>::unword(words[i + 1]), n);
            i += 5;
        }
        let len = 1 + size_of::<D3D12_VIEWPORT>().div_ceil(8) as u64;
        assert_eq!((words[i] & 0xffff, words[i] >> 16, words[i + 1]), (VIEWPORTS, len, 1));
        // SAFETY: what was written just now.
        assert_eq!(unsafe { *at::<D3D12_VIEWPORT>(&words[i + 1..], 1, 1) }.Width, 3.0);
        i += 1 + len as usize;
        assert_eq!((words[i] & 0xffff, words[i] >> 16), (COMPUTE_CONSTANTS, 3));
        // SAFETY: as above.
        assert_eq!(unsafe { *at::<[u32; 2]>(&words[i + 1..], 2, 2) }, [9, 10]);
        assert_eq!(words[i + 1] as u32, 3, "slot kept");
        assert_eq!((words[i + 1] >> 32) as u32, 7, "offset kept");
        assert_eq!(words[i + 2] as u32, 2, "count kept");
        assert!(at::<u32>(&words[i + 1..], 2, 0).is_null());
    }

    /// Recording a list's calls is cheaper than translating them, which is
    /// where the win comes from: N draws recorded one at a time write one
    /// record each, and nothing of the list's meaning is worked out on the
    /// recording thread.
    #[test]
    fn recording_is_the_cheap_half_of_a_list() {
        const N: u32 = 20_000;
        let mut list = test_list();
        let this: This = (&mut *list as *mut List).cast();
        let before = OPS.load(Relaxed);
        let began = std::time::Instant::now();
        for n in 0..N {
            draw(this, n, 1, 0, 0);
        }
        let recorded = began.elapsed();
        assert_eq!(OPS.load(Relaxed) - before, u64::from(N), "one record a draw");
        // What translating the same calls through the list methods costs,
        // here with no host list behind them (the floor of that cost).
        let began = std::time::Instant::now();
        for n in 0..N {
            super::super::state::draw(std::ptr::null(), n, 1, 0, 0);
        }
        let plain = began.elapsed();
        eprintln!(
            "{N} draws: recorded {recorded:?} with words written down, {plain:?} through the \
             list methods (no host list, the floor)"
        );
        assert!(OPS.load(Relaxed) > 0);
    }

    /// A view's record is the one at the call, also if its slot is written
    /// again right after.
    #[test]
    fn a_target_is_the_one_at_the_call() {
        let mut slot = [0x5256_5476u64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let kept = record(slot.as_ptr() as usize as u64);
        slot[1] = 99;
        assert_eq!(slot[1], 99, "the slot was written again");
        assert_eq!(kept[1], 2, "the copy is the one at the call");
        assert_eq!(record(0), [0; RECORD]);
    }

    /// A submission names the list of ours it went to when its translation
    /// begins, not where the title closed it: a recording closed before the
    /// translation started has no list yet, and taking that as the answer left
    /// the submission with nothing to run (a texture the title filled kept
    /// zeros).
    #[test]
    fn a_submission_names_the_list_of_ours_its_translation_made() {
        let mut list = test_list();
        let this: This = (&mut *list as *mut List).cast();
        draw(this, 0, 1, 0, 0);
        let submitted = super::submitted(&[this]);
        assert_eq!(submitted.recordings.len(), 1);
        // Translate it here, as a thread of ours would.
        submitted.recordings[0].run();
        let backs = submitted.ready();
        assert_eq!(backs.len(), 1);
        assert!(!backs[0].is_null(), "the list of ours its translation made");
    }
}
