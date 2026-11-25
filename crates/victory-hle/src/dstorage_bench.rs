//! The offline benchmark of the DirectStorage path (`victory
//! dstorage-bench`): the same `execute` a title's requests take, over the
//! title's own files (opened read-only, never written) or memory standing
//! in for them, with a chosen number of workers.
//!
//! Why: a load's 5 s dstorage window showed the workers busy on average
//! while almost none of them was inside a read (loading.md, "the 142 s
//! middle"): the time is in what a worker does with the bytes and in the
//! destination, not in the disk. Game runs are held until a change is
//! measured offline, so this is where a request's stages are measured.
//!
//! Modes (each drives `execute`, so the timers, the locks and the
//! destination code are the load's own):
//! - `raw`: file reads into an address, as uncompressed data requests are.
//! - `page`: the same reads into 64 KiB physical pages, the form the
//!   title's texture and stream destinations take.
//! - `zlib`: the load's inflate, over a chunk of the files deflated once.
//! - `zpage`: the inflate into physical pages.
//!
//! BCPack is not benched: its streams come from the title's encoder and
//! none is recorded offline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::modules::dstorage::bench::{self, Pages, STAGES, Source, Target};
use crate::modules::dstorage::take_stages;

/// What to run.
pub struct Opts {
    /// The archive files to read (the title's `.rpf`s). Never written.
    pub files: Vec<PathBuf>,
    pub workers: usize,
    pub seconds: f64,
    /// Bytes per request.
    pub block: usize,
    pub mode: Mode,
    /// Bytes of the files to deflate for the `zlib` modes.
    pub corpus_mb: usize,
    /// Page destinations a worker rotates through: a load's requests land
    /// in many different pages, not always the same ones.
    pub tiles: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Raw,
    Page,
    Zlib,
    Zpage,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::Page => "page",
            Mode::Zlib => "zlib",
            Mode::Zpage => "zpage",
        }
    }

    fn pages(self) -> bool {
        matches!(self, Mode::Page | Mode::Zpage)
    }

    fn zlib(self) -> bool {
        matches!(self, Mode::Zlib | Mode::Zpage)
    }
}

/// What the bench measured.
#[derive(Default)]
pub struct Report {
    pub mode: &'static str,
    pub workers: usize,
    pub requests: u64,
    pub bytes: u64,
    pub seconds: f64,
    /// Requests over 100 ms, and the slowest one.
    pub slow: u64,
    pub slowest_ms: f64,
    /// Worker-seconds in each stage, from the module's own counters.
    pub stages: Vec<(&'static str, f64)>,
}

impl Report {
    pub fn mb_per_s(&self) -> f64 {
        self.bytes as f64 / self.seconds.max(1e-9) / 1e6
    }

    pub fn print(&self) {
        let n = self.requests.max(1) as f64;
        println!(
            "mode={} workers={} requests={} bytes={} seconds={:.1}",
            self.mode, self.workers, self.requests, self.bytes, self.seconds
        );
        println!(
            "throughput {:.1} MB/s; {:.0} requests/s; mean {:.2} ms; slowest {:.1} ms; {} over 100 ms",
            self.mb_per_s(),
            self.requests as f64 / self.seconds.max(1e-9),
            self.seconds * 1e3 / n,
            self.slowest_ms,
            self.slow
        );
        let ms: Vec<String> = self
            .stages
            .iter()
            .map(|(name, s)| format!("{name} {:.2}", s / n * 1e3))
            .collect();
        println!("stage mean ms/request: {}", ms.join(" "));
        let workers: Vec<String> = self
            .stages
            .iter()
            .map(|(name, s)| format!("{name} {:.2}", s / self.seconds.max(1e-9)))
            .collect();
        println!("stage workers: {}", workers.join(" "));
        let total: f64 = self.stages.iter().map(|(_, s)| s).sum();
        println!(
            "stages {:.1} s of {:.1} s of {:.1} worker-seconds ({:.0} % accounted)",
            total,
            self.seconds * self.workers as f64,
            self.seconds * self.workers as f64,
            100.0 * total / (self.seconds * self.workers as f64).max(1e-9)
        );
    }
}

/// One worker's tally.
#[derive(Default)]
struct Part {
    requests: u64,
    bytes: u64,
    slow: u64,
    slowest_ms: f64,
}

impl Part {
    fn done(&mut self, request_s: f64) {
        self.requests += 1;
        if request_s > 0.1 {
            self.slow += 1;
        }
        self.slowest_ms = self.slowest_ms.max(request_s * 1e3);
    }
}

pub fn run(opts: &Opts) -> Result<Report, String> {
    if opts.files.is_empty() {
        return Err("no files to read".to_owned());
    }
    let corpus = if opts.mode.zlib() {
        Some(Arc::new(corpus(opts)?))
    } else {
        None
    };
    let before = take_stages();
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(opts.seconds.max(0.1));
    let parts = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..opts.workers.clamp(1, 256))
            .map(|id| {
                let corpus = corpus.clone();
                scope.spawn(move || worker(id, opts, corpus.as_deref().map(Vec::as_slice), deadline))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().map_err(|_| "a worker panicked".to_owned()))
            .collect::<Result<Vec<_>, String>>()
    })?;
    let seconds = started.elapsed().as_secs_f64();
    let after = take_stages();
    let mut report = Report {
        mode: opts.mode.name(),
        workers: opts.workers.clamp(1, 256),
        seconds,
        stages: STAGES
            .iter()
            .zip(before.into_iter().zip(after))
            .map(|(name, (b, a))| (*name, a.saturating_sub(b) as f64 / 1e9))
            .collect(),
        ..Default::default()
    };
    for part in parts {
        report.requests += part.requests;
        report.bytes += part.bytes;
        report.slow += part.slow;
        report.slowest_ms = report.slowest_ms.max(part.slowest_ms);
    }
    Ok(report)
}

