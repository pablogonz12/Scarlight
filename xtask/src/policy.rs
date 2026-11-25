//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const MAX_ENCODED_RUN: usize = 512;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
    "env", "py",
];
/// Two-part extensions, matched as a suffix of the name including the dot
/// (`x.desktop.in` is not an `.in` file we can judge by extension alone, and
/// `notdesktop.in` must stay refused).
const ALLOWED_NAME_SUFFIXES: &[&str] = &[".desktop.in"];
/// Extensionless files are accepted only here, and only when they are scripts:
/// a `#!` first line is a text file we wrote, not a blob.
const SCRIPT_DIR_PREFIX: &str = "tools/";
const SCRIPT_SHEBANG: &[u8] = b"#!";
/// The one extensionless data file under `tools/`: the GE-Proton release a
/// Scarlight release is cut from, one `GE-Proton<major>-<release>` line. The
/// packager reads it for the version, the entry script for the Proton to use.
const BASE_FILE: &str = "tools/steam-compat/BASE";
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Files whose type is not on the allowlist but which still get every other
/// check (size, forbidden signatures, NUL bytes, encoded-data runs).
const EXTENSION_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "docs/brand/logo.svg",
        "the project logo: a hand-written vector drawing of our own, text only",
    ),
    (
        "docs/brand/logo-mark.svg",
        "the project mark: a hand-written vector drawing of our own, text only",
    ),
    (
        "docs/brand/divider.svg",
        "the README section divider: a hand-written vector drawing of our own, text only",
    ),
    (
        "NOTICE",
        "third-party notices for the ports we carry: plain text, no extension to judge",
    ),
];

/// Path prefixes holding our own terminal recordings: screencast GIFs of our
/// tools, made on our machines with a throwaway profile. A GIF is binary by
/// construction, so files here skip the type allowlist and the text/NUL check;
/// the size limit and the forbidden signatures still apply, and so does every
/// other file in the tree. See docs/asset-policy.md.
const RECORDING_PREFIXES: &[&str] = &["docs/img/"];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    for path in &files {
        if EXCEPTIONS.iter().any(|(p, _)| p == path) {
            continue;
        }
        let content = if staged {
            let out = Command::new("git")
                .args(["show", &format!(":{path}")])
                .current_dir(root)
                .output()?;
            if !out.status.success() {
                continue; // deleted in the index
            }
            out.stdout
        } else {
            match std::fs::read(root.join(path)) {
                Ok(c) => c,
                Err(_) => continue,
            }
        };
        for problem in check_with(path, &content, policy_flags(path)) {
            violations.push(format!("{path}: {problem}"));
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
                .collect(),
        ))
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// All problems with one file. Pure, for testing.
#[cfg(test)]
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    check_with(path, content, policy_flags(path))
}

/// The two escapes a path is granted: skip the type allowlist, and (recordings
/// only) allow binary content.
fn policy_flags(path: &str) -> (bool, bool) {
    let recording = RECORDING_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(*prefix));
    let skip_extension = recording || EXTENSION_EXCEPTIONS.iter().any(|(p, _)| *p == path);
    (skip_extension, recording)
}

/// `check`, optionally skipping only the file-type allowlist, and (for our own
/// recordings) the text/NUL-byte check.
fn check_with(path: &str, content: &[u8], flags: (bool, bool)) -> Vec<String> {
    let (skip_extension, binary_ok) = flags;
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let allowed = ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e))
        || ALLOWED_NAME_SUFFIXES.iter().any(|s| name.ends_with(s))
        || (ext.is_none() && (is_tool_script(path, content) || is_tool_base_file(path, content)));
    if !allowed && !skip_extension {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        problems.push(format!(
            "{} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.len()
        ));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) && !binary_ok {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if !binary_ok
        && let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    }
    problems
}

