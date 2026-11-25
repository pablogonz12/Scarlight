//! First uploads of textures started ahead, on a copy queue of their own
//! (`VICTORY_UPLOAD_AHEAD=1`; off), and a submission that samples one not
//! shown before its upload ran (`VICTORY_SHOW_AFTER`; on unless `0`).
//!
//! Ported from avatarmanz/VIctory-7900XTX (`texture/ahead.rs` and
//! `descriptor/host.rs::show_after`, commits 350c68648 and 3bab23c06): there
//! a texture's *first* upload — the one that reads and detiles title memory
//! and copies it into the host texture — was done by the frame's own list, on
//! the submitting thread; it now runs as soon as a list asks for it, on a copy
//! queue of its own, and a slot shows the texture only once its upload ran.
//! His reason for the second half is a device loss every ~16 minutes there: a
//! texture sampled before its upload ran.
//!
//! What it does here:
//!
//! - [`take`] hands a first upload (a staging buffer whose contents no
//!   submitted list has carried yet) to a job: the detiling of title memory
//!   and the copies run on threads of ours, the copies on the copy queue
//!   [`crate::host::Host`] makes beside its direct one. The list that asked
//!   records no copies, so its submission's preparation does not wait for the
//!   read of a whole texture.
//! - [`synchronise`] runs before a submission's lists: for every job still on
//!   its way the direct queue waits on the copy queue's fence (a queue wait,
//!   so the submitting thread is not stopped), or, for a job its worker has
//!   not submitted yet, the thread waits for it, briefly. That is
//!   [`show_after`]: shaders are shown the texture only after its upload ran.
//!   With `VICTORY_SHOW_AFTER=0` a submission may sample the copy before it
//!   ran, as his switch allows, and those are counted.
//! - A later upload of the same texture (the title wrote its memory again) is
//!   the list's, as before: only the first is taken, which is the one that
//!   pays for reading and detiling the whole texture.
//!
//! The copies leave the texture in the common state, which is where a texture
//! no list has used is, for the title's barriers and ours.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether uploads ahead are on (`VICTORY_UPLOAD_AHEAD=1`; off, as in his
/// fork's `-Safe` run, which is the only place it is not).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPLOAD_AHEAD").is_some_and(|v| v != "0"))
}

/// Whether a submission waits for the uploads ahead of what it samples
/// (`VICTORY_SHOW_AFTER`; on unless `0`, as his).
pub(crate) fn show_after() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHOW_AFTER").is_none_or(|v| v != "0"))
}

/// A texture's first upload has not been started ahead.
pub(crate) const NONE: u8 = 0;
/// It is on its way ([`Job`] in `ON_THEIR_WAY`): a list records no copy of
/// its own, and a submission waits for it before its lists run.
pub(crate) const PENDING: u8 = 1;
/// It has run: the host texture holds the staging buffer's contents.
pub(crate) const DONE: u8 = 2;

/// Bytes of staging buffers on their way at most: past it an upload is the
/// list's, as before (his bound).
const MOST: u64 = 96 << 20;
/// Jobs in one submission at most, and their bytes.
const BATCH: (u64, usize) = (16 << 20, 64);
/// How long anything waits for a job whose worker has not submitted it yet
/// (the pop-and-record path is all that is left, microseconds).
const SUBMIT_WAIT: std::time::Duration = std::time::Duration::from_millis(2000);

const COPY_DEST: u32 = 0x400;

/// The staging buffer and the host texture a job copies, as
/// [`super::upload`] records them.
pub(crate) struct Upload {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    pub(crate) texture: ID3D12Resource,
    pub(crate) bytes: u64,
}

