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
//! A job's block protection is dropped even if the merge panics (the
//! [`Release`] guard below), so a fault cannot leave pages inaccessible
//! (issue #32, "an unwind guard").

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

/// Whether a job has finished, for threads that faulted on its blocks.
#[derive(Default)]
pub(crate) struct Done {
    finished: Mutex<bool>,
    ready: Condvar,
}

impl Done {
    pub(crate) fn finish(&self) {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_all();
    }

    /// Waits until the job's copy-back and writes are done.
    pub(crate) fn wait(&self) {
        let mut finished = self.finished.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            finished = self.ready.wait(finished).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Whether the job is done (no wait).
    pub(crate) fn finished(&self) -> bool {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Gives a job's blocks back if the job does not (a panic in the merge).
struct Release(Arc<Done>, Vec<usize>);

impl Drop for Release {
    fn drop(&mut self) {
        crate::watch::back_release(&self.1, &self.0);
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
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
                for mut job in work {
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    let done = job.done();
                    let blocks = Release(done.clone(), std::mem::take(&mut job.blocks));
                    // The merge (and the scan before it) as the submitting
                    // thread did them; then every range is done with, which
                    // lets the title's next refreshes take those bytes.
                    crate::host::hand_back_now(&job.back);
                    for (at, what, bytes) in &job.writes {
                        // The job's blocks are the job's to write: opened
                        // first, or the kernel's write would fail on the
                        // protection the job itself set.
                        crate::watch::back_open(*at, *bytes);
                        what.write(*at as *mut u8, *bytes);
                    }
                    drop(blocks);
                    done.finish();
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
        // The worker is gone (only at process exit): nothing to wait for.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wait returns only once the job finished, on another thread.
    #[test]
    fn done_waits_for_the_finish() {
        let done = Arc::new(Done::default());
        assert!(!done.finished());
        let flag = done.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            flag.finish();
        });
        done.wait();
        assert!(done.finished());
        thread.join().unwrap();
    }
}
