//! The debug tool's control folder: files dropped into `VICTORY_CTL_DIR`
//! (default `<exe dir>/ctl`) that the running game acts on.
//!
//! * `shot` or `shot <label>`: save the next presented image.
//! * `key <button> [ms]`: hold a pad button that long (default 150 ms).
//! * `keys "<sequence>"`: steps separated by `;`, each `<buttons> [ms]` (a
//!   leading `wait <ms>` is a pause in the sequence).
//!
//! Buttons are named after the pad's stand-in keyboard table, so a request
//! works without the window having focus. The folder is looked at once per
//! present, at most every [`POLL_EVERY`]; with nothing requested that is a
//! handful of `stat` calls.
//!
//! Only a diagnostics build of the game reads the folder; everywhere else
//! the parsing and the held keys are kept (and tested).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the folder is read, at most.
pub const POLL_EVERY: Duration = Duration::from_millis(250);

/// How often `switches` is read, at most: a run changes its switches at
/// phase boundaries, seconds apart, and a frame is milliseconds.
pub const SWITCH_EVERY: Duration = Duration::from_secs(1);

/// How long a key is held when the request names no time.
pub const DEFAULT_HOLD_MS: u64 = 150;

/// A key that stays down until `end`.
#[derive(Clone, Copy)]
struct Held {
    key: u8,
    start: Instant,
    end: Instant,
}

/// The keys requested from the folder and not released yet.
static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

/// The folder requests arrive in: `VICTORY_CTL_DIR`, else `ctl` next to the
/// title's executable (which is what `play.sh` and `vic-run.sh` set it to).
pub fn dir() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::var_os("VICTORY_CTL_DIR") {
            return Some(PathBuf::from(dir));
        }
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("ctl"))
    })
    .as_deref()
}

/// What one request file asks for.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// Save the next presented image, under this label.
    Shot(Option<String>),
    /// Hold these keys together, in this order, `ms` each (empty keys: wait).
    Keys(Vec<Step>),
}

/// One step of a `keys` request: `keys` down from its start, for `ms`.
#[derive(Debug, PartialEq)]
pub struct Step {
    pub keys: Vec<u8>,
    pub ms: u64,
}

/// The request `text` (the whole content of a request file) asks for, or
/// `None` when it is not one.
pub fn parse(text: &str) -> Option<Request> {
    let text = text.trim().trim_matches('"').trim();
    let (verb, rest) = match text.split_once(|c: char| c.is_whitespace()) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (text, ""),
    };
    match verb {
        "shot" => Some(Request::Shot(Some(rest).filter(|r| !r.is_empty()).map(label))),
        "key" => Some(Request::Keys(vec![group(rest)?])),
        "keys" => {
            let steps: Vec<Step> = rest
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(group)
                .collect::<Option<_>>()?;
            (!steps.is_empty()).then_some(Request::Keys(steps))
        }
        _ => None,
    }
}

/// A step: `<buttons> [ms]`, `wait <ms>`, or a bare `<buttons>` (held for
/// [`DEFAULT_HOLD_MS`]).
fn group(text: &str) -> Option<Step> {
    let text = text.trim().trim_matches('"').trim();
    let (words, ms) = match text.rsplit_once(|c: char| c.is_whitespace()) {
        Some((words, last)) if last.parse::<u64>().is_ok() => (words.trim(), last.parse().ok()?),
        _ => (text, DEFAULT_HOLD_MS),
    };
    if words == "wait" {
        return Some(Step { keys: Vec::new(), ms });
    }
    let keys: Vec<u8> = words
        .split(['+', ','])
        .flat_map(str::split_whitespace)
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(key)
        .collect::<Option<_>>()?;
    (!keys.is_empty()).then_some(Step { keys, ms })
}

/// A label as a file name part: letters, digits, `-` and `_` kept.
fn label(text: &str) -> String {
    let kept: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let kept = kept.trim_matches('-').to_owned();
    if kept.is_empty() { "shot".to_owned() } else { kept }
}

/// The virtual key a button name stands for (the stand-in pad's table maps
/// them: Enter is A, Backspace is B, Q/E the shoulders, Z/C the triggers,
/// F/H the stick clicks, arrows the d-pad), or `None` for an unknown name.
pub fn key(name: &str) -> Option<u8> {
    let name = name.trim();
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "a" | "enter" => 0x0D,
        "b" | "back" | "backspace" => 0x08,
        "x" => b'X',
        "y" => b'Y',
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "lb" => b'Q',
        "rb" => b'E',
        "lt" | "z" => b'Z',
        "rt" | "c" => b'C',
        "ls" | "f" => b'F',
        "rs" | "h" => b'H',
        "menu" | "esc" | "escape" => 0x1B,
        "view" | "tab" => 0x09,
        "space" => 0x20,
        // The sticks, as the letters the stand-in pad reads them as.
        "lx+" | "d" => b'D',
        "lx-" => b'A',
        "ly+" | "w" => b'W',
        "ly-" | "s" => b'S',
        "rx+" | "l" => b'L',
        "rx-" | "j" => b'J',
        "ry+" | "i" => b'I',
        "ry-" | "k" => b'K',
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // Any other single letter or digit, as its own virtual key.
    let bytes = name.as_bytes();
    (bytes.len() == 1 && bytes[0].is_ascii_alphanumeric())
        .then(|| bytes[0].to_ascii_uppercase())
}

