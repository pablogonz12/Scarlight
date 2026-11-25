//! Building the title's pipelines before the title starts.
//!
//! The pipelines of the early list (`library`) are started when the title
//! creates them, during its loading, and nothing waits for them: after an
//! update (a translation changes, so no library entry matches) the world's
//! first frames meet some 2,000 pipelines that are not made, and the draws
//! with them are dropped for the 10 to 30 minutes the compiles take.
//!
//! A pipeline that was drawn with is also kept in `warm-pipelines.bin`
//! (`warmfile`): its shader containers and its graphics state. At start,
//! before any title code runs, [`warm_up`] makes every one of them on all
//! the processors, so that they are in the library when the title creates
//! them, and shows how many are left: a line in the log every 2 s and a
//! small window named after the count. It is done here, not while the title
//! loads, because the title ends the process when its main thread does not
//! advance for about 30 s (decision 0007 notes, the title's watchdog): a
//! wait in one of its threads for minutes would be that, while before the
//! title runs, there is no thread to wait. The title's own timers start
//! after, so none of this time is in them.
//!
//! The list is `warm-pipelines-<hash>.bin`, the hash of the adapter and the
//! driver its pipelines were made by: a new adapter or driver starts a list
//! of its own. A run appends to it as it draws, and its last save writes the
//! whole list through a temporary file renamed over it, so a start reads a
//! whole list and never a part.
//!
//! `VICTORY_WARMUP=0` turns it off; `VICTORY_WARMUP_THREADS=<n>` sets the
//! workers (default: all processors, 2 to 16, all but one of them waiting
//! while less than 4 GB of memory is free); `VICTORY_WARMUP_MAX_SECS=<n>`
//! stops after that long (default: no limit); closing the window stops it
//! too, and the title starts: what was not made is made as before.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::host::GraphicsState;
use super::make::Pending;
use victory_pipeline::warmfile;

const FILE: &str = "warm-pipelines.bin";

/// The list's file: `warm-pipelines-<hash>.bin`, the hash of the adapter and
/// the driver the pipelines are for (`crate::host::adapter_identity`), so a
/// new adapter or driver starts a list of its own rather than a start spent
/// meeting 2,000 pipelines that its host refuses. `None` while the host
/// cannot name them.
fn file_name() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        let identity = crate::host::adapter_identity()?;
        let hash = blake3::hash(identity.as_bytes());
        Some(format!("warm-pipelines-{}.bin", &hash.to_hex()[..12]))
    })
    .as_deref()
}

/// The list's path in `dir`.
fn path(dir: &Path) -> std::path::PathBuf {
    dir.join(file_name().unwrap_or(FILE))
}
/// Pipelines kept in the file, at most (as many as the early list).
const KEPT_MAX: usize = 8192;
/// The name in the window's title.
const PRODUCT: &str = "Scarlight";

#[derive(Default)]
struct Kept {
    /// Keys and shader hashes in the file or waiting to be written.
    keys: HashSet<String>,
    shaders: HashSet<[u8; 32]>,
    /// Frames not written yet.
    frames: Vec<u8>,
    /// The length of the file's good part, once it was read (`None`: not
    /// read yet).
    good: Option<usize>,
}

static KEPT: LazyLock<Mutex<Kept>> = LazyLock::new(Mutex::default);

fn kept_in<'a>(kept: &'a mut Kept, dir: Option<&Path>) -> &'a mut Kept {
    if kept.good.is_none() {
        let contents = dir
            .and_then(|d| std::fs::read(path(d)).ok())
            .map(|b| warmfile::parse(&b))
            .unwrap_or_default();
        kept.keys.extend(contents.pipelines.iter().map(|p| p.key.clone()));
        kept.shaders.extend(contents.shaders.keys().copied());
        kept.good = Some(contents.good);
    }
    kept
}

