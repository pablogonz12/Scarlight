//! `cargo xtask test-wine`: the crates whose tests need Windows, built for
//! the Windows target and run under Wine.
//!
//! Some tests reach the host's D3D12 (creating a device, a resource, a
//! pipeline) or clean up a real image, so they only compile and pass on
//! Windows. On Linux the rest of the workspace runs with
//! `cargo test --workspace`; these run here, one command, threads pinned to
//! one: a host device per test thread either fails or stalls the driver.
//!
//! Wine is `$VICTORY_WINE`, else the newest GE-Proton under Steam's
//! `compatibilitytools.d`, else `wine` from the PATH. The prefix is
//! `$VICTORY_WINEPREFIX`, else `~/.local/share/scarlight-prefix`.
//!
//! Builds go through `cargo xwin`, which is a cross build like any other: one
//! at a time on a small machine (the farm's build-slot wrapper).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Tested under Wine unless `--package` says otherwise.
const DEFAULT_PACKAGES: [&str; 3] = ["victory-gpu", "victory-hle", "victory-loader"];

pub fn run(packages: &[String], integration: bool, filters: &[String]) -> Result<()> {
    let packages: Vec<&str> = if packages.is_empty() {
        DEFAULT_PACKAGES.to_vec()
    } else {
        packages.iter().map(String::as_str).collect()
    };
    let wine = wine_path()?;
    let prefix = prefix_path()?;
    println!("wine:   {}", wine.display());
    println!("prefix: {}", prefix.display());
    let mut failed = Vec::new();
    for package in &packages {
        let mut command = Command::new("cargo");
        command.args([
            "xwin",
            "test",
            "--no-run",
            "--target",
            "x86_64-pc-windows-msvc",
            "-p",
            package,
            "--message-format=json",
        ]);
        println!("\n> cargo {}", tail(&command));
        let output = command
            .output()
            .context("running cargo xwin test (is cargo-xwin installed?)")?;
        // The JSON goes to stdout, the compiler's own output to stderr: show
        // stderr only when the build fails.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            print!("{}", String::from_utf8_lossy(&output.stderr));
            bail!("`cargo xwin test --no-run -p {package}` failed");
        }
        let binaries = test_binaries(&stdout, integration);
        if binaries.is_empty() {
            println!("{package}: no test binaries");
            continue;
        }
        for binary in binaries {
            let mut run = Command::new(&wine);
            run.arg(&binary).arg("--test-threads=1").args(filters);
            run.env("WINEPREFIX", &prefix);
            println!("> wine {} --test-threads=1 {}", binary.display(), filters.join(" "));
            let status = run
                .status()
                .with_context(|| format!("running {}", binary.display()))?;
            if !status.success() {
                failed.push(binary);
            }
        }
    }
    if failed.is_empty() {
        println!("\nall Wine test binaries passed");
        return Ok(());
    }
    for binary in &failed {
        eprintln!("FAILED: {}", binary.display());
    }
    bail!("{} test binaries failed", failed.len());
}

/// The test executables in cargo's JSON messages (`"executable":null` for
/// the artifacts that have none). An integration test is left out unless
/// asked for: the loader's end-to-end test builds a fixture with `cargo`,
/// which under Wine would have to be a Windows one.
fn test_binaries(json: &str, integration: bool) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = json
        .lines()
        .filter(|line| integration || !line.contains("\"kind\":[\"test\"]"))
        .filter_map(|line| {
            let at = line.find("\"executable\":\"")?;
            let rest = &line[at + "\"executable\":\"".len()..];
            let end = rest.find('"')?;
            // A Windows path in a JSON string: `\\` is one backslash.
            Some(PathBuf::from(rest[..end].replace("\\\\", "\\")))
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The command line, for the log line that says what ran.
fn tail(command: &Command) -> String {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn wine_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("VICTORY_WINE") {
        return Ok(PathBuf::from(path));
    }
    let tools = home().join(".local/share/Steam/compatibilitytools.d");
    // The newest GE-Proton installed, if any: the same build a run uses.
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&tools)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("GE-Proton"))
        })
        .collect();
    candidates.sort();
    if let Some(dir) = candidates.pop() {
        // `<dir>/files/bin/wine`, or `<dir>/bin/wine` for the older layout.
        for relative in ["files/bin/wine", "bin/wine"] {
            let wine = dir.join(relative);
            if wine.is_file() {
                return Ok(wine);
            }
        }
    }
    // A system Wine, or nothing: say how to point at one.
    let found = Command::new("wine").arg("--version").output();
    if found.is_ok_and(|out| out.status.success()) {
        return Ok(PathBuf::from("wine"));
    }
    bail!(
        "no Wine found: install GE-Proton under ~/.local/share/Steam/compatibilitytools.d, \
         put `wine` on the PATH, or set VICTORY_WINE to one"
    )
}

fn prefix_path() -> Result<PathBuf> {
    match std::env::var_os("VICTORY_WINEPREFIX") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(home().join(".local/share/scarlight-prefix")),
    }
}

fn home() -> &'static Path {
    // Every platform we run this on sets it; a missing HOME is not worth a
    // fallback that silently writes somewhere else.
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    })
}