/// One texture's first upload, on its way.
pub(crate) struct Job {
    upload: Upload,
    /// The staging buffer's key ([`key`]), which the texture waits on.
    key: usize,
    /// Detiles the staging buffer before the copies are recorded; run on a
    /// worker, so the read of title memory is off the submitting thread.
    prepare: Box<dyn Fn() + Send + Sync>,
    /// The fence value its submission signals (0: not submitted yet).
    value: AtomicU64,
    /// What to do once the copies have run; called on the worker.
    then: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    /// It has run.
    ran: Mutex<bool>,
    ran_now: Condvar,
    asked: std::time::Instant,
}

impl Job {
    /// Calls `f` once the copies have run: now, if they have.
    pub(crate) fn then(&self, f: impl FnOnce() + Send + 'static) {
        if self.done() {
            f();
            return;
        }
        let mut then = self.then.lock().unwrap_or_else(|e| e.into_inner());
        if self.done() {
            drop(then);
            f();
            return;
        }
        then.push(Box::new(f));
    }

    /// Waits up to `timeout` for the copies to have run; false if they have
    /// not. (A title thread never waits without a bound.)
    pub(crate) fn wait(&self, timeout: std::time::Duration) -> bool {
        let mut ran = self.ran.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + timeout;
        while !*ran {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            ran = self
                .ran_now
                .wait_timeout(ran, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Whether the copies have run.
    pub(crate) fn done(&self) -> bool {
        *self.ran.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The fence value the copies signal, 0 while their worker has not
    /// submitted them.
    fn value(&self) -> u64 {
        self.value.load(Relaxed)
    }
}

/// The key of a staging buffer (its address), as a submission names it.
pub(crate) fn key(staging: &Arc<super::Staging>) -> usize {
    key_of(staging)
}

/// [`key`] for a staging buffer that is not behind an `Arc` yet.
pub(crate) fn key_of(staging: &super::Staging) -> usize {
    staging as *const super::Staging as usize
}

static QUEUE: (Mutex<VecDeque<Arc<Job>>>, Condvar) = (Mutex::new(VecDeque::new()), Condvar::new());
/// Those on their way, by staging buffer.
static ON_THEIR_WAY: Mutex<Vec<Arc<Job>>> = Mutex::new(Vec::new());
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Since the start: asked for, that ran, submissions that queued a wait for
/// one, threads that waited for one, submissions that sampled one before it
/// ran, askings turned down, those that ran into an error.
pub(crate) const AHEAD: usize = 0;
pub(crate) const RAN: usize = 1;
pub(crate) const QUEUED_WAIT: usize = 2;
pub(crate) const THREAD_WAIT: usize = 3;
pub(crate) const EARLY: usize = 4;
pub(crate) const TURNED_DOWN: usize = 5;
pub(crate) const FAILED: usize = 6;
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// Microseconds from the asking until the copies had run, all of them, and
/// the longest.
static US: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// [`STATS`], then the bytes on their way, the microseconds all told and the
/// longest, for the tests and the line.
pub(crate) fn counters() -> [u64; 10] {
    [
        STATS[0].load(Relaxed),
        STATS[1].load(Relaxed),
        STATS[2].load(Relaxed),
        STATS[3].load(Relaxed),
        STATS[4].load(Relaxed),
        STATS[5].load(Relaxed),
        STATS[6].load(Relaxed),
        BYTES.load(Relaxed),
        US[0].load(Relaxed),
        US[1].load(Relaxed),
    ]
}

/// Bytes on their way at most are not taken ([`take`] turning them down).
pub(crate) fn full(bytes: u64) -> bool {
    BYTES.load(Relaxed) + bytes > MOST
}

/// Starts `upload` on its way, `prepare` having detiled the staging buffer
/// first; `None` when there is no copy queue (the list copies, as before) or
/// the jobs on their way are full. The switch is the caller's ([`on`]).
pub(crate) fn take(
    key: usize,
    upload: Upload,
    prepare: Box<dyn Fn() + Send + Sync>,
) -> Option<Arc<Job>> {
    if full(upload.bytes) || workers().is_none() {
        STATS[TURNED_DOWN].fetch_add(1, Relaxed);
        return None;
    }
    let bytes = upload.bytes;
    let job = Arc::new(Job {
        upload,
        key,
        prepare,
        value: AtomicU64::new(0),
        then: Mutex::new(Vec::new()),
        ran: Mutex::new(false),
        ran_now: Condvar::new(),
        asked: std::time::Instant::now(),
    });
    BYTES.fetch_add(bytes, Relaxed);
    STATS[AHEAD].fetch_add(1, Relaxed);
    ON_THEIR_WAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(job.clone());
    QUEUE.0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(job.clone());
    QUEUE.1.notify_one();
    Some(job)
}

/// The job of the staging buffer at `key`, if it is still on its way.
fn on_the_way(key: usize) -> Option<Arc<Job>> {
    ON_THEIR_WAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|job| job.key == key)
        .cloned()
}

/// Waits, briefly, for the upload ahead of the staging buffer at `key` to
/// have run: a refresh of it must not write the bytes a copy is reading.
pub(crate) fn settle(key: usize) {
    let Some(job) = on_the_way(key) else {
        return;
    };
    if job.done() {
        return;
    }
    STATS[THREAD_WAIT].fetch_add(1, Relaxed);
    if !job.wait(SUBMIT_WAIT) {
        STATS[FAILED].fetch_add(1, Relaxed);
        tracing::warn!(key, "a texture's upload ahead did not run: its contents are a frame late");
    }
}

/// A submission is about to run and samples the texture at `key`: its upload
/// ahead is waited for (`show_after`), or it is not (counted).
pub(crate) fn shown(key: usize) {
    let Some(job) = on_the_way(key) else {
        return;
    };
    if !show_after() {
        STATS[EARLY].fetch_add(1, Relaxed);
        return;
    }
    if let Some(host) = crate::host::host()
        && let value = job.value()
        && value != 0
        && host.wait_copy(value)
    {
        // The direct queue waits on the copy queue's fence: the lists that
        // follow read what the copies wrote, and this thread is not stopped.
        STATS[QUEUED_WAIT].fetch_add(1, Relaxed);
        return;
    }
    // Its worker has not submitted it yet (or the queue would not take the
    // wait): the thread waits for the copies.
    STATS[THREAD_WAIT].fetch_add(1, Relaxed);
    if !job.wait(SUBMIT_WAIT) {
        STATS[FAILED].fetch_add(1, Relaxed);
        tracing::warn!(key, "a texture's upload ahead did not run: it is sampled as it was");
    }
}

/// Before a submission's lists run: every upload ahead still on its way is
/// waited for ([`shown`]), whichever of the submission's lists asked.
pub(crate) fn synchronise() {
    let keys: Vec<usize> = ON_THEIR_WAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|job| job.key)
        .collect();
    for key in keys {
        shown(key);
    }
}

/// The copy queue `host` made beside its direct one, if any.
struct Copy {
    host: &'static crate::host::Host,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The fence value of the next submission: one counter for the queue, as
    /// a fence only moves forward (a value at or below the one it holds is
    /// not signalled, and a wait for it would end at once).
    next: AtomicU64,
}

impl Copy {
    /// The host and the copy queue created at device creation, if there is
    /// one.
    fn get() -> Option<&'static Copy> {
        static COPY: OnceLock<Option<Copy>> = OnceLock::new();
        COPY.get_or_init(|| {
            let host = crate::host::host()?;
            let (queue, fence) = host.copy_queue()?;
            Some(Copy {
                host,
                queue: queue.clone(),
                fence: fence.clone(),
                next: AtomicU64::new(0),
            })
        })
        .as_ref()
    }
}

/// The copy queue and its threads, made on first use.
fn workers() -> Option<&'static Copy> {
    static STARTED: OnceLock<Option<()>> = OnceLock::new();
    // One worker: a fence moves forward once for the queue, so a second
    // worker's batches would have to be ordered with this one's anyway
    // (shortcut: the detilings of two textures are not done at once).
    STARTED.get_or_init(|| {
        Copy::get()?;
        std::thread::Builder::new()
            .name("uploads ahead".to_owned())
            .spawn(|| {
                if let Err(e) = work() {
                    tracing::warn!(error = %e, "uploads ahead stopped");
                }
            })
            .ok()?;
        Some(())
    });
    Copy::get()
}