/// The title drew with the pipeline `p`, which has a host pipeline: keeps
/// what it is made from for the next start.
pub(crate) fn note(p: &Pending) {
    let Some(dir) = super::library::dir() else { return };
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let kept = kept_in(&mut kept, Some(dir));
    if kept.keys.len() >= KEPT_MAX || kept.keys.contains(p.key()) {
        return;
    }
    let (shaders, graphics) = p.parts();
    let mut hashes = Vec::new();
    let mut frames = Vec::new();
    for s in shaders {
        let Some(stage) = warmfile::stage_code(s.stage) else { return };
        let full = *blake3::hash(&s.container).as_bytes();
        hashes.push(full);
        if kept.shaders.insert(full) {
            frames.extend(warmfile::shader_frame(&full, stage, &s.container));
        }
    }
    frames.extend(warmfile::pipeline_frame(p.key(), graphics.map(GraphicsState::to_bytes).as_deref(), &hashes));
    kept.keys.insert(p.key().to_owned());
    kept.frames.extend(frames);
}

/// Writes what [`note`] kept since the last call to the file in `dir`.
/// `atomic`: through a temporary file renamed over the list, for the last
/// write of a run (a copy of the list, so only then); the writes a run makes
/// as it goes are appended where it left off, which a next start reads up to
/// its last whole frame.
pub(crate) fn flush(dir: &Path, atomic: bool) {
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let kept = kept_in(&mut kept, Some(dir));
    if kept.frames.is_empty() {
        return;
    }
    let good = kept.good.unwrap_or(0);
    let written = if atomic { rewrite(&path(dir), good, &kept.frames) } else { append(&path(dir), good, &kept.frames) };
    match written {
        Ok(good) => {
            kept.good = Some(good);
            kept.frames.clear();
        }
        Err(e) => tracing::warn!(error = %e, "warm-up list not saved"),
    }
}

/// Appends the frames after the list's good part, cutting a part-written
/// last frame off first.
fn append(path: &Path, good: usize, frames: &[u8]) -> std::io::Result<usize> {
    let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
    // A file that is not ours, or ends in a part-written frame, is
    // started again from its good part.
    file.set_len(good as u64)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(good as u64))?;
    let mut bytes = Vec::new();
    if good == 0 {
        bytes.extend_from_slice(warmfile::MAGIC);
    }
    bytes.extend_from_slice(frames);
    file.write_all(&bytes)?;
    Ok(good + bytes.len())
}

/// Writes the whole list through a temporary file and renames it over the
/// list: a start in the middle of the write reads the old list, whole.
fn rewrite(path: &Path, good: usize, frames: &[u8]) -> std::io::Result<usize> {
    let old = match good {
        0 => Vec::new(),
        n => {
            let mut bytes = std::fs::read(path)?;
            bytes.truncate(n);
            bytes
        }
    };
    let tmp = path.with_extension("tmp");
    let mut bytes = if good == 0 { warmfile::MAGIC.to_vec() } else { old };
    bytes.extend_from_slice(frames);
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(bytes.len())
}

fn enabled() -> bool {
    std::env::var_os("VICTORY_WARMUP").is_none_or(|v| v != "0")
}

fn threads() -> usize {
    std::env::var("VICTORY_WARMUP_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 16)))
}

/// What a warm-up did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    /// Pipelines listed.
    pub listed: usize,
    /// Pipelines looked at.
    pub done: usize,
    /// Of those, compiled by the host (the library did not hold them).
    pub compiled: u64,
    /// Of those, found in the library.
    pub loaded: u64,
    /// Of those, without a host pipeline (a shader that does not translate).
    pub failed: usize,
    /// Whether it was stopped (window closed, time limit).
    pub stopped: bool,
    pub seconds: f64,
}

/// Makes the pipelines listed in the warm-up file and not yet in the
/// library, before the title runs; `None` if there is nothing to do (no
/// list, turned off, no host GPU).
pub fn warm_up() -> Option<Report> {
    if !enabled() {
        tracing::info!("pipeline warm-up off (VICTORY_WARMUP=0)");
        return None;
    }
    let dir = super::library::dir()?;
    let contents = std::fs::read(path(dir)).ok().map(|b| warmfile::parse(&b)).unwrap_or_default();
    if contents.pipelines.is_empty() {
        tracing::info!("pipeline warm-up: no list yet (it is made from the pipelines this run draws with)");
        return None;
    }
    // The device first: the window should not stand for a host without one.
    crate::host::host()?;

    let jobs = prepare(&contents);
    drop(contents);
    if jobs.is_empty() {
        return None;
    }
    let report = run(&jobs, threads());
    super::library::save(true);
    tracing::info!(
        listed = report.listed,
        compiled = report.compiled,
        loaded = report.loaded,
        failed = report.failed,
        stopped = report.stopped,
        seconds = format!("{:.1}", report.seconds),
        "pipeline warm-up finished"
    );
    Some(report)
}

