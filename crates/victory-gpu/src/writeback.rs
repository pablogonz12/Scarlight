//! A write-back worker: what shaders wrote (the copy-back into title memory)
//! and the submission's bottom-of-pipe writes are done on a thread of their
//! own, after the submission's fence, so the submitting thread can prepare
//! the title's next submission meanwhile. Off by default:
//! `VICTORY_ASYNC_WRITEBACK=1` (idea from ivall/xcl#562, ported from the
//! Scarlight PR "copy-back on a per-queue worker").
//!
//! Correctness rests on three things:
//! - one worker, jobs in submission order: a job's copy-back runs before its
//!   bottom-of-pipe writes, and before the next job's, exactly as the
//!   submitting thread did them inline;
//! - the blocks a job puts back are held inaccessible to the title until it
//!   has ([`crate::watch::back_begin`]): a title thread that reads or writes
//!   one takes the access fault and waits for the job, so it cannot see the
//!   bytes the GPU replaced (Scarlight issue #32, "the no-access read path");
//! - a later submission's snapshot of the same host copy waits for the
//!   range (`Shadow::writing`) instead of taking title memory over bytes
//!   still to be put back (issue #32, "a snapshot per submission").
//!
//! A job's block protection is dropped even if the merge panics: [`Job`]'s
//! own `Drop` gives the blocks back and lets go of whoever waits on it, and
//! the worker catches the panic and goes on to the next job, so a fault
//! cannot leave pages inaccessible for the rest of the run (issue #32, "an
//! unwind guard"). A job the worker never runs - because it is gone, or
//! because the job turned out empty - is let go the same way.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::queue::Written;
use crate::shadow::Shadow;

thread_local! {
    /// Whether this thread is the write-back worker.
    static IS_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread is the write-back worker (it must not wait for its
/// own job when it faults on a block the job holds).
pub(crate) fn this_is_worker() -> bool {
    IS_WORKER.with(|w| w.get())
}

/// Whether the write-back runs on a thread of its own
/// (`VICTORY_ASYNC_WRITEBACK=1`; off by default).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    let start = *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_ASYNC_WRITEBACK").as_deref() == Ok("1");
        if on {
            tracing::info!(
                "copy-back runs on a thread of its own (VICTORY_ASYNC_WRITEBACK=1): a title read \
                 of a range still being put back waits for it"
            );
        }
        on
    });
    // Live, taken at a frame boundary with nothing in the air (crate::switches).
    crate::switches::b("VICTORY_ASYNC_WRITEBACK", start)
}

/// Whether a submission's shader-written ranges go back on [`worker`].
pub(crate) fn switched_on() -> bool {
    // The relay lands flights on a thread of its own already and keeps the
    // order of the line: the two together are not made to work.
    enabled() && !crate::shadow::relay()
}

/// A submission's work still to be done after its fence: what shaders wrote
/// goes back into title memory, then the bottom-of-pipe writes tell the
/// title the GPU is done.
pub(crate) struct Job {
    /// The submission's ranges, each copy's once, for the merge and then
    /// `Shadow::copy_back_done`.
    pub(crate) back: Vec<(Arc<Shadow>, Vec<(usize, usize)>)>,
    /// The submission's bottom-of-pipe writes, after the copy-back.
    pub(crate) writes: Vec<(usize, Written, usize)>,
    /// Blocks [`crate::watch::back_begin`] made inaccessible for the job.
    pub(crate) blocks: Vec<usize>,
    done: Arc<Done>,
}

impl Job {
    pub(crate) fn new() -> Job {
        Job {
            back: Vec::new(),
            writes: Vec::new(),
            blocks: Vec::new(),
            done: Arc::new(Done::default()),
        }
    }

    /// The flag a title thread waits on before it touches a block of the job.
    pub(crate) fn done(&self) -> Arc<Done> {
        self.done.clone()
    }

    /// Nothing to put back and nothing to write: no job.
    pub(crate) fn is_empty(&self) -> bool {
        self.back.is_empty() && self.writes.is_empty()
    }
}

impl Drop for Job {
    /// The job is done with, or it will never run: its blocks are given back
    /// the protection they had, then whoever faulted on one of them is let go.
    /// Order matters: a title thread that wakes on `finish` must find its page
    /// accessible. This also covers a panic in the merge (`run` below leaves it
    /// to this drop on the way out) and a job the worker never took.
    fn drop(&mut self) {
        let blocks = std::mem::take(&mut self.blocks);
        crate::watch::back_release(&blocks, &self.done);
        self.done.finish();
    }
}

