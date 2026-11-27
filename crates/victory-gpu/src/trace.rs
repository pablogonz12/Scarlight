//! Opt-in, bounded-by-frame CPU timeline.
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

struct Event {
    name: &'static str,
    frame: u64,
    tid: String,
    thread: String,
    start: u64,
    duration: u64,
}
type Buffer = Arc<Mutex<Vec<Event>>>;
static BUFFERS: Mutex<Vec<Buffer>> = Mutex::new(Vec::new());
static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static FRAME: AtomicU64 = AtomicU64::new(1);
static START: OnceLock<Instant> = OnceLock::new();
static RANGE: OnceLock<Option<(u64, u64)>> = OnceLock::new();

thread_local! {
    static BUFFER: Buffer = {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).push(buffer.clone());
        buffer
    };
}

fn range() -> Option<(u64, u64)> {
    *RANGE.get_or_init(|| {
        let value = std::env::var("VICTORY_TRACE_FRAMES").ok()?;
        let (first, last) = value.split_once('-')?;
        let (first, last) = (first.parse().ok()?, last.parse().ok()?);
        (first <= last).then_some((first, last))
    })
}

pub(crate) struct Span {
    name: &'static str,
    frame: u64,
    start: Option<Instant>,
}

pub(crate) fn span(name: &'static str) -> Span {
    let frame = FRAME.load(Ordering::Relaxed);
    let enabled = range().is_some_and(|(first, last)| (first..=last).contains(&frame));
    if enabled {
        START.get_or_init(Instant::now);
    }
    Span {
        name,
        frame,
        start: enabled.then(Instant::now),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        let thread = std::thread::current();
        let event = Event {
            name: self.name,
            frame: self.frame,
            tid: format!("{:?}", thread.id()),
            thread: thread.name().unwrap_or("unnamed").to_owned(),
            start: start.duration_since(*START.get().unwrap()).as_micros() as u64,
            duration: start.elapsed().as_micros() as u64,
        };
        BUFFER.with(|buffer| buffer.lock().unwrap_or_else(|e| e.into_inner()).push(event));
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_json(mut out: impl Write, events: &[Event]) -> io::Result<()> {
    write!(out, "{{\"traceEvents\":[")?;
    for (i, e) in events.iter().enumerate() {
        if i != 0 {
            write!(out, ",")?;
        }
        write!(
            out,
            "{{\"name\":{},\"ph\":\"X\",\"ts\":{},\"dur\":{},\"pid\":1,\"tid\":{},\"args\":{{\"frame\":{},\"thread\":{}}}}}",
            quote(e.name),
            e.start,
            e.duration,
            quote(&e.tid),
            e.frame,
            quote(&e.thread)
        )?;
    }
    write!(out, "]}}")
}

pub(crate) fn frame_end(frame: u64) {
    FRAME.store(frame.saturating_add(1), Ordering::Relaxed);
    let Some((first, last)) = range() else { return };
    if frame < first || frame > last {
        return;
    }
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    for buffer in BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        events.append(&mut buffer.lock().unwrap_or_else(|e| e.into_inner()));
    }
    if frame == last {
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join("victory-frame-trace.json");
        match std::fs::File::create(&path).and_then(|file| write_json(file, &events)) {
            Ok(()) => tracing::info!(path = %path.display(), "frame trace written"),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "frame trace could not be written")
            }
        }
    }
}

pub(crate) struct FrameEnd(pub u64);
impl Drop for FrameEnd {
    fn drop(&mut self) {
        frame_end(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_escapes_and_writes_event() {
        let events = [Event {
            name: "a\"b",
            frame: 3,
            tid: "ThreadId(1)".into(),
            thread: "test\nthread".into(),
            start: 12,
            duration: 5,
        }];
        let mut output = Vec::new();
        write_json(&mut output, &events).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"name\":\"a\\\"b\""));
        assert!(output.contains("\"frame\":3"));
        assert!(output.contains("\"thread\":\"test\\u000athread\""));
    }
}
