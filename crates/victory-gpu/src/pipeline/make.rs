//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost.
//!
//! Passes over the screen (no vertex input, no depth; see
//! [`GraphicsState::screen_pass`]) have workers and a budget of their own:
//! the title fills its lighting tables with such draws once, at the world's
//! first frame, and behind a world's worth of pipelines they waited out the
//! frame's budget before a worker ever reached them (a run that lost them
//! had no diffuse light). Both kinds keep the same half a second, so a pass
//! classified on either side never waits longer than one budget allows.
//! Off with `VICTORY_SCREEN_PIPELINE_POOL=0` (every pipeline then goes to
//! the ordinary workers).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// A pass over the screen (see [`GraphicsState::screen_pass`]): made by
    /// the screen workers, out of the world's budget.
    screen: bool,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// Whether what it is made from went to the warm-up list.
    warmed: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            shaders,
            screen: screen_pool() && graphics.as_ref().is_some_and(|g| g.screen_pass()),
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            warmed: AtomicBool::new(false),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// What it is made from.
    pub(crate) fn parts(&self) -> (&[Arc<Shader>], Option<&GraphicsState>) {
        (&self.shaders, self.graphics.as_deref())
    }

    /// Makes it on this thread (the warm-up's workers); whether it has a
    /// host pipeline.
    pub(crate) fn make_here(&self) -> bool {
        self.make();
        self.made.get().is_some_and(|made| made.is_some())
    }

    fn make(&self) {
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen get their own workers and budget.
/// On by default; `VICTORY_SCREEN_PIPELINE_POOL=0` puts every pipeline back
/// in the ordinary pool, as before.
fn screen_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCREEN_PIPELINE_POOL").is_none_or(|v| v != "0"))
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);
/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    // One observation of it for both the drop report and the answer: a worker
    // may finish between two reads, and a finished pipeline is not dropped.
    let made = p.made.get();
    if made.is_none() {
        dropped(p);
    }
    let made = made?.as_ref();
    // What it is made from, for the next start's warm-up (only one that has
    // a host pipeline: the list names pipelines to build).
    if made.is_some() && !p.warmed.swap(true, Ordering::Relaxed) {
        super::warm::note(p);
    }
    made
}