struct Job {
    key: String,
    shaders: Vec<Arc<super::Shader>>,
    state: Option<Vec<u8>>,
}

fn prepare(contents: &warmfile::Contents) -> Vec<Job> {
    let shaders: std::collections::HashMap<[u8; 32], Arc<super::Shader>> = contents
        .shaders
        .iter()
        .map(|(h, s)| (*h, super::shader(s.stage, &s.container)))
        .collect();
    let mut seen = HashSet::new();
    let mut skipped = 0;
    let jobs: Vec<Job> = contents
        .pipelines
        .iter()
        .filter(|p| seen.insert(p.key.clone()))
        .filter_map(|p| {
            let job = (|| {
                let list = p.shaders.iter().map(|h| shaders.get(h).cloned()).collect::<Option<Vec<_>>>()?;
                Some(Job { key: p.key.clone(), shaders: list, state: p.state.clone() })
            })();
            skipped += usize::from(job.is_none());
            job
        })
        .collect();
    if skipped > 0 {
        tracing::warn!(skipped, "pipeline warm-up: listed pipelines that could not be read");
    }
    jobs
}

fn run(jobs: &[Job], workers: usize) -> Report {
    let total = jobs.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let other_key = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let compiled_before = super::library::MADE.load(Ordering::Relaxed);
    let loaded_before = super::library::LOADED.load(Ordering::Relaxed);
    let began = Instant::now();
    let limit = std::env::var("VICTORY_WARMUP_MAX_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs);
    let window = Window::open(&format!("{PRODUCT} \u{2014} building shaders 0/{total}"));
    tracing::info!(total, workers, "pipeline warm-up: building the listed pipelines before the title starts");

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for i in 0..workers {
            // Each compile can take hundreds of MB: no worker is added, over
            // the first, while the system is short of memory (whatever the
            // processor count says).
            let free = available_mb();
            if i > 0 && free < LOW_MB {
                tracing::info!(workers = i, free_mb = free, "pipeline warm-up: no more workers, memory is short");
                break;
            }
            let (next, done, failed, other_key, stop) = (&next, &done, &failed, &other_key, &stop);
            let work = move || {
                while !stop.load(Ordering::Relaxed) {
                    // Each compile can take hundreds of MB: past the first,
                    // a worker waits while the system is short of memory.
                    while i > 0 && available_mb() < LOW_MB && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(at) else { break };
                    let graphics = match &job.state {
                        Some(bytes) => match GraphicsState::from_bytes(bytes) {
                            Some(g) => Some(Box::new(g)),
                            None => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                done.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        },
                        None => None,
                    };
                    let p = Pending::new(job.shaders.clone(), graphics);
                    if p.key() != job.key {
                        other_key.fetch_add(1, Ordering::Relaxed);
                    }
                    if !p.make_here() {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    super::library::save(false);
                }
            };
            // The host's compilers can use deep stacks.
            let spawned = std::thread::Builder::new()
                .name(format!("victory-warmup-{i}"))
                .stack_size(16 << 20)
                .spawn_scoped(scope, work);
            match spawned {
                Ok(h) => handles.push(h),
                Err(e) => tracing::warn!(error = %e, "no warm-up worker"),
            }
        }
        let mut last_log = Instant::now();
        let mut last_done = 0;
        let mut rate = 0.0f64;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let now = done.load(Ordering::Relaxed);
            if handles.iter().all(|h| h.is_finished()) {
                break;
            }
            let compiled = super::library::MADE.load(Ordering::Relaxed) - compiled_before;
            window.show(&format!("{PRODUCT} \u{2014} building shaders {now}/{total}"));
            if window.closed() || limit.is_some_and(|l| began.elapsed() > l) {
                stop.store(true, Ordering::Relaxed);
                tracing::info!(done = now, total, "pipeline warm-up stopped; the title starts");
                break;
            }
            let since = last_log.elapsed();
            if since >= Duration::from_secs(2) {
                // The rate of the last seconds, smoothed: the pipelines the
                // library holds go by in milliseconds, the compiles do not.
                let fresh = (now - last_done) as f64 / since.as_secs_f64();
                rate = if rate == 0.0 { fresh } else { 0.7 * rate + 0.3 * fresh };
                last_log = Instant::now();
                last_done = now;
                let left = total - now;
                let eta = if rate > 0.0 { format_eta((left as f64 / rate) as u64) } else { "unknown".into() };
                tracing::info!(
                    "pipelines: {now} of {total} built, {left} left, ETA {eta} ({compiled} compiled, {} in the library)",
                    super::library::LOADED.load(Ordering::Relaxed) - loaded_before
                );
            }
        }
    });
    drop(window);
    let done = done.load(Ordering::Relaxed);
    let other = other_key.load(Ordering::Relaxed);
    if other > 0 {
        tracing::warn!(other, "pipeline warm-up: pipelines whose key came out different from the list's");
    }
    Report {
        listed: total,
        done,
        compiled: super::library::MADE.load(Ordering::Relaxed) - compiled_before,
        loaded: super::library::LOADED.load(Ordering::Relaxed) - loaded_before,
        failed: failed.load(Ordering::Relaxed),
        stopped: done < total,
        seconds: began.elapsed().as_secs_f64(),
    }
}