/// `corpus_mb` of the files as packed chunks, each inflating to `block`
/// bytes: what the `zlib` modes inflate, as the load's chunks are.
fn corpus(opts: &Opts) -> Result<Vec<Vec<u8>>, String> {
    let block = opts.block.max(4096);
    let wanted = (opts.corpus_mb.max(1) * 1024 * 1024 / block).max(1);
    let mut chunks = Vec::with_capacity(wanted);
    for (index, path) in (0..wanted).zip(opts.files.iter().cycle()) {
        let mut file =
            std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Each chunk from its own place, so they are not one stream again.
        let at = index as u64 * block as u64;
        if at + block as u64 > file_len(path) {
            break;
        }
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(at))
            .map_err(|e| e.to_string())?;
        let mut raw = vec![0u8; block];
        std::io::Read::read_exact(&mut file, &mut raw).map_err(|e| e.to_string())?;
        let mut packed = vec![0u8; zlib_rs::compress_bound(block)];
        let (packed, code) =
            zlib_rs::compress_slice(&mut packed, &raw, zlib_rs::DeflateConfig::default());
        if code != zlib_rs::ReturnCode::Ok {
            return Err(format!("deflating {block} bytes: {code:?}"));
        }
        chunks.push(packed.to_vec());
    }
    if chunks.is_empty() {
        return Err("no corpus: the files are smaller than one request".to_owned());
    }
    Ok(chunks)
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// One worker: its own file, its own slice of it, requests until the
/// deadline.
fn worker(id: usize, opts: &Opts, corpus: Option<&[Vec<u8>]>, deadline: Instant) -> Part {
    let mut part = Part::default();
    let path = &opts.files[id % opts.files.len()];
    let file = match bench::File::open(&path.to_string_lossy()) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("{e}");
            return part;
        }
    };
    let len = file_len(path);
    let block = opts.block.min(len as usize).max(1);
    // Each worker walks its own quarter of the file, so the workers do not
    // read the same place at once (the page cache still serves them).
    let span = len / opts.workers.max(1) as u64;
    let mut offset = span * id as u64;
    let mut out = vec![0u8; block];
    let mut pages = Vec::new();
    for _ in 0..if opts.mode.pages() { opts.tiles.max(1) } else { 0 } {
        match Pages::new(block.div_ceil(64 * 1024)) {
            Ok(pages_at) => pages.push(pages_at),
            Err(e) => {
                eprintln!("{e}");
                return part;
            }
        }
    }
    let mut index = id;
    while Instant::now() < deadline {
        let started = Instant::now();
        let slot = index % pages.len().max(1);
        let tile = pages.get_mut(slot);
        let result = match (tile, corpus) {
            (Some(pages), None) => bench::request(
                Source::File {
                    file: &file,
                    offset,
                },
                block,
                false,
                block,
                Target::Pages(pages.numbers()),
            ),
            (Some(pages), Some(chunks)) => bench::request(
                Source::Memory(&chunks[index % chunks.len()]),
                chunks[index % chunks.len()].len(),
                true,
                block,
                Target::Pages(pages.numbers()),
            ),
            (None, None) => bench::request(
                Source::File {
                    file: &file,
                    offset,
                },
                block,
                false,
                block,
                Target::Address(&mut out),
            ),
            (None, Some(chunks)) => bench::request(
                Source::Memory(&chunks[index % chunks.len()]),
                chunks[index % chunks.len()].len(),
                true,
                block,
                Target::Address(&mut out),
            ),
        };
        if let Err(e) = result {
            eprintln!("{}: offset {offset}: {e}", path.display());
            break;
        }
        part.done(started.elapsed().as_secs_f64());
        part.bytes += block as u64;
        index += 1;
        offset += block as u64;
        if offset + block as u64 >= len {
            offset = span * id as u64;
        }
    }
    part
}