/// How long a title's access to a job's block, or a submission's snapshot of
/// one, waits for the job before the merge is given up: a merge takes
/// milliseconds, and `Job`'s own drop finishes it on every early return, so a
/// wait this long means the job is never going to finish. `Done::wait_for`
/// then says so, with the job's id, and the caller takes the block back
/// instead of waiting forever (a title's loader thread that never returns is
/// the whole run stalled).
pub(crate) const MERGE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// The id of the next job, so a wait that gives up can name it.
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Whether a job has finished, for threads that faulted on its blocks.
pub(crate) struct Done {
    id: u64,
    finished: Mutex<bool>,
    ready: Condvar,
}

impl Default for Done {
    fn default() -> Done {
        Done {
            id: NEXT_JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            finished: Mutex::new(false),
            ready: Condvar::new(),
        }
    }
}

impl Done {
    /// The job this waits for (for a test that checks the wait named it).
    #[cfg(test)]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn finish(&self) {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_all();
    }

    /// Waits until the job's copy-back and writes are done; false, with a log
    /// line naming the job and what waited, if `limit` went by first. A wait
    /// that gives up must take the block back from the job (`watch::wait_held`):
    /// the job's own release will then find the block gone from `Back`, and
    /// the title's range is its own again.
    pub(crate) fn wait_for(&self, what: &str, limit: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        let mut finished = self.finished.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            let left = limit.saturating_sub(started.elapsed());
            if left.is_zero() {
                tracing::error!(
                    job = self.id,
                    what,
                    ms = started.elapsed().as_millis() as u64,
                    "a write-back job did not finish: giving its range up (that merge is lost, and named here)"
                );
                return false;
            }
            finished = self
                .ready
                .wait_timeout(finished, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Whether the job is done (no wait).
    pub(crate) fn finished(&self) -> bool {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
}

/// A job's work on the worker: the copy-back's merge, then the bottom-of-pipe
/// writes. Nothing here is allowed to end the worker - [`Job`]'s drop gives
/// the blocks back on a panic, and the caller catches it and goes on.
fn run(job: Job) {
    crate::host::hand_back_now(&job.back);
    for (at, what, bytes) in &job.writes {
        // The job's blocks are the job's to write: opened first, or the
        // kernel's write would fail on the protection the job itself set.
        crate::watch::back_open(*at, *bytes);
        what.write(*at as *mut u8, *bytes);
    }
}

/// The worker, started on first use.
fn worker() -> &'static Worker {
    static WORKER: OnceLock<Worker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (jobs, work) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("victory-gpu-writeback".into())
            .spawn(move || {
                IS_WORKER.with(|w| w.set(true));
                for job in work {
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    // The merge (and the scan before it) as the submitting
                    // thread did them; the job's drop then gives every range
                    // back, which lets the title's next refreshes take those
                    // bytes. A job that panics must not end the worker: its
                    // own pages are given back by that drop, and the next
                    // submission's copy-back still runs (issue #32).
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(job))).is_err() {
                        tracing::error!(
                            "a write-back job panicked: its blocks were given back and the worker goes on (that submission's copy-back is lost)"
                        );
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        crate::copystat::add(crate::copystat::C::AsyncJobs, 1);
                        crate::copystat::add(
                            crate::copystat::C::AsyncUs,
                            started.elapsed().as_micros() as u64,
                        );
                    }
                }
            })
            .expect("write-back worker");
        Worker { jobs }
    })
}

