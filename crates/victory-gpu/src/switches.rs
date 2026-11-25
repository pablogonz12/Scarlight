//! Switches that may be changed while the game runs.
//!
//! Every speed switch used to be read once, into a `OnceLock`, the first time
//! its site ran; an A/B was therefore two game runs (2-3.5 min of boot and
//! world load each). Here a small set of them is re-read from
//! `<ctl>/switches` (the debug tool's control folder, `VICTORY_CTL_DIR`), at
//! most once a second, and applied at a frame boundary ([`poll`] from
//! `queue::present`). A change is logged as
//! `switches changed: K=V ...` with the frame number, which is the phase
//! marker `xcl-bin/notes/phase-stats.py` splits a run on.
//!
//! What is safe live, and why:
//!
//! - `VICTORY_TEXTURE_WAIT_US` ([`crate::texture::wait_us`]) is a per-refresh
//!   budget: a change takes effect at the next refresh.
//! - `VICTORY_CARD_BUFFERS`, `VICTORY_CARD_BUFFERS_MIN_MB` and
//!   `VICTORY_CARD_BUFFERS_MB` ([`crate::shadow`]) decide what new copies go
//!   to the card; copies already there stay until they are let go, which is
//!   how the run behaves anyway while the budget fills.
//! - `VICTORY_CARD_REHASH_RUNS` ([`crate::host::rehash_runs`]) picks whether a
//!   compare pass hashes runs joined, and `VICTORY_CARD_HASH_SPAN`
//!   ([`crate::host::hash_span`]) the gap a pass bridges; a new pass takes the
//!   new values.
//! - `VICTORY_TEXTURE_IN_PLACE` and `VICTORY_TEXTURE_INPLACE_MAX_MB`
//!   ([`crate::texture`]) decide how the next refresh reads: in place under
//!   the remap lock, and up to what size. Both are asked per refresh.
//! - `VICTORY_LATE_WAITS` ([`crate::frame`]) picks how a wait sleeps.
//! - `VICTORY_RELAY`, `VICTORY_LATE_WAIT`, `VICTORY_ASYNC_WRITEBACK`,
//!   `VICTORY_QUEUE_WAIT` and `VICTORY_SPLIT_WAITS` change how submissions
//!   are held and landed, so they are only taken when nothing is in the air
//!   (`crate::host::flights_quiet`); until then the poll tries again a second
//!   later. The frame boundary the poll runs at has already settled what a
//!   present settles, and the write-back worker's jobs are waited for by the
//!   fence they belong to, so a switch turned off mid-run leaves the jobs it
//!   started to finish as they were started.
//!
//! Everything else read once at start-up (capture folders, dump lists,
//! diagnostics gates, the pipeline cache) stays a start-up switch.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

/// The switches that may be changed live, in the order they are logged.
const NAMES: [&str; 17] = [
    "VICTORY_ASYNC_WRITEBACK",
    "VICTORY_TEXTURE_WAIT_US",
    "VICTORY_TEXTURE_IN_PLACE",
    "VICTORY_TEXTURE_INPLACE_MAX_MB",
    "VICTORY_TEXTURE_KEEP",
    "VICTORY_CARD_BUFFERS",
    "VICTORY_CARD_BUFFERS_MIN_MB",
    "VICTORY_CARD_BUFFERS_MB",
    "VICTORY_CARD_REHASH_RUNS",
    "VICTORY_CARD_HASH_SPAN",
    "VICTORY_RELAY",
    "VICTORY_LATE_WAIT",
    "VICTORY_LATE_WAIT_LISTS",
    "VICTORY_LATE_WAIT_WRITES",
    "VICTORY_LATE_WAITS",
    "VICTORY_QUEUE_WAIT",
    "VICTORY_SPLIT_WAITS",
];

const N: usize = NAMES.len();

/// What each switch is set to now: -1 unset (the start-up value stands).
static VALUES: [AtomicI64; N] = [const { AtomicI64::new(-1) }; N];

/// What was applied, as (name, value), to tell a change from a repeat.
static APPLIED: Mutex<Vec<(String, i64)>> = Mutex::new(Vec::new());

fn index(name: &str) -> Option<usize> {
    NAMES.iter().position(|n| *n == name)
}

/// Whether a switch changes how submissions are held or landed, and so may
/// only be taken with nothing in the air.
fn needs_quiet(name: &str) -> bool {
    matches!(
        name,
        "VICTORY_ASYNC_WRITEBACK"
            | "VICTORY_RELAY"
            | "VICTORY_LATE_WAIT"
            | "VICTORY_QUEUE_WAIT"
            | "VICTORY_SPLIT_WAITS"
    )
}

/// The live value of a switch, if it is set.
fn live(name: &str) -> Option<i64> {
    let i = index(name)?;
    let v = VALUES[i].load(Ordering::Relaxed);
    (v >= 0).then_some(v)
}

/// A boolean switch: the live value if set, `base` otherwise.
pub(crate) fn b(name: &str, base: bool) -> bool {
    live(name).map_or(base, |v| v != 0)
}

/// A byte switch (0, 1, 2, 3): the live value if set, `base` otherwise.
pub(crate) fn u8v(name: &str, base: u8) -> u8 {
    live(name).map_or(base, |v| v.clamp(0, 255) as u8)
}

/// Whether a switch is set to `value` live (`None` when it is not set).
pub(crate) fn live_is(name: &str, value: i64) -> Option<bool> {
    live(name).map(|v| v == value)
}

