//! Repository maintenance. Run with `cargo xtask <command>`.

mod policy;
mod wine;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Build the tests of the crates that need Windows and run them under
    /// Wine, one thread each.
    TestWine {
        /// Crate to test; repeatable. Defaults to victory-gpu, victory-hle
        /// and victory-loader.
        #[arg(short, long)]
        package: Vec<String>,
        /// Also run integration tests (end to end: one builds a fixture with
        /// `cargo`, which under Wine would have to be a Windows one).
        #[arg(long)]
        integration: bool,
        /// Filter (or any test binary argument), after `--`.
        #[arg(last = true)]
        filters: Vec<String>,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::TestWine {
            package,
            integration,
            filters,
        } => wine::run(&package, integration, &filters),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