/// Queues `job` for the worker; the caller must have held its blocks with
/// [`crate::watch::back_begin`] first.
pub(crate) fn submit(job: Job) {
    if worker().jobs.send(job).is_err() {
        // The worker is gone (only at process exit): the job's own drop gives
        // its blocks back, so nothing stays inaccessible for the rest of the
        // run.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page a job holds is given back when the job is, also when it never
    /// ran (no worker, an empty job, a panic): the title must not find memory
    /// of its own inaccessible for the rest of the run (issue #32).
    #[test]
    fn a_job_dropped_without_running_gives_its_pages_back() {
        let _journal = crate::watch::journal_test_lock();
        use windows_sys::Win32::System::Memory::{
            MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS,
            PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualQuery,
        };
        let block = crate::watch::BLOCK;
        // SAFETY: an allocation of the test's own, protected and queried by
        // this test only.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                2 * block,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            let mut job = Job::new();
            let done = job.done();
            job.blocks = crate::watch::back_begin(&done, base as usize, base as usize + 2 * block);
            assert!(!job.blocks.is_empty(), "the job holds the pages");
            let protection = |at: usize| {
                let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                assert_ne!(VirtualQuery(at as *const _, &mut info, std::mem::size_of_val(&info)), 0);
                info.Protect
            };
            assert_eq!(protection(base as usize), PAGE_NOACCESS);
            drop(job);
            assert_eq!(protection(base as usize), PAGE_READWRITE, "the pages are the title's again");
            assert!(done.finished(), "and whoever waited on the job is let go");
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }

    /// A job that panics on the worker still gives its blocks back and finishes,
    /// so a title thread that faulted on one of its pages is let go instead of
    /// waiting for a job that will never finish (issue #32).
    #[test]
    fn a_panicking_job_finishes_and_lets_its_waiters_go() {
        let _journal = crate::watch::journal_test_lock();
        let done = Arc::new(Done::default());
        let waiter = done.clone();
        let thread = std::thread::spawn(move || {
            waiter.wait_for("a waiter of a test job", MERGE_DEADLINE);
            true
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let done = done.clone();
            move || {
                let _job = Job { back: Vec::new(), writes: Vec::new(), blocks: Vec::new(), done };
                panic!("the merge panics");
            }
        }));
        assert!(outcome.is_err());
        assert!(done.finished(), "the job's own drop finishes it on the way out");
        assert!(thread.join().unwrap(), "the waiter was let go");
    }

    /// A wait returns only once the job finished, on another thread.
    #[test]
    fn done_waits_for_the_finish() {
        let _journal = crate::watch::journal_test_lock();
        let done = Arc::new(Done::default());
        assert!(!done.finished());
        let flag = done.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            flag.finish();
        });
        assert!(done.wait_for("a wait for a test job", MERGE_DEADLINE));
        assert!(done.finished());
        thread.join().unwrap();
    }

    /// A job with nothing to put back and nothing to write (every shadow of
    /// its ranges was carded, or its range list came out empty) still gives
    /// its pages back when it is dropped: a title read of them must go
    /// through, and no one may be left waiting for a job that has no work.
    #[test]
    fn an_empty_job_gives_its_pages_back_and_lets_a_read_through() {
        let _journal = crate::watch::journal_test_lock();
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualQuery,
        };
        let block = crate::watch::BLOCK;
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                block,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            base.write_bytes(0x5a, 16);
            let mut job = Job::new();
            let done = job.done();
            job.blocks = crate::watch::back_begin(&done, base as usize, base as usize + block);
            assert!(!job.blocks.is_empty(), "the job holds the page");
            assert!(job.is_empty(), "nothing to put back, nothing to write");
            assert_eq!(
                {
                    let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
                        std::mem::zeroed();
                    assert_ne!(
                        VirtualQuery(base.cast(), &mut info, std::mem::size_of_val(&info)),
                        0
                    );
                    info.Protect
                },
                PAGE_NOACCESS
            );
            drop(job);
            assert!(done.finished(), "a job with no work still finishes");
            assert_eq!(
                {
                    let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
                        std::mem::zeroed();
                    assert_ne!(
                        VirtualQuery(base.cast(), &mut info, std::mem::size_of_val(&info)),
                        0
                    );
                    info.Protect
                },
                PAGE_READWRITE,
                "the title's read goes through"
            );
            assert_eq!(base.read(), 0x5a);
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }

    /// A wait that is never satisfied gives up at its deadline and says which
    /// job it waited for; it does not sleep forever (the title's thread that
    /// faults on the block would never run again).
    #[test]
    fn a_wait_gives_up_at_the_deadline_and_names_the_job() {
        let _journal = crate::watch::journal_test_lock();
        let done = Done::default();
        assert_ne!(done.id(), 0, "a job has an id to name it");
        let limit = std::time::Duration::from_millis(50);
        let started = std::time::Instant::now();
        assert!(!done.wait_for("a title read of a block a job holds", limit));
        let took = started.elapsed();
        assert!(took >= limit, "it waited the deadline: {took:?}");
        assert!(took < std::time::Duration::from_secs(5), "and no longer: {took:?}");
        assert!(!done.finished());
    }
}