/// Free physical memory below which workers other than the first wait.
const LOW_MB: u64 = 4096;

/// Physical memory available now, in MB (`u64::MAX` if it cannot be asked).
fn available_mb() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a plain query into a structure with its length set.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return u64::MAX;
        }
        status.ullAvailPhys >> 20
    }
}

fn format_eta(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The caches the driver and the translation layer keep on disk, and their
/// sizes: a start that builds pipelines reads them, a run adds to them.
/// Reported after a run (`apps/victory/src/run.rs`).
pub fn report_shader_caches() {
    for (what, dir) in victory_pipeline::caches::dirs() {
        match victory_pipeline::caches::size(&dir) {
            Some((files, bytes)) => tracing::info!(
                cache = what,
                dir = %dir.display(),
                files,
                mb = bytes >> 20,
                "shader cache on disk after the run"
            ),
            None => tracing::info!(cache = what, dir = %dir.display(), "shader cache: no folder"),
        }
    }
}

/// A small window whose title is the progress; closing it stops the
/// warm-up. Its own thread makes and serves it.
struct Window {
    text: Arc<Mutex<Option<String>>>,
    closed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Window {
    fn open(first: &str) -> Window {
        let text = Arc::new(Mutex::new(Some(first.to_owned())));
        let closed = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        // `VICTORY_WARMUP_WINDOW=0`: no window, the log only.
        let wanted = std::env::var_os("VICTORY_WARMUP_WINDOW").is_none_or(|v| v != "0");
        let thread = wanted
            .then(|| {
                let (text, closed, finished) = (text.clone(), closed.clone(), finished.clone());
                std::thread::Builder::new()
                    .name("victory-warmup-window".into())
                    .spawn(move || serve(&text, &closed, &finished))
                    .ok()
            })
            .flatten();
        Window { text, closed, finished, thread }
    }

    fn show(&self, text: &str) {
        *self.text.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_owned());
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(text: &Mutex<Option<String>>, closed: &AtomicBool, finished: &AtomicBool) {
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{PCWSTR, w};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let first = text.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let first = wide(&first);
    // A static control as the whole window: its text is the caption and,
    // centred, the content; closing it is the system's own destroy.
    // SAFETY: plain window creation on this thread, which serves it.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(first.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WINDOW_STYLE(0x1 | 0x200),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            560,
            120,
            None,
            None,
            None,
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        return;
    };
    while !finished.load(Ordering::Relaxed) {
        let mut msg = MSG::default();
        // SAFETY: messages of this thread's own window.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                closed.store(true, Ordering::Relaxed);
                return;
            }
            if let Some(next) = text.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let next = wide(&next);
                let _ = SetWindowTextW(hwnd, PCWSTR(next.as_ptr()));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: our own window.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_reads_in_the_largest_unit() {
        assert_eq!(format_eta(9), "9s");
        assert_eq!(format_eta(75), "1m15s");
        assert_eq!(format_eta(7500), "2h05m");
    }
}
