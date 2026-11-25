//! Submissions made by a thread of ours (`VICTORY_ASYNC_SUBMIT=1`): the
//! title's `ExecuteCommandLists` puts its lists in a line and returns, and the
//! thread that serves the line waits for their translation ([`crate::list::
//! thin`]) and submits them. Signals go into the same line, so a fence value
//! is set after the work before it ran; a wait for a fence, our own
//! submissions and a present take in what the line holds before them, which is
//! where the title meets the card again.
//!
//! Why: with the calls written down, what is left on the recording thread is
//! the translation of its own list if it has to wait for it, and the
//! submission; both are work the card's line does not need the title for. His
//! 68 -> ~63 ms step ("Submissions joined, card steps, textures sent ahead").
//!
//! Adapted from avatarmanz/VIctory-7900XTX `crates/victory-gpu/src/queue/behind.rs`
//! (commit b8b30d1cf, GPL-3.0-or-later).

use super::*;
use crate::list::thin::Submitted;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::sync::{Condvar, Mutex, OnceLock};

/// Whether submissions go into a line of ours (`VICTORY_ASYNC_SUBMIT=1`; it
/// needs the written-down lists, whose translation it waits for).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        wanted() && crate::list::thin::on() && {
            start();
            true
        }
    })
}

/// Whether it is asked for.
pub(crate) fn wanted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASYNC_SUBMIT").is_some_and(|v| v == "1"))
}

// The thread of ours is the one that runs the line: it never waits for it.
thread_local! {
    static WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One thing the line does, in the order the title made them.
enum Job {
    /// Lists the title submitted: their recordings are closed (on its thread);
    /// translated and submitted here.
    Submit {
        queue: This,
        lists: Box<Submitted>,
    },
    /// A fence value, after the work before it.
    Signal { fence: This, value: u64 },
    /// Anything else, for tests of the order.
    #[cfg(test)]
    Work(Box<dyn FnOnce() + Send>),
}

// SAFETY: D3D12 objects are free-threaded, and a job keeps what it names alive
// (`Submitted` holds its recordings and the lists of ours they went to).
unsafe impl Send for Job {}

static LINE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
/// Jobs taken or waiting; the line is empty when it is zero.
static PENDING: Mutex<usize> = Mutex::new(0);
static IDLE: Condvar = Condvar::new();
static WORK: Condvar = Condvar::new();
/// Since the last line of the log: submissions, waits the title took in, and
/// the microseconds it spent taking the line in.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn push(job: Job) {
    *held(&PENDING) += 1;
    held(&LINE).push_back(job);
    WORK.notify_one();
}

/// Runs the oldest job there is: whether there was one.
fn run_one() -> bool {
    let job = {
        let mut line = held(&LINE);
        let Some(job) = line.pop_front() else {
            return false;
        };
        job
    };
    run(job);
    let mut pending = held(&PENDING);
    *pending = pending.saturating_sub(1);
    IDLE.notify_all();
    true
}

fn run(job: Job) {
    match job {
        Job::Submit { queue, lists } => {
            if let Some(host) = crate::host::host() {
                let backs = lists.ready().to_vec();
                STATS[0].fetch_add(1, Relaxed);
                submit_list(host, queue, &backs);
            }
            // The title may release it as soon as its call returned.
            crate::object::release(queue);
        }
        Job::Signal { fence, value } => {
            crate::fence::signal(fence, value);
            // As above: the title's reference may be gone already.
            crate::object::release(fence);
        }
        #[cfg(test)]
        Job::Work(work) => work(),
    }
}

fn start() {
    let worker = move || {
        WORKER.with(|w| w.set(true));
        loop {
            {
                let mut line = held(&LINE);
                while line.is_empty() {
                    line = WORK.wait(line).unwrap_or_else(|e| e.into_inner());
                }
            };
            if std::panic::catch_unwind(run_one).is_err() {
                crate::host::fail(format_args!("the thread that submits lists panicked"));
            }
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("victory-submit".into())
        .spawn(worker)
    {
        crate::host::fail(format_args!("no thread to submit lists ({e})"));
    }
    tracing::info!("submissions are made by a thread of ours (VICTORY_ASYNC_SUBMIT=1)");
}

/// The lists the title submitted, into the line: the recordings are closed and
/// taken now (the title may reset its lists at once), translated and submitted
/// by the line's thread.
pub(crate) fn submit(queue: This, lists: &[This]) {
    if !queue.is_null() {
        crate::object::add_ref(queue);
    }
    push(Job::Submit {
        queue,
        lists: Box::new(crate::list::thin::submitted(lists)),
    });
}

/// A fence value, after what the queue was given before it.
pub(crate) fn signal(fence: This, value: u64) {
    if !fence.is_null() {
        crate::object::add_ref(fence);
    }
    push(Job::Signal { fence, value });
}

/// Takes in everything the line holds: the title meets the card here.
pub(crate) fn drain() {
    if !on() || WORKER.with(|w| w.get()) {
        return;
    }
    let began = std::time::Instant::now();
    let mut pending = held(&PENDING);
    while *pending > 0 {
        pending = IDLE.wait(pending).unwrap_or_else(|e| e.into_inner());
    }
    drop(pending);
    // And what the line sent to the card without a wait has run: a drain point
    // is where the title meets the card again.
    crate::host::settle_for_present();
    STATS[1].fetch_add(1, Relaxed);
    STATS[2].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// What the title made that the line has not finished: submissions waiting to
/// run count as made.
pub(crate) fn queued() -> u64 {
    *held(&PENDING) as u64
}

/// For the frame line.
pub(crate) fn log(frames: f64) {
    if !wanted() || !on() {
        return;
    }
    let submissions = STATS[0].swap(0, Relaxed) as f64 / frames;
    let waits = STATS[1].swap(0, Relaxed) as f64 / frames;
    let us = STATS[2].swap(0, Relaxed) as f64 / frames;
    tracing::info!(
        "submissions by the line: {submissions:.1}/frame, the title took it in {waits:.1} times \
         ({us:.0} us/frame), {} waiting",
        queued()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fence, for the order test (no device needed).
    fn fence() -> This {
        let mut fence: This = std::ptr::null();
        let iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, iid, &mut fence), S_OK);
        fence
    }

    /// A submission is not observed before its writes are: the fence a
    /// submission's signal sets is not set until the submission ran, and the
    /// title's wait for it takes the line in first.
    #[test]
    fn a_submission_is_not_seen_before_its_writes_are() {
        let fence = fence();
        let written = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = written.clone();
        push(Job::Work(Box::new(move || flag.store(true, Release))));
        push(Job::Signal { fence, value: 1 });
        assert!(!written.load(Acquire), "nothing runs before the line does");
        assert_eq!(queued(), 2);
        assert!(run_one(), "the work runs first");
        assert!(written.load(Acquire), "its writes are in");
        assert_eq!(queued(), 1, "the signal is still behind them");
        assert!(run_one(), "then the signal");
        assert_eq!(crate::fence::completed(fence), 1, "the value is set last");
        assert_eq!(queued(), 0);
    }
}