/// Logs a draw or dispatch whose pipeline was not made within its kind of
/// the frame's budget, so it is skipped this frame: once per pipeline, as a
/// pass drawn only once is never drawn again.
fn dropped(p: &Pending) {
    static DROPPED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let first = DROPPED
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(p.key.clone());
    if first {
        tracing::warn!(
            key = %p.key,
            screen = p.screen,
            "a draw's pipeline is not made within the frame's budget; it is skipped this frame — a pass drawn only once is lost"
        );
    }
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// What one kind of a frame's budget has spent: the frame it belongs to, and
/// the time waited of it (or taken by a thread waiting now).
struct Spent {
    frame: u64,
    used: Duration,
}

impl Spent {
    const fn new() -> Spent {
        Spent {
            frame: u64::MAX,
            used: Duration::ZERO,
        }
    }

    /// What a thread may wait of `frame`'s `budget`: all that is left, taken
    /// now when `shared`, so no two threads wait the same half second.
    fn take(&mut self, frame: u64, budget: Duration, shared: bool) -> Duration {
        if self.frame != frame {
            *self = Spent {
                frame,
                used: Duration::ZERO,
            };
        }
        let left = budget.saturating_sub(self.used);
        if shared {
            self.used += left;
        }
        left
    }

    /// Gives back what a thread took and did not wait (`shared`), or counts
    /// what it waited (`VICTORY_PIPELINE_WAIT_SHARED=0`, per thread).
    fn give_back(&mut self, frame: u64, left: Duration, waited: Duration, shared: bool) {
        // A frame that began meanwhile has its own budget.
        if self.frame != frame {
            return;
        }
        if shared {
            self.used = self.used.saturating_sub(left - waited);
        } else {
            self.used += waited;
        }
    }
}

/// Waits for `p` while frame `frame` has budget of its kind left.
fn wait(p: &Pending, frame: u64) {
    let budget = wait_budget();
    if budget.is_zero() {
        return;
    }
    // Each kind (a world's pipelines, a pass over the screen) counts its own
    // frame: a world's worth of pipelines cannot spend a screen pass's.
    static SPENT: [Mutex<Spent>; 2] = [const { Mutex::new(Spent::new()) }; 2];
    let shared = shared_budget();
    let spent = &SPENT[usize::from(p.screen)];
    let left = spent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(frame, budget, shared);
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    spent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .give_back(frame, left, waited, shared);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

/// One kind's queues: those a draw waits for, then those started early.
/// `[0]` is the world's pipelines, `[1]` passes over the screen.
static QUEUE: [Mutex<Queue>; 2] = [const { Mutex::new(Queue::new()) }; 2];
static AVAILABLE: [Condvar; 2] = [const { Condvar::new() }; 2];

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    let kind = usize::from(p.screen);
    if !workers(kind) {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE[kind].lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE[kind].notify_one();
    }
}

fn next(kind: usize) -> Arc<Pending> {
    let mut queue = QUEUE[kind].lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE[kind].wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Number of background pipeline compile workers for a machine with `cores`
/// logical processors.
fn pipeline_worker_count(cores: usize) -> usize {
    (cores / 2).clamp(2, 12)
}

/// Number of screen-pass compile workers: the passes are few and their
/// shaders translate quickly, so two are enough to be ahead of the world.
fn screen_worker_count() -> usize {
    2
}

/// Starts the workers of one kind (half the logical processors, 2 to 12, and
/// two for screen passes) on first use.
fn workers(kind: usize) -> bool {
    static RUNNING: [OnceLock<bool>; 2] = [const { OnceLock::new() }; 2];
    *RUNNING[kind].get_or_init(|| {
        let screen = kind == 1;
        let mut running = false;
        let workers = if screen {
            screen_worker_count()
        } else {
            pipeline_worker_count(std::thread::available_parallelism().map_or(4, |n| n.get()))
        };
        for i in 0..workers {
            let name = if screen {
                format!("victory-pipelines-screen-{i}")
            } else {
                format!("victory-pipelines-{i}")
            };
            let spawned = std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        next(kind).make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn pipeline_worker_count_scales_with_cores() {
        assert_eq!(pipeline_worker_count(2), 2);
        assert_eq!(pipeline_worker_count(4), 2);
        assert_eq!(pipeline_worker_count(8), 4);
        assert_eq!(pipeline_worker_count(16), 8);
        assert_eq!(pipeline_worker_count(28), 12);
        assert_eq!(pipeline_worker_count(64), 12);
        // Screen passes keep two workers whatever the machine.
        assert_eq!(screen_worker_count(), 2);
    }

    /// A world's pipelines spend the whole budget of a frame and a screen
    /// pass still waits its own half second: the two kinds count apart, so
    /// a once-drawn screen pass is not starved behind the world.
    #[test]
    fn each_kind_keeps_its_own_budget() {
        let budget = Duration::from_millis(500);
        let mut world = Spent::new();
        let mut screen = Spent::new();

        // The world's pipelines take all of frame 7's budget.
        assert_eq!(world.take(7, budget, true), budget);
        assert_eq!(world.take(7, budget, true), Duration::ZERO);
        // The screen pass's own budget is untouched.
        assert_eq!(screen.take(7, budget, true), budget);

        // A new frame gives each kind the whole budget again.
        assert_eq!(world.take(8, budget, true), budget);
        assert_eq!(screen.take(8, budget, true), budget);
    }

    /// What a thread took and did not wait goes back to its kind's budget;
    /// a frame that began meanwhile is not charged by the thread that was
    /// waiting when it did.
    #[test]
    fn a_wait_gives_back_what_it_did_not_use() {
        let budget = Duration::from_millis(500);
        let mut spent = Spent::new();
        let left = spent.take(3, budget, true);
        assert_eq!(left, budget);
        spent.give_back(3, left, Duration::from_millis(10), true);
        assert_eq!(spent.take(3, budget, true), budget - Duration::from_millis(10));

        // A frame that began while the thread waited keeps its own budget.
        spent.give_back(4, left, Duration::from_millis(10), true);
        assert_eq!(spent.take(4, budget, true), budget);

        // Counted per thread (`VICTORY_PIPELINE_WAIT_SHARED=0`): each wait
        // adds only what it waited, and takes none of the rest.
        let mut per_thread = Spent::new();
        let left = per_thread.take(3, budget, false);
        per_thread.give_back(3, left, Duration::from_millis(20), false);
        assert_eq!(
            per_thread.take(3, budget, false),
            budget - Duration::from_millis(20)
        );
    }

    /// A screen-classified pipeline (no inputs, no depth) is routed to the
    /// screen pool, whose workers make it like any other (here: no shaders,
    /// so no host pipeline — the answer has the same shape, and the screen
    /// workers ran, so the routing did).
    #[test]
    fn a_screen_pass_is_made_by_its_own_workers() {
        let state = GraphicsState::screen_triangle_state();
        assert!(state.screen_pass());
        let p = Arc::new(Pending::new(Vec::new(), Some(Box::new(state))));
        assert_eq!(p.screen, screen_pool());
        if !screen_pool() {
            return;
        }
        assert!(ready(&p).is_none());
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A pipeline that is not a screen pass goes to the world's workers.
    #[test]
    fn a_world_pipeline_is_not_a_screen_pass() {
        let mut state = GraphicsState::screen_triangle_state();
        state.topology =
            windows::Win32::Graphics::Direct3D12::D3D12_PRIMITIVE_TOPOLOGY_TYPE_LINE;
        assert!(!state.screen_pass());
        let p = Pending::new(Vec::new(), Some(Box::new(state)));
        assert!(!p.screen);
    }
}