/// What one worker owns while it lives.
struct Worker {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    event: HANDLE,
}

impl Worker {
    fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            let event =
                windows::Win32::System::Threading::CreateEventW(None, false, false, None)?;
            Ok(Self { allocator, list, event })
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: the event is ours.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

/// Takes jobs off [`QUEUE`] until the process ends.
fn work() -> windows::core::Result<()> {
    let copy = Copy::get().ok_or_else(windows::core::Error::empty)?;
    let device = copy.host.device.clone();
    let worker = Worker::new(&device)?;
    loop {
        let jobs = next();
        for job in &jobs {
            (job.prepare)();
        }
        let value = copy.next.fetch_add(1, Relaxed) + 1;
        for job in &jobs {
            job.value.store(value, Relaxed);
        }
        let ran = match submit(copy, &worker, &jobs, value) {
            Err(e) => {
                STATS[FAILED].fetch_add(jobs.len() as u64, Relaxed);
                tracing::warn!(error = %e, jobs = jobs.len(), "uploads ahead did not run");
                false
            }
            Ok(()) => {
                // SAFETY: the fence and the event are ours, and the jobs
                // hold what the submission names until it has run.
                unsafe {
                    copy.fence.SetEventOnCompletion(value, worker.event)?;
                    let waited = windows::Win32::System::Threading::WaitForSingleObject(
                        worker.event,
                        windows::Win32::System::Threading::INFINITE,
                    );
                    if waited != WAIT_OBJECT_0 {
                        tracing::warn!(?waited, "uploads ahead not waited for");
                    }
                }
                true
            }
        };
        finish(&jobs, ran);
    }
}

/// The next submission's jobs: at least one, more while [`BATCH`] allows.
fn next() -> Vec<Arc<Job>> {
    let mut jobs: Vec<Arc<Job>> = Vec::new();
    let mut bytes = 0;
    let mut queue = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
    while jobs.is_empty() {
        match queue.pop_front() {
            Some(job) => {
                bytes += job.upload.bytes;
                jobs.push(job);
            }
            None => queue = QUEUE.1.wait(queue).unwrap_or_else(|e| e.into_inner()),
        }
    }
    while jobs.len() < BATCH.1 && bytes < BATCH.0 {
        let Some(job) = queue.pop_front() else { break };
        bytes += job.upload.bytes;
        jobs.push(job);
    }
    jobs
}

/// Records the jobs' copies into `worker`'s list and submits them on the copy
/// queue, which signals `value`.
fn submit(copy: &Copy, worker: &Worker, jobs: &[Arc<Job>], value: u64) -> windows::core::Result<()> {
    // SAFETY: this worker's allocator and list, whose last submission has
    // run; the jobs hold what the list names until after the wait.
    unsafe {
        worker.allocator.Reset()?;
        worker.list.Reset(&worker.allocator, None)?;
        for job in jobs {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
            let common = crate::host::transition(
                &job.upload.texture,
                State(0),
                State(COPY_DEST as i32),
            );
            worker.list.ResourceBarrier(&[common]);
            for (i, footprint) in job.upload.footprints.iter().enumerate() {
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: a borrowed pointer the location never releases.
                    pResource: std::mem::transmute_copy(&job.upload.texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: as above.
                    pResource: std::mem::transmute_copy(&job.upload.buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
                };
                worker.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            }
            // Left where a texture no list has used is: that is the state the
            // title's barriers and ours assume for one.
            let done = crate::host::transition(
                &job.upload.texture,
                State(COPY_DEST as i32),
                State(0),
            );
            worker.list.ResourceBarrier(&[done]);
        }
        worker.list.Close()?;
        copy.queue.ExecuteCommandLists(&[Some(worker.list.cast()?)]);
        copy.queue.Signal(&copy.fence, value)?;
    }
    Ok(())
}

/// The jobs' copies have run (or could not be recorded: then not).
fn finish(jobs: &[Arc<Job>], ran: bool) {
    {
        let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        way.retain(|job| !jobs.iter().any(|done| Arc::ptr_eq(job, done)));
    }
    let mut us = 0;
    if ran {
        let n = STATS[RAN].fetch_add(jobs.len() as u64, Relaxed) + jobs.len() as u64;
        for job in jobs {
            let took = job.asked.elapsed().as_micros() as u64;
            us += took;
            US[1].fetch_max(took, Relaxed);
        }
        let all = US[0].fetch_add(us, Relaxed) + us;
        if n == jobs.len() as u64 || n.is_multiple_of(5000) {
            let counters = counters();
            tracing::info!(
                "textures uploaded ahead of the lists (VICTORY_UPLOAD_AHEAD=1), since the start: {n}, {} MB, {:.1} ms from the asking until they were there, the longest {:.0} ms; {} queue waits, {} thread waits, {} shown early, {} turned down, {} failed",
                counters[7] >> 20,
                all as f64 / n as f64 / 1e3,
                counters[9] as f64 / 1e3,
                counters[QUEUED_WAIT],
                counters[THREAD_WAIT],
                counters[EARLY],
                counters[TURNED_DOWN],
                counters[FAILED]
            );
        }
    }
    for job in jobs {
        BYTES.fetch_sub(job.upload.bytes, Relaxed);
        *job.ran.lock().unwrap_or_else(|e| e.into_inner()) = true;
        job.ran_now.notify_all();
        let then = std::mem::take(&mut *job.then.lock().unwrap_or_else(|e| e.into_inner()));
        for f in then {
            f();
        }
    }
}

/// A drawing list of ours, for the microbench's list-path side.
#[cfg(test)]
fn direct_list(host: &crate::host::Host) -> Option<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
    // SAFETY: plain object creation.
    unsafe {
        let allocator: ID3D12CommandAllocator = host
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .ok()?;
        let list: ID3D12GraphicsCommandList = host
            .device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .ok()?;
        list.Close().ok()?;
        Some((allocator, list))
    }
}

/// A copy list of ours, for the tests that submit their own.
#[cfg(test)]
fn copy_list(host: &crate::host::Host) -> Option<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
    // SAFETY: plain object creation.
    unsafe {
        let allocator: ID3D12CommandAllocator = host
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
            .ok()?;
        let list: ID3D12GraphicsCommandList = host
            .device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
            .ok()?;
        list.Close().ok()?;
        Some((allocator, list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::host;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    /// Copy source, which a read back names (the texture path's own is in
    /// `crate::list`).
    const COPY_SOURCE: u32 = 0x800;

    /// BC7 UNORM: the format a title's compressed texture boards are in.
    const BC7: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT =
        windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(98);

    /// A texture in the default heap, as the host copy of a title texture is.
    fn texture(
        host: &crate::host::Host,
        width: u32,
        height: u32,
        mips: u16,
    ) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: BC7,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a texture.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATES(0),
                    None,
                    &mut made,
                )
                .ok()?
        };
        made
    }