/// The live value of a number switch, `None` when it is not set (a caller
/// with its own default to keep, such as `shadow::card_limits`).
pub(crate) fn live_value(name: &str) -> Option<u64> {
    live(name).map(|v| v.max(0) as u64)
}

/// A number switch: the live value if set, `base` otherwise.
pub(crate) fn u64v(name: &str, base: u64) -> u64 {
    live(name).map_or(base, |v| v.max(0) as u64)
}

/// Whether nothing is in the air, so that a switch may change how flights
/// are held and landed.
fn quiet() -> bool {
    #[cfg(windows)]
    {
        crate::host::flights_quiet()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Parses the file: one `K=V` a line, `#` comments, blank lines skipped.
/// `VICTORY_` is understood in front of a name or left off; a value of
/// `1`, `on`, `true` or `yes` is 1, `0`, `off`, `false` or `no` is 0, and
/// anything else is a number. Names that may not be changed live are
/// ignored (they are not in [`NAMES`]), as is a line without `=`.
fn parse(text: &str) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_uppercase();
        let name = if name.starts_with("VICTORY_") {
            name
        } else {
            format!("VICTORY_{name}")
        };
        if index(&name).is_none() {
            continue;
        }
        let value = value.trim().to_ascii_lowercase();
        let value = match value.as_str() {
            "1" | "on" | "true" | "yes" => 1,
            "0" | "off" | "false" | "no" | "" => 0,
            _ => match value.parse::<i64>() {
                Ok(v) => v.max(0),
                Err(_) => continue,
            },
        };
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => out.push((name, value)),
        }
    }
    // A stable order, so the same file always compares equal.
    out.sort_by_key(|(name, _)| index(name).unwrap_or(N));
    out
}

/// The one poll at a present: the debug tool's requests (shots, held pad
/// keys) and the switches, in that order, both through the control folder
/// `victory_core::ctl` owns — one resolver (`ctl::dir`) and one set of
/// throttles (`ctl::POLL_EVERY` for requests, `ctl::SWITCH_EVERY` here).
/// Called once per present; it applies the switches at most once a second,
/// and only when nothing is in the air for the ones that need it.
pub(crate) fn poll(frame: u64) {
    // A shot request is remembered for the frame's capture, key requests are
    // held now (both inside `capture::poll` -> `victory_core::ctl::tick`).
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::capture::poll();
    let Some(text) = victory_core::ctl::switches_text() else { return };
    let wanted = parse(&text);
    let mut applied = APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    if *applied == wanted {
        return;
    }
    let drained = wanted.iter().any(|(name, _)| needs_quiet(name))
        || applied
            .iter()
            .any(|(name, _)| needs_quiet(name) && !wanted.iter().any(|(n, _)| n == name));
    if drained && !quiet() {
        // Nothing is applied under a flight that would land another way:
        // the next poll, a second later, tries again.
        return;
    }
    let mut changes: Vec<String> = Vec::new();
    for (name, value) in &wanted {
        let before = applied.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
        if before != Some(*value) {
            changes.push(format!("{name}={value}"));
        }
    }
    for (name, _) in applied.iter() {
        if !wanted.iter().any(|(n, _)| n == name) {
            changes.push(format!("{name}=<start-up>"));
        }
    }
    for (name, value) in &wanted {
        if let Some(i) = index(name) {
            VALUES[i].store(*value, Ordering::Relaxed);
        }
    }
    for (name, _) in applied.iter() {
        if !wanted.iter().any(|(n, _)| n == name)
            && let Some(i) = index(name)
        {
            VALUES[i].store(-1, Ordering::Relaxed);
        }
    }
    *applied = wanted;
    if !changes.is_empty() {
        tracing::info!(frame, "switches changed: {}", changes.join(" "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_names_with_and_without_the_prefix() {
        let got = parse("ASYNC_WRITEBACK=1\nVICTORY_TEXTURE_WAIT_US=8000\n");
        assert_eq!(
            got,
            vec![
                ("VICTORY_ASYNC_WRITEBACK".to_string(), 1),
                ("VICTORY_TEXTURE_WAIT_US".to_string(), 8000)
            ]
        );
    }

    #[test]
    fn understands_words_and_comments() {
        let got = parse("# a comment\n\nCARD_BUFFERS=off\nRELAY=on\nQUEUE_WAIT=true\n");
        assert_eq!(
            got.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
    }

    #[test]
    fn ignores_unknown_names_bad_numbers_and_lines_without_equals() {
        let got = parse("SOMETHING_ELSE=1\nTEXTURE_WAIT_US=lots\nno equals here\nRELAY=2\n");
        assert_eq!(got, vec![("VICTORY_RELAY".to_string(), 2)]);
    }

    #[test]
    fn a_later_line_for_the_same_name_wins_and_empty_is_off() {
        let got = parse("LATE_WAIT=2\nLATE_WAIT=0\nQUEUE_WAIT=\n");
        assert_eq!(
            got,
            vec![
                ("VICTORY_LATE_WAIT".to_string(), 0),
                ("VICTORY_QUEUE_WAIT".to_string(), 0)
            ]
        );
    }

    #[test]
    fn only_the_ones_that_change_how_flights_land_need_quiet() {
        assert!(needs_quiet("VICTORY_RELAY"));
        assert!(needs_quiet("VICTORY_ASYNC_WRITEBACK"));
        assert!(!needs_quiet("VICTORY_TEXTURE_WAIT_US"));
        assert!(!needs_quiet("VICTORY_CARD_BUFFERS_MIN_MB"));
    }
}
