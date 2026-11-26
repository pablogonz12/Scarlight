//! What a translation's key must cover: every switch the sources read is
//! either in [`super::SWITCHES`] (and so changes [`super::settings_bytes`] and
//! the shader cache's key) or known to change nothing but the reporting.

use std::collections::BTreeSet;
use std::ffi::OsString;

use super::{DIAGNOSTIC_SWITCHES, SWITCHES, TEST_SWITCHES, settings_bytes_of};

/// `settings_bytes_of` with `name` set to `value`, every other switch unset.
fn with(name: &str, value: Option<&str>) -> Vec<u8> {
    settings_bytes_of(|asked| match asked == name {
        true => value.map(OsString::from),
        false => None,
    })
}

#[test]
fn setting_any_switch_changes_the_settings() {
    assert!(!SWITCHES.is_empty());
    let unset = settings_bytes_of(|_| None);
    for name in SWITCHES {
        let zero = with(name, Some("0"));
        let one = with(name, Some("1"));
        assert_ne!(unset, zero, "{name}=0 does not change the key");
        assert_ne!(unset, one, "{name}=1 does not change the key");
        assert_ne!(zero, one, "{name} keys 0 and 1 alike");
    }
}

#[test]
fn the_settings_are_the_same_for_the_same_switches() {
    let a = settings_bytes_of(|name| (name == "VICTORY_WAVE_WIDE").then(|| OsString::from("1")));
    let b = settings_bytes_of(|name| (name == "VICTORY_WAVE_WIDE").then(|| OsString::from("1")));
    assert_eq!(a, b);
    // Values are delimited: one switch's value cannot be read as another's.
    let swapped = settings_bytes_of(|name| match name {
        "VICTORY_WAVE_WIDE" => Some(OsString::from("0VICTORY_WAVE_LOCAL=1")),
        _ => None,
    });
    let two = settings_bytes_of(|name| match name {
        "VICTORY_WAVE_WIDE" => Some(OsString::from("0")),
        "VICTORY_WAVE_LOCAL" => Some(OsString::from("1")),
        _ => None,
    });
    assert_ne!(swapped, two);
}

#[test]
fn the_host_inputs_are_part_of_the_key() {
    // The limits and the wave size are process-wide; this only checks that
    // they are in the bytes at all, and in a place a change is visible.
    let before = super::device_bytes();
    let limit = super::view_heap_limit();
    super::set_view_heap_limit(limit.wrapping_add(1));
    let after = super::device_bytes();
    super::set_view_heap_limit(limit);
    assert_ne!(before, after);
    assert_eq!(before, super::device_bytes());
}

#[test]
fn every_listed_switch_is_listed_once() {
    let all: Vec<&&str> = SWITCHES
        .iter()
        .chain(DIAGNOSTIC_SWITCHES)
        .chain(TEST_SWITCHES)
        .collect();
    let unique: BTreeSet<&&str> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "a switch is in two lists");
}

/// The names the sources read. A new switch that is in no list fails this:
/// it would silently be left out of the cache's key.
#[test]
fn the_sources_read_only_known_switches() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(files.len() > 5, "no sources found under {}", root.display());
    let known: BTreeSet<&str> = SWITCHES
        .iter()
        .chain(DIAGNOSTIC_SWITCHES)
        .chain(TEST_SWITCHES)
        .copied()
        .collect();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for name in env_names(&text) {
            if !known.contains(name.as_str()) {
                unknown.insert(format!("{name} ({})", file.display()));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "these switches read by the sources are in none of SWITCHES, DIAGNOSTIC_SWITCHES or \
         TEST_SWITCHES: {unknown:?}"
    );
}

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `VICTORY_<NAME>` in `text`: uppercase letters, digits and underscores
/// after the prefix (`VICTORY_...` in prose names nothing and is skipped).
fn env_names(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find("VICTORY_") {
        rest = &rest[at + "VICTORY_".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        if end > 0 {
            out.insert(format!("VICTORY_{}", &rest[..end]));
        }
    }
    out
}