    /// The staging buffer of `texture`, filled from `byte`, and what a job
    /// copies: its footprints and bytes.
    fn staging(
        host: &crate::host::Host,
        texture: &ID3D12Resource,
        byte: impl Fn(usize) -> u8,
    ) -> Option<Upload> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let count = u32::from(desc.MipLevels) * u32::from(desc.DepthOrArraySize);
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
        let mut rows = vec![0u32; count as usize];
        let mut total = 0;
        // SAFETY: outputs for `count` subresources.
        unsafe {
            host.device.GetCopyableFootprints(
                &desc,
                0,
                count,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            );
        }
        let total = footprints
            .iter()
            .zip(&rows)
            .map(|(f, &r)| {
                f.Offset + u64::from(f.Footprint.RowPitch) * u64::from(r) * u64::from(f.Footprint.Depth)
            })
            .fold(total, u64::max);
        let (buffer, data) = host.upload_buffer(total)?;
        for (i, footprint) in footprints.iter().enumerate() {
            let f = footprint.Footprint;
            let len = (f.RowPitch * rows[i] * f.Depth) as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(data.add(footprint.Offset as usize), len)
            };
            for (n, b) in linear.iter_mut().enumerate() {
                *b = byte(n + footprint.Offset as usize);
            }
        }
        Some(Upload { buffer, footprints, texture: texture.clone(), bytes: total })
    }

    /// Reads `texture` back through the copy queue (its own list and fence),
    /// after `show_after` has been honoured by the caller.
    fn read_back(host: &crate::host::Host, texture: &ID3D12Resource) -> Option<Vec<u8>> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let count = u32::from(desc.MipLevels);
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
        let mut rows = vec![0u32; count as usize];
        let mut total = 0;
        // SAFETY: outputs for `count` subresources.
        unsafe {
            host.device.GetCopyableFootprints(
                &desc,
                0,
                count,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            );
        }
        let (queue, fence) = host.copy_queue()?;
        // SAFETY: a plain query.
        let value = unsafe { fence.GetCompletedValue() } + 1;
        let (allocator, list) = copy_list(host)?;
        let back = host.read_back_buffer(total)?;
        let event =
            // SAFETY: a plain event of ours.
            unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
                .ok()?;
        // SAFETY: plain recording on a list of ours, waited for below.
        unsafe {
            allocator.Reset().ok()?;
            list.Reset(&allocator, None).ok()?;
            let to_source = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(0),
                D3D12_RESOURCE_STATES(COPY_SOURCE as i32),
            );
            list.ResourceBarrier(&[to_source]);
            for (i, footprint) in footprints.iter().enumerate() {
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&back),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
                };
                list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            }
            let to_common = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(COPY_SOURCE as i32),
                D3D12_RESOURCE_STATES(0),
            );
            list.ResourceBarrier(&[to_common]);
            list.Close().ok()?;
            queue.ExecuteCommandLists(&[Some(list.cast().ok()?)]);
            queue.Signal(fence, value).ok()?;
            fence.SetEventOnCompletion(value, event).ok()?;
            windows::Win32::System::Threading::WaitForSingleObject(
                event,
                windows::Win32::System::Threading::INFINITE,
            );
            let _ = CloseHandle(event);
            let mut at = std::ptr::null_mut();
            back.Map(0, None, Some(&mut at)).ok()?;
            let pitch = footprints[0].Footprint.RowPitch as usize;
            let len = pitch * rows[0] as usize;
            let bytes = std::slice::from_raw_parts(at.cast::<u8>(), len).to_vec();
            back.Unmap(0, None);
            Some(bytes)
        }
    }

    /// Our device makes a copy queue beside its direct one, and it is one:
    /// the family an upload ahead's copies run on is a copy queue's own, not
    /// the drawing queue's.
    #[test]
    fn the_host_makes_a_copy_queue_of_its_own() {
        let _lock = crate::warptest::lock();
        let Some(host) = host() else { return };
        let Some((queue, fence)) = host.copy_queue() else {
            // A device without one is a finding, not a skip: the ports of
            // his fork's upload ahead cannot run at all there.
            panic!("the host device has no copy queue: VICTORY_UPLOAD_AHEAD cannot be used");
        };
        // SAFETY: plain queries.
        let desc = unsafe { queue.GetDesc() };
        assert_eq!(
            desc.Type,
            D3D12_COMMAND_LIST_TYPE_COPY,
            "the uploads ahead queue is a copy queue"
        );
        // The copy queue drives that fence, and a wait on the value it
        // signals ends: the two queues of the device are kept in step by
        // fences, which is what a submission waits on (`wait_copy`).
        // SAFETY: a plain query, a signal of ours and an event of ours.
        let made = unsafe {
            let value = fence.GetCompletedValue() + 1;
            let event =
                windows::Win32::System::Threading::CreateEventW(None, false, false, None)
                    .expect("an event");
            queue.Signal(fence, value).expect("the copy queue signals");
            fence.SetEventOnCompletion(value, event).expect("an event on the fence");
            let waited = windows::Win32::System::Threading::WaitForSingleObject(
                event,
                windows::Win32::System::Threading::INFINITE,
            );
            let _ = CloseHandle(event);
            (value, waited)
        };
        assert_eq!(made.1, WAIT_OBJECT_0, "the copy queue's signal is waited on");
        // SAFETY: a plain query.
        assert!(
            unsafe { fence.GetCompletedValue() } >= made.0,
            "and the fence holds it"
        );
        assert!(
            host.wait_copy(made.0),
            "the drawing queue takes a wait on the copy queue's fence"
        );
    }

    /// A texture uploaded ahead is in the host copy before the submission
    /// that samples it runs (`VICTORY_SHOW_AFTER`), and the submission waits
    /// for the copy queue either through the queue or the thread: the bytes
    /// read back are the staging buffer's, not the texture's old contents.
    #[test]
    fn a_texture_uploaded_ahead_is_there_when_the_submission_samples_it() {
        let _lock = crate::warptest::lock();
        let Some(host) = host() else { return };
        let Some(texture) = texture(host, 256, 256, 1) else {
            panic!("no 256x256 BC7 texture");
        };
        let Some(upload) = staging(host, &texture, |n| (n * 7 + 3) as u8) else {
            panic!("no staging buffer");
        };
        let bytes = upload.bytes;
        let led = counters();
        let key = 0x000a_2ead;
        let Some(job) = take(key, upload, Box::new(|| {})) else {
            panic!("no upload ahead taken: the host has no copy queue");
        };
        // What a submission does before its lists: wait for the upload.
        synchronise();
        let after = counters();
        assert_eq!(after[AHEAD], led[AHEAD] + 1, "one upload started ahead");
        if show_after() {
            assert!(
                after[QUEUED_WAIT] + after[THREAD_WAIT] > led[QUEUED_WAIT] + led[THREAD_WAIT],
                "the submission waited for it: {after:?}"
            );
            assert_eq!(after[EARLY], led[EARLY], "nothing was sampled before it ran");
        } else {
            assert_eq!(after[EARLY], led[EARLY] + 1, "shown before it ran (switch off)");
        }
        assert!(job.done(), "the copies have run");
        assert_eq!(
            after[7],
            led[7],
            "no bytes are on their way once they have run"
        );
        let read = read_back(host, &texture).expect("a read back of the texture");
        let mut expected = vec![0u8; read.len()];
        for (n, b) in expected.iter_mut().enumerate() {
            *b = (n * 7 + 3) as u8;
        }
        assert_eq!(read, expected, "{bytes} bytes copied ahead of the lists, as staged");
    }

    /// Uploads started ahead and waits for them are counted, and a job's
    /// callbacks run once the copies have: the counter test for the pair.
    #[test]
    fn uploads_started_ahead_and_waits_for_them_are_counted() {
        let _lock = crate::warptest::lock();
        let Some(host) = host() else { return };
        let Some(texture) = texture(host, 64, 64, 1) else {
            panic!("no 64x64 BC7 texture");
        };
        let Some(upload) = staging(host, &texture, |n| n as u8) else {
            panic!("no staging buffer");
        };
        let led = counters();
        let key = 0xc0_0c;
        let job = take(key, upload, Box::new(|| {})).expect("taken ahead");
        let (told, heard) = std::sync::mpsc::channel();
        job.then(move || told.send(1).unwrap());
        // Twice: whichever of the queue wait and the thread wait comes first,
        // both are counted, and the second asking finds it done.
        synchronise();
        synchronise();
        assert!(heard.recv_timeout(std::time::Duration::from_secs(20)).is_ok());
        let after = counters();
        assert_eq!(after[AHEAD], led[AHEAD] + 1);
        assert_eq!(after[RAN], led[RAN] + 1);
        assert_eq!(after[FAILED], led[FAILED], "no upload ahead failed");
        if show_after() {
            assert!(after[QUEUED_WAIT] + after[THREAD_WAIT] >= 1, "{after:?}");
            assert_eq!(after[EARLY], led[EARLY], "and none was shown early");
        } else {
            assert_eq!(after[EARLY], led[EARLY] + 2, "both waitings skipped (switch off)");
        }
    }

    /// The microbench: a 4k BC7 texture's upload, 16 MiB of texels, through
    /// the copy queue the ahead path uses and through a list, in
    /// microseconds. Not a gate on the numbers (the drivers under Wine are
    /// what they are), but the figures the report quotes.
    #[test]
    fn a_4k_bc7_upload_is_timed_on_the_copy_queue_and_in_a_list() {
        let _lock = crate::warptest::lock();
        let Some(host) = host() else { return };
        let mut ahead_us = Vec::new();
        let mut list_us = Vec::new();
        let mut bytes = 0;
        // A throwaway upload first: the worker thread, its allocator and its
        // list are made on first use, and that is not the upload's cost.
        if let Some(texture) = texture(host, 64, 64, 1)
            && let Some(upload) = staging(host, &texture, |n| n as u8)
        {
            let _ = take(0x00_abcd, upload, Box::new(|| {}));
            synchronise();
        }
        for _ in 0..3 {
            let Some(texture) = texture(host, 4096, 4096, 13) else {
                panic!("no 4096x4096 BC7 texture");
            };
            let Some(upload) = staging(host, &texture, |n| (n * 5) as u8) else {
                panic!("no staging buffer");
            };
            bytes = upload.bytes;
            let started = std::time::Instant::now();
            let job = take(0x4_7004, upload, Box::new(|| {})).expect("taken ahead");
            synchronise();
            assert!(job.wait(std::time::Duration::from_secs(20)));
            ahead_us.push(started.elapsed().as_micros() as u64);
        }
        for _ in 0..3 {
            let Some(texture) = texture(host, 4096, 4096, 13) else {
                panic!("no 4096x4096 BC7 texture");
            };
            let Some(upload) = staging(host, &texture, |n| (n * 5) as u8) else {
                panic!("no staging buffer");
            };
            let (allocator, list) = direct_list(host).expect("a direct list");
            let started = std::time::Instant::now();
            // SAFETY: our allocator and list, run and waited for by the host.
            unsafe {
                allocator.Reset().expect("allocator reset");
                list.Reset(&allocator, None).expect("list reset");
                let common = crate::host::transition(
                    &upload.texture,
                    D3D12_RESOURCE_STATES(0),
                    D3D12_RESOURCE_STATES(COPY_DEST as i32),
                );
                list.ResourceBarrier(&[common]);
                for (i, footprint) in upload.footprints.iter().enumerate() {
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&upload.texture),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
                    };
                    let from = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&upload.buffer),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
                    };
                    list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                }
                list.Close().expect("list close");
            }
            assert!(
                host.run_raw(&list.cast::<ID3D12CommandList>().expect("a list")),
                "the drawing queue ran the list"
            );
            list_us.push(started.elapsed().as_micros() as u64);
        }
        let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
        eprintln!(
            "a 4k BC7 upload, {bytes} bytes: ahead {ahead_us:?}, in a list {list_us:?}: mean {:.0} us ahead, {:.0} us in a list",
            mean(&ahead_us),
            mean(&list_us)
        );
        assert!(ahead_us.iter().all(|&us| us > 0) && list_us.iter().all(|&us| us > 0));
    }
}
