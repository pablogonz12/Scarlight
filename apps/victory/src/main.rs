//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
mod run;
mod shader;
mod translate;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader
        /// (translated for this host's wave sizes unless --host-lanes says).
        #[arg(long)]
        pipelines: bool,
        /// Translate for a host whose waves are MIN-MAX lanes (32-32 for
        /// NVIDIA, 32-64 for AMD RDNA): shaders that need another wave size
        /// are listed as failures. Default: any wave size.
        #[arg(long, value_name = "MIN-MAX", value_parser = parse_lanes)]
        host_lanes: Option<(u32, u32)>,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Print the identity the layer uses to pick a title profile (BLAKE3),
    /// and whether a profile matches it. Prints only numbers of this build.
    TitleId {
        /// The title's executable.
        exe: PathBuf,
    },
    /// Make the title pipelines of earlier runs that the pipeline library
    /// lacks, without starting the title (`run` does it at its start;
    /// `VICTORY_WARMUP=0` turns that off).
    Warmup,
    /// Show the resolved configuration and check it.
    Config,
    /// Bench the DirectStorage path over the title's own archive files
    /// (read-only, never written): MB/s, the stage split per request
    /// (queue, lock, read, inflate, dest) and worker utilisation, through
    /// the same `execute` a load's requests take. No title, no display, no
    /// Vulkan (loading.md).
    #[cfg(feature = "diagnostics")]
    DstorageBench {
        /// The archive files to read (the title's `.rpf`s). Never written.
        files: Vec<PathBuf>,
        /// `raw` (reads into an address), `page` (reads into 64 KiB
        /// physical pages, the title's destination form), `zlib` or
        /// `zpage` (the load's inflate into either).
        #[arg(long, default_value = "page")]
        mode: String,
        /// Workers, as the load's dstorage pool has them.
        #[arg(long, default_value_t = 16)]
        workers: usize,
        /// Seconds of work.
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Bytes per request.
        #[arg(long, default_value_t = 1 << 20)]
        block: usize,
        /// MiB of the files to deflate for `--mode zlib` and `zpage`.
        #[arg(long, default_value_t = 32)]
        corpus_mb: usize,
        /// Page destinations a worker rotates through: the load's requests
        /// land in many pages, not always the same ones.
        #[arg(long, default_value_t = 4)]
        tiles: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

/// `victory title-id <exe>`: the BLAKE3 a profile matches, and the profile
/// that matches it, if any. Run locally; do not paste the hash into a report.
fn title_id(exe: &std::path::Path) -> Result<()> {
    let data = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let hash = blake3::hash(&data);
    let cfg = victory_core::Config::load()?;
    println!("image          {}", exe.display());
    println!("blake3         {hash}");
    match cfg.title_profile(&hash.to_hex()) {
        Some(profile) => println!(
            "profile        {} ({} hooks)",
            profile
                .file()
                .map_or_else(|| "<inline>".to_owned(), |p| p.display().to_string()),
            profile.hooks.len()
        ),
        None => {
            let dirs: Vec<String> = cfg
                .profile_dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect();
            println!("profile        none (searched {})", dirs.join(", "));
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            host_lanes,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            // `VICTORY_HOST_WAVE=32`: translate as for a GPU whose waves are at most 32 lanes
            if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_host_max_wave(n);
            }
            translate::run(paths, validate, pipelines, list, jobs, host_lanes)
        }
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => run::run(&exe, unimplemented, strict, execute, console),
        Command::TitleId { exe } => title_id(&exe),
        Command::Warmup => run::warmup(),
        Command::Config => show_config(),
        #[cfg(feature = "diagnostics")]
        Command::DstorageBench {
            files,
            mode,
            workers,
            seconds,
            block,
            corpus_mb,
            tiles,
        } => {
            use victory_hle::dstorage_bench::{Mode, Opts};
            let mode = match mode.as_str() {
                "raw" => Mode::Raw,
                "page" => Mode::Page,
                "zlib" => Mode::Zlib,
                "zpage" => Mode::Zpage,
                other => anyhow::bail!("--mode is raw, page, zlib or zpage, not {other:?}"),
            };
            let opts = Opts {
                files,
                workers: workers.clamp(1, 256),
                seconds,
                block: block.clamp(4096, 16 << 20),
                mode,
                corpus_mb,
                tiles: tiles.clamp(1, 256),
            };
            let report = victory_hle::dstorage_bench::run(&opts)
                .map_err(|e| anyhow::anyhow!(e))
                .context("the dstorage bench")?;
            report.print();
            Ok(())
        }
    }
}

/// `MIN-MAX` lanes per wave, or one size `N`.
fn parse_lanes(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let n = |x: &str| {
        x.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| n.is_power_of_two() && (4..=128).contains(n))
            .ok_or_else(|| format!("{x:?} is not a wave size (4 to 128, a power of two)"))
    };
    let (a, b) = (n(a)?, n(b)?);
    if a > b {
        return Err(format!("{a} is more than {b}"));
    }
    Ok((a, b))
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}
