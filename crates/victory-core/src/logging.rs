//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! `VICTORY_LOG_FILE=<path>` writes the log to that file (appended) instead
//! of stderr: under Wine or Proton a program started by Steam has no
//! terminal, and its stderr goes nowhere. A Linux path works there as
//! `Z:\home\...`. If the file cannot be opened, the log goes to stderr.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

/// The log file named by `VICTORY_LOG_FILE`, opened to append; `None` (and a
/// line on stderr, which is still a terminal here) if it cannot be opened, so
/// the log stays on stderr rather than disappearing.
fn open_log_file(path: &std::ffi::OsStr) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| eprintln!("VICTORY_LOG_FILE {}: {e}", path.to_string_lossy()))
        .ok()
}

pub fn init(default_level: &str) {
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));
    let file = std::env::var_os("VICTORY_LOG_FILE").and_then(|path| open_log_file(&path));
    let registry = tracing_subscriber::registry().with(CAP).with(filter);
    let _ = match file {
        Some(file) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file)),
            )
            .try_init(),
        None => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(std::io::stderr),
            )
            .try_init(),
    };
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::open_log_file;

    /// A second run of the program must not lose the first one's log: the
    /// file is appended to, not truncated.
    #[test]
    fn log_file_is_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victory.log");
        open_log_file(path.as_os_str())
            .unwrap()
            .write_all(b"first\n")
            .unwrap();
        open_log_file(path.as_os_str())
            .unwrap()
            .write_all(b"second\n")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    /// A path that cannot be opened gives no file, so the log stays on stderr.
    #[test]
    fn unusable_log_file_falls_back_to_stderr() {
        let dir = tempfile::tempdir().unwrap();
        // A directory: opening it for writing fails on every platform.
        assert!(open_log_file(dir.path().as_os_str()).is_none());
    }
}
