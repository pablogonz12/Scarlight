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
    /// A fence value, after the work the queue was given before it: the queue
    /// is named, as the title's own signal names it (what it holds still holds
    /// the value back).
    Signal {
        queue: This,
        fence: This,
        value: u64,
    },
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
/// Jobs accepted since the process began, and those finished: a drain waits
/// only for what was accepted before it was called (the line runs them in
/// order, so a count of finished jobs covers the prefix).
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static FINISHED_JOBS: Mutex<u64> = Mutex::new(0);
/// Submissions taken or waiting: those the card will be given (a signal in
/// the line is not one).
static SUBMISSIONS: Mutex<u64> = Mutex::new(0);
/// A test holding the line's thread out of its own jobs (the switches being
/// on, it would take them as they are pushed).
#[cfg(test)]
static PARKED: Mutex<bool> = Mutex::new(false);
#[cfg(test)]
static PARKED_ACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static IDLE: Condvar = Condvar::new();
static WORK: Condvar = Condvar::new();
/// Since the last line of the log: submissions, waits the title took in, and
/// the microseconds it spent taking the line in.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn push(job: Job) {
    if matches!(job, Job::Submit { .. }) {
        *held(&SUBMISSIONS) += 1;
    }
    // Counted before it is in the line: a drain that reads the count and then
    // waits must not miss a job it came after.
    ACCEPTED.fetch_add(1, Relaxed);
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
    let submit = matches!(job, Job::Submit { .. });
    run(job);
    if submit {
        let mut submissions = held(&SUBMISSIONS);
        *submissions = submissions.saturating_sub(1);
    }
    *held(&FINISHED_JOBS) += 1;
    let mut pending = held(&PENDING);
    *pending = pending.saturating_sub(1);
    IDLE.notify_all();
    true
}

