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
    bytes: Option<usize>,
}
type Buffer = Arc<Mutex<Vec<Event>>>;
static BUFFERS: Mutex<Vec<Buffer>> = Mutex::new(Vec::new());
static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static FRAME: AtomicU64 = AtomicU64::new(1);
static START: OnceLock<Instant> = OnceLock::new();
static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
static FIRST: AtomicU64 = AtomicU64::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);
static FAULT_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WORLD: Mutex<World> = Mutex::new(World { previous: None, slow: 0 });

#[cfg(windows)]
struct FaultEvent {
    ready: std::sync::atomic::AtomicBool,
    frame: AtomicU64,
    tid: AtomicU64,
    start: AtomicU64,
    duration: AtomicU64,
}

#[cfg(windows)]
static FAULTS: [FaultEvent; 65536] = [const { FaultEvent {
    ready: std::sync::atomic::AtomicBool::new(false),
    frame: AtomicU64::new(0), tid: AtomicU64::new(0),
    start: AtomicU64::new(0), duration: AtomicU64::new(0),
} }; 65536];
#[cfg(windows)]
static FAULT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Exception-path recording uses no trace mutex, TLS initialization or allocation.
#[cfg(windows)]
pub(crate) struct FaultSpan {
    start: Option<Instant>,
    frame: u64,
}

#[cfg(windows)]
pub(crate) fn fault_span() -> FaultSpan {
    FaultSpan {
        start: FAULT_ACTIVE.load(Ordering::Acquire).then(Instant::now),
        frame: FRAME.load(Ordering::Relaxed),
    }
}

#[cfg(windows)]
impl Drop for FaultSpan {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        let index = FAULT_COUNT.fetch_add(1, Ordering::Relaxed) as usize;
        let Some(event) = FAULTS.get(index) else { return };
        let Some(origin) = START.get() else { return };
        event.frame.store(self.frame, Ordering::Relaxed);
        event.tid.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as u64, Ordering::Relaxed);
        event.start.store(start.duration_since(*origin).as_micros() as u64, Ordering::Relaxed);
        event.duration.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        event.ready.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
enum Config {
    Range(u64, u64),
    World(u64),
}

struct World {
    previous: Option<Instant>,
    slow: u32,
}

impl World {
    fn observe(&mut self, frame: u64, elapsed: std::time::Duration, count: u64) -> Option<(u64, u64)> {
        self.slow = if elapsed > std::time::Duration::from_millis(200) {
            self.slow.saturating_add(1)
        } else {
            0
        };
        (self.slow == 20).then(|| (frame.saturating_add(1), frame.saturating_add(count)))
    }
}

thread_local! {
    static BUFFER: Buffer = {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).push(buffer.clone());
        buffer
    };
}

fn parse(value: &str) -> Option<Config> {
    if let Some(n) = value.strip_prefix("world:") {
        let n = n.parse().ok()?;
        return (n > 0).then_some(Config::World(n));
    }
    let (first, last) = value.split_once('-')?;
    let (first, last) = (first.parse().ok()?, last.parse().ok()?);
    (first <= last).then_some(Config::Range(first, last))
}

fn config() -> Option<Config> {
    *CONFIG.get_or_init(|| parse(&std::env::var("VICTORY_TRACE_FRAMES").ok()?))
}

fn range() -> Option<(u64, u64)> {
    match config()? {
        Config::Range(first, last) => Some((first, last)),
        Config::World(_) => {
            let first = FIRST.load(Ordering::Acquire);
            (first != 0).then(|| (first, LAST.load(Ordering::Relaxed)))
        }
    }
}

pub(crate) struct Span {
    name: &'static str,
    frame: u64,
    start: Option<Instant>,
    bytes: Option<usize>,
}

pub(crate) fn span(name: &'static str) -> Span {
    span_bytes(name, None)
}