/// An extensionless file is allowed only under `tools/` and only when it starts
/// with a `#!` line: the launcher scripts we ship there are text, but the name
/// carries no extension the allowlist could judge.
fn is_tool_script(path: &str, content: &[u8]) -> bool {
    path.starts_with(SCRIPT_DIR_PREFIX) && content.starts_with(SCRIPT_SHEBANG)
}

/// `tools/steam-compat/BASE`: exactly one line, `GE-Proton<digits>-<digits>` and
/// nothing else, so it can never become a place to hide data.
fn is_tool_base_file(path: &str, content: &[u8]) -> bool {
    if path != BASE_FILE {
        return false;
    }
    let line = content.strip_suffix(b"\n").unwrap_or(content);
    let Some(rest) = line.strip_prefix(b"GE-Proton") else {
        return false;
    };
    let mut parts = rest.split(|b| *b == b'-');
    let (Some(major), Some(release), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |part: &[u8]| !part.is_empty() && part.iter().all(|b| b.is_ascii_digit());
    digits(major) && digits(release)
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

#[cfg(test)]
mod tests {
    use super::{check, check_with};

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    #[test]
    fn accepts_our_recordings() {
        // A minimal GIF: header, logical screen, one pixel, trailer.
        let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";
        assert!(check("docs/img/launcher.gif", gif).is_empty());
        assert!(
            !check("assets/launcher.gif", gif).is_empty(),
            "outside docs/img/ a GIF is still refused"
        );
        assert!(
            !check("docs/img/launcher.gif", b"MZ\x90\0").is_empty(),
            "magic still wins over the recording exemption"
        );
        assert!(
            !check("docs/img/launcher.gif", &vec![b'a'; 600 * 1024]).is_empty(),
            "recordings are still size-limited"
        );
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }

    #[test]
    fn accepts_tool_scripts() {
        assert!(
            check(
                "tools/scarlight-launcher/scarlight",
                b"#!/usr/bin/env bash\n"
            )
            .is_empty()
        );
        assert!(
            !check("tools/scarlight-launcher/scarlight", b"no shebang here\n").is_empty(),
            "an extensionless file under tools/ must be a script"
        );
        assert!(
            check("tools/steam-compat/BASE", b"GE-Proton11-14\n").is_empty(),
            "the BASE file is the one extensionless data file we ship"
        );
        assert!(!check("tools/steam-compat/BASE", b"GE-Proton11\n").is_empty());
        assert!(!check("tools/steam-compat/BASE", b"GE-Proton11-14-\n").is_empty());
        assert!(!check("tools/other/BASE", b"GE-Proton11-14\n").is_empty());
        assert!(
            !check("scripts/thing", b"#!/bin/sh\n").is_empty(),
            "extensionless scripts outside tools/ stay refused"
        );
    }

    #[test]
    fn accepts_env_profiles_and_desktop_templates() {
        assert!(check("tools/x/examples/profile.env", b"PRESET='play'\n").is_empty());
        assert!(check("tools/x/scarlight.desktop.in", b"[Desktop Entry]\n").is_empty());
        assert!(
            !check("tools/x/notdesktop.in", b"[Desktop Entry]\n").is_empty(),
            "the dot is part of the suffix"
        );
        assert!(
            !check("tools/x/scarlight.desktop.in", b"MZ\x90\0").is_empty(),
            "magic still wins"
        );
    }

    #[test]
    fn extension_exception_still_runs_other_checks() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert!(check_with("docs/brand/logo.svg", svg, (true, false)).is_empty());
        let embedded = format!(
            "<svg><image href=\"data:image/png;base64,{}\"/></svg>",
            "A1".repeat(400)
        );
        assert!(!check_with("docs/brand/logo.svg", embedded.as_bytes(), (true, false)).is_empty());
        assert!(
            !check_with(
                "docs/brand/logo.svg",
                &vec![b'a'; 600 * 1024],
                (true, false)
            )
            .is_empty()
        );
        assert!(!check_with("docs/brand/logo.svg", b"MZ\x90\0", (true, false)).is_empty());
    }
}