fn run(job: Job) {
    match job {
        Job::Submit { queue, lists } => {
            if let Some(host) = crate::host::host() {
                let backs = lists.ready();
                STATS[0].fetch_add(1, Relaxed);
                // The routes a title list's submission takes: a wait the
                // stream wrote down cuts it where it stands.
                crate::queue::dispatch(host, queue, &backs);
            }
            // The title may release it as soon as its call returned: the
            // reference taken with the job is let go the way the title's is
            // (the last one frees the queue, and takes it off the waiting
            // list).
            if !queue.is_null() {
                crate::queue::release(queue);
            }
        }
        Job::Signal { queue, fence, value } => {
            // What the queue holds comes first: a wait it was given before
            // this signal keeps the value back, as the title's own signal
            // does.
            let held_back = !queue.is_null() && crate::queue::after_held(queue, fence, value);
            if !held_back {
                crate::fence::signal(fence, value);
            }
            // As above: the title's references may be gone already.
            if !queue.is_null() {
                crate::queue::release(queue);
            }
            if !fence.is_null() {
                crate::fence::release(fence);
            }
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
            #[cfg(test)]
            if *held(&PARKED) {
                PARKED_ACK.store(true, Release);
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            #[cfg(test)]
            PARKED_ACK.store(false, Release);
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
pub(crate) fn signal(queue: This, fence: This, value: u64) {
    if !fence.is_null() {
        crate::object::add_ref(fence);
    }
    if !queue.is_null() {
        crate::object::add_ref(queue);
    }
    push(Job::Signal {
        queue,
        fence,
        value,
    });
}

/// Takes in everything the line holds: the title meets the card here.
pub(crate) fn drain() {
    if !on() || WORKER.with(|w| w.get()) {
        return;
    }
    // Only what the line took before this call: work other threads submit
    // meanwhile is not what this waits for.
    drain_to(ACCEPTED.load(Acquire));
}

/// Waits until the jobs accepted up to `accepted` have run.
fn drain_to(accepted: u64) {
    let began = std::time::Instant::now();
    let mut finished = held(&FINISHED_JOBS);
    while *finished < accepted {
        finished = IDLE.wait(finished).unwrap_or_else(|e| e.into_inner());
    }
    drop(finished);
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

/// Submissions the line holds or is running.
pub(crate) fn submissions() -> u64 {
    *held(&SUBMISSIONS)
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

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    /// Keeps the line's own thread out of a test's jobs: with the switches on
    /// it takes them as they are pushed, and the test then reads another
    /// thread's work. (With no line of ours there is nothing to keep out.)
    struct Parked {
        parked: bool,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn park_the_line() -> Parked {
        let guard = held(&TEST_LOCK);
        if !on() {
            return Parked { parked: false, _guard: guard };
        }
        *held(&PARKED) = true;
        // A job for it to wake on: it takes none, and says so.
        push(Job::Work(Box::new(|| {})));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !PARKED_ACK.load(Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "the line of ours did not stand still for the test"
            );
            std::thread::yield_now();
        }
        // That one is ours to run: it does nothing.
        assert!(run_one(), "the job the line was woken with");
        Parked { parked: true, _guard: guard }
    }

    impl Drop for Parked {
        fn drop(&mut self) {
            if self.parked {
                *held(&PARKED) = false;
                WORK.notify_all();
            }
        }
    }

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
        let _parked = park_the_line();
        let base = queued();
        let fence = fence();
        let written = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = written.clone();
        push(Job::Work(Box::new(move || flag.store(true, Release))));
        crate::object::add_ref(fence);
        push(Job::Signal {
            queue: std::ptr::null(),
            fence,
            value: 1,
        });
        assert!(!written.load(Acquire), "nothing runs before the line does");
        assert_eq!(queued() - base, 2);
        assert!(run_one(), "the work runs first");
        assert!(written.load(Acquire), "its writes are in");
        assert_eq!(queued() - base, 1, "the signal is still behind them");
        assert!(run_one(), "then the signal");
        // (Ours, not the title's: the title's read takes the line in.)
        assert_eq!(crate::fence::value(fence), 1, "the value is set last");
        assert_eq!(queued(), base);
        crate::fence::release(fence);
    }

    /// A drain waits for what the line took before it was called, not for what
    /// other threads hand it meanwhile (that would block for as long as they
    /// keep submitting).
    #[test]
    fn a_drain_waits_only_for_what_it_came_after() {
        let _parked = park_the_line();
        push(Job::Work(Box::new(|| {})));
        let up_to = ACCEPTED.load(Acquire);
        push(Job::Work(Box::new(|| {})));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drain_to(up_to);
            tx.send(()).ok();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(500)).is_err(),
            "the drain returned before the job it came after ran"
        );
        assert!(run_one(), "the first job runs");
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the drain waited for a job made after it");
        assert!(run_one(), "the second job runs");
    }

    /// A signal the line makes keeps the queue's identity: a wait the queue was
    /// given before it still holds the value back, as the title's own signal
    /// does (the value is not seen before the work before it ran).
    #[test]
    fn a_signal_of_the_line_waits_for_what_the_queue_holds() {
        let _parked = park_the_line();
        let mut queue: This = std::ptr::null();
        let desc = crate::queue::QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        assert_eq!(
            crate::queue::create(std::ptr::null(), &desc, &crate::queue::QUEUE_IIDS[0], &mut queue),
            S_OK
        );
        let fence = fence();
        // The queue is told to wait for a value nothing has reached.
        assert_eq!(crate::queue::wait(queue, fence, 2), S_OK);
        crate::object::add_ref(queue);
        crate::object::add_ref(fence);
        push(Job::Signal {
            queue,
            fence,
            value: 1,
        });
        assert!(run_one());
        assert_eq!(
            crate::fence::value(fence),
            0,
            "the value was seen though the queue still waited for one before it"
        );
        crate::queue::release(queue);
        crate::fence::release(fence);
    }

    /// A signal in the line is not a submission the card will be given: an
    /// object is not kept for a number `FINISHED` will never reach.
    #[test]
    fn a_pending_signal_is_not_a_submission_to_keep_an_object_for() {
        let _parked = park_the_line();
        struct Flag(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Release);
            }
        }
        let fence = fence();
        crate::object::add_ref(fence);
        push(Job::Signal {
            queue: std::ptr::null(),
            fence,
            value: 1,
        });
        assert_eq!(submissions(), 0, "a signal is not a submission");
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        crate::host::retire(Flag(dropped.clone()));
        assert!(
            dropped.load(Acquire),
            "an object was kept for a submission that is only a signal"
        );
        assert!(run_one());
        crate::fence::release(fence);
    }
}