pub(crate) fn span_bytes(name: &'static str, bytes: impl Into<Option<usize>>) -> Span {
    let frame = FRAME.load(Ordering::Relaxed);
    let enabled = range().is_some_and(|(first, last)| (first..=last).contains(&frame));
    if enabled {
        START.get_or_init(Instant::now);
    }
    Span {
        name,
        frame,
        start: enabled.then(Instant::now),
        bytes: bytes.into(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        let thread = std::thread::current();
        let event = Event {
            name: self.name,
            frame: self.frame,
            tid: thread_id(&thread),
            thread: thread.name().unwrap_or("unnamed").to_owned(),
            start: start.duration_since(*START.get().unwrap()).as_micros() as u64,
            duration: start.elapsed().as_micros() as u64,
            bytes: self.bytes,
        };
        BUFFER.with(|buffer| buffer.lock().unwrap_or_else(|e| e.into_inner()).push(event));
    }
}

fn thread_id(thread: &std::thread::Thread) -> String {
    #[cfg(windows)]
    {
        let _ = thread;
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }.to_string()
    }
    #[cfg(not(windows))]
    format!("{:?}", thread.id())
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
            "{{\"name\":{},\"ph\":\"X\",\"ts\":{},\"dur\":{},\"pid\":1,\"tid\":{},\"args\":{{\"frame\":{},\"thread\":{},\"bytes\":{}}}}}",
            quote(e.name),
            e.start,
            e.duration,
            quote(&e.tid),
            e.frame,
            quote(&e.thread),
            e.bytes.map_or_else(|| "null".to_string(), |n| n.to_string())
        )?;
    }
    write!(out, "]}}")
}

pub(crate) fn frame_end(frame: u64) {
    FRAME.store(frame.saturating_add(1), Ordering::Relaxed);
    if let Some(Config::World(count)) = config() {
        let now = Instant::now();
        let mut state = WORLD.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state.previous.replace(now);
        if FIRST.load(Ordering::Relaxed) == 0
            && let Some(previous) = previous
            && let Some((first, last)) = state.observe(frame, now.duration_since(previous), count)
        {
            LAST.store(last, Ordering::Relaxed);
            FIRST.store(first, Ordering::Release);
        }
    }
    let next = frame.saturating_add(1);
    let fault_active = range().is_some_and(|(first, last)| (first..=last).contains(&next));
    if fault_active {
        START.get_or_init(Instant::now);
    }
    FAULT_ACTIVE.store(fault_active, Ordering::Release);
    let Some((first, last)) = range() else { return };
    if frame < first || frame > last {
        return;
    }
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    for buffer in BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        events.append(&mut buffer.lock().unwrap_or_else(|e| e.into_inner()));
    }
    if frame == last {
        #[cfg(windows)]
        for fault in &FAULTS {
            if fault.ready.load(Ordering::Acquire) {
                let fault_frame = fault.frame.load(Ordering::Relaxed);
                if !(first..=last).contains(&fault_frame) {
                    continue;
                }
                events.push(Event {
                    name: "guard fault handler",
                    frame: fault_frame,
                    tid: fault.tid.load(Ordering::Relaxed).to_string(),
                    thread: "faulting thread".into(),
                    start: fault.start.load(Ordering::Relaxed),
                    duration: fault.duration.load(Ordering::Relaxed),
                    bytes: None,
                });
            }
        }
        #[cfg(windows)]
        if FAULT_COUNT.load(Ordering::Relaxed) > FAULTS.len() as u64 {
            tracing::warn!("frame trace fault-event capacity exceeded");
        }
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
            bytes: Some(64),
        }];
        let mut output = Vec::new();
        write_json(&mut output, &events).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"name\":\"a\\\"b\""));
        assert!(output.contains("\"frame\":3"));
        assert!(output.contains("\"thread\":\"test\\u000athread\""));
        assert!(output.contains("\"bytes\":64"));
    }

    #[test]
    fn world_trigger_needs_twenty_consecutive_slow_frames() {
        let mut world = World { previous: None, slow: 0 };
        for frame in 1..20 {
            assert!(world.observe(frame, std::time::Duration::from_millis(201), 5).is_none());
        }
        assert!(world.observe(20, std::time::Duration::from_millis(200), 5).is_none());
        for frame in 21..40 {
            assert!(world.observe(frame, std::time::Duration::from_millis(201), 5).is_none());
        }
        assert_eq!(world.observe(40, std::time::Duration::from_millis(201), 5), Some((41, 45)));
        assert!(matches!(parse("10-20"), Some(Config::Range(10, 20))));
        assert!(matches!(parse("world:5"), Some(Config::World(5))));
        assert!(parse("world:0").is_none());
    }
}