/// Holds what `request` asks for, from `now`.
fn schedule(request: &Request, now: Instant) {
    let Request::Keys(steps) = request else { return };
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let mut at = now;
    for step in steps {
        let end = at + Duration::from_millis(step.ms);
        held.extend(step.keys.iter().map(|&key| Held { key, start: at, end }));
        at = end;
    }
}

/// The keys a request holds down now.
pub fn held() -> Vec<u8> {
    let now = Instant::now();
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    held.retain(|h| h.end > now);
    let mut keys: Vec<u8> = held.iter().filter(|h| h.start <= now).map(|h| h.key).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// What the folder asked for since the last look, if anything.
pub fn tick() -> Option<Request> {
    let dir = dir()?;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match *last {
            Some(at) if now.duration_since(at) < POLL_EVERY => return None,
            _ => *last = Some(now),
        }
    }
    // A shot first: a request that saves a picture should not wait for keys.
    for name in ["shot", "key", "keys"] {
        let path = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let request = parse(&text);
        // Removed whatever it said: a file that does not parse is a bug in
        // the caller, not a race (callers write then rename).
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, path = %path.display(), "request not removed"),
        }
        match request {
            Some(request) => {
                tracing::info!(?request, path = %path.display(), "control request");
                if let Request::Keys(_) = request {
                    schedule(&request, Instant::now());
                }
                return Some(request);
            }
            None => tracing::warn!(path = %path.display(), "control request not understood"),
        }
    }
    None
}

/// The text of `<ctl>/switches`, the switch settings a running game takes at
/// a frame boundary (`crates/victory-gpu/src/switches.rs`): read at most once
/// a [`SWITCH_EVERY`], and `None` when it is not time to look again.
///
/// `Some("")` is the file absent or empty: the start-up settings, which is
/// what a `base` phase of `vic-run.sh` writes. The file is read whole and
/// written whole by its writer (a temp file and `mv`), so a reader never
/// sees half of one.
pub fn switches_text() -> Option<String> {
    let dir = dir()?;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match *last {
            Some(at) if now.duration_since(at) < SWITCH_EVERY => return None,
            _ => *last = Some(now),
        }
    }
    Some(std::fs::read_to_string(dir.join("switches")).unwrap_or_default())
}

/// The UTC stamp a save is named after: `YYYYMMDD-HHMMSS`.
pub fn stamp() -> String {
    stamp_of(SystemTime::now())
}

fn stamp_of(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The civil date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shot_names_itself() {
        assert_eq!(parse("shot"), Some(Request::Shot(None)));
        assert_eq!(parse("  shot  \n"), Some(Request::Shot(None)));
        assert_eq!(
            parse("shot world after the cutscene"),
            Some(Request::Shot(Some("world-after-the-cutscene".into())))
        );
    }

    #[test]
    fn a_key_holds_a_button_or_several() {
        let one = Some(Request::Keys(vec![Step { keys: vec![0x0D], ms: DEFAULT_HOLD_MS }]));
        assert_eq!(parse("key a"), one);
        assert_eq!(parse("key A 400"), Some(Request::Keys(vec![Step { keys: vec![0x0D], ms: 400 }])));
        assert_eq!(
            parse("key lb+rt 800"),
            Some(Request::Keys(vec![Step { keys: vec![b'Q', b'C'], ms: 800 }]))
        );
        assert_eq!(parse("key nope 100"), None);
        assert_eq!(parse("key"), None);
    }

    #[test]
    fn a_sequence_keeps_its_pauses() {
        assert_eq!(
            parse("keys \"a 200; wait 300; down 500\""),
            Some(Request::Keys(vec![
                Step { keys: vec![0x0D], ms: 200 },
                Step { keys: Vec::new(), ms: 300 },
                Step { keys: vec![0x28], ms: 500 },
            ]))
        );
    }

    #[test]
    fn buttons_are_the_stand_in_pads_keys() {
        assert_eq!(key("a"), Some(0x0D));
        assert_eq!(key("Menu"), Some(0x1B));
        assert_eq!(key("lx-"), Some(b'A'));
        assert_eq!(key("r"), Some(b'R'));
        assert_eq!(key("7"), Some(b'7'));
        assert_eq!(key("nothing"), None);
    }

    #[test]
    fn a_request_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: this is the only test that names a control folder.
        unsafe { std::env::set_var("VICTORY_CTL_DIR", dir.path()) };
        std::fs::write(dir.path().join("shot"), "shot menu\n").unwrap();
        assert_eq!(tick(), Some(Request::Shot(Some("menu".to_owned()))));
        assert!(!dir.path().join("shot").exists());
        // Throttled to one look per 250 ms, and the folder is now empty.
        assert_eq!(tick(), None);
    }

    #[test]
    fn a_key_is_held_while_it_lasts() {
        let now = Instant::now();
        schedule(
            &Request::Keys(vec![Step { keys: vec![b'Q'], ms: 400 }]),
            now,
        );
        assert!(held().contains(&b'Q'));
    }

    #[test]
    fn a_stamp_is_utc_and_sortable() {
        let at = UNIX_EPOCH + Duration::from_secs(1_763_910_000);
        assert_eq!(stamp_of(at), "20251123-150000");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_696), (2026, 8, 31));
    }
}
