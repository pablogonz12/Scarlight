//! Bounded submission history and fence deadline detection.
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub(crate) struct Record {
    pub frame: u64,
    pub lists: usize,
    // These are process-wide totals sampled at submission, not per-list counts.
    pub draws_total: u64,
    pub dispatches_total: u64,
    pub pipelines: Vec<String>,
}

#[derive(Default)]
struct History(VecDeque<Record>);

impl History {
    fn push(&mut self, record: Record) {
        if self.0.len() == 16 {
            self.0.pop_front();
        }
        self.0.push_back(record);
    }
}

#[derive(Default)]
struct Deadlines {
    warned_3: bool,
    warned_8: bool,
}

impl Deadlines {
    fn check(&mut self, elapsed: Duration, completed: bool) -> Option<u8> {
        if completed {
            return None;
        }
        if elapsed >= Duration::from_secs(8) && !self.warned_8 {
            self.warned_3 = true;
            self.warned_8 = true;
            Some(8)
        } else if elapsed >= Duration::from_secs(3) && !self.warned_3 {
            self.warned_3 = true;
            Some(3)
        } else {
            None
        }
    }
}

#[cfg(windows)]
mod runtime {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    use windows::Win32::Graphics::Direct3D12::ID3D12Fence;

    struct Pending {
        fence: ID3D12Fence,
        value: u64,
        since: Instant,
        deadlines: Deadlines,
    }

    #[derive(Default)]
    struct State {
        history: History,
        pending: Vec<Pending>,
    }

    static STATE: OnceLock<Mutex<State>> = OnceLock::new();

    fn state() -> &'static Mutex<State> {
        STATE.get_or_init(|| {
            std::thread::Builder::new()
                .name("gpu-hangdog".into())
                .spawn(watch)
                .expect("start GPU watchdog");
            Mutex::new(State::default())
        })
    }

    fn watch() {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            let mut overdue = Vec::new();
            state.pending.retain_mut(|pending| {
                // SAFETY: the COM fence is kept alive by Pending.
                let completed = unsafe { pending.fence.GetCompletedValue() };
                if completed == u64::MAX || completed >= pending.value {
                    return false;
                }
                if let Some(seconds) = pending.deadlines.check(pending.since.elapsed(), false) {
                    overdue.push((pending.value, seconds));
                }
                true
            });
            for (value, seconds) in overdue {
                tracing::warn!("GPU fence overdue {seconds} s: value={value}, newest first: {:?}", state.history.0.iter().rev().collect::<Vec<_>>());
            }
        }
    }

    pub(crate) fn record(record: Record) {
        state().lock().unwrap_or_else(|e| e.into_inner()).history.push(record);
    }

    pub(crate) fn fence_started(fence: &ID3D12Fence, value: u64) {
        state().lock().unwrap_or_else(|e| e.into_inner()).pending.push(Pending {
            fence: fence.clone(),
            value,
            since: Instant::now(),
            deadlines: Deadlines::default(),
        });
    }

    pub(crate) fn dump() {
        let state = state().lock().unwrap_or_else(|e| e.into_inner());
        tracing::warn!("GPU fence overdue device removal: newest first: {:?}", state.history.0.iter().rev().collect::<Vec<_>>());
    }
}

#[cfg(windows)]
pub(crate) use runtime::{dump, fence_started, record};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_last_sixteen_newest_first() {
        let mut history = History::default();
        for frame in 0..20 {
            history.push(Record { frame, ..Default::default() });
        }
        let frames: Vec<_> = history.0.iter().rev().map(|r| r.frame).collect();
        assert_eq!(frames, (4..20).rev().collect::<Vec<_>>());
    }

    #[test]
    fn fake_fence_warns_once_per_deadline() {
        let mut deadlines = Deadlines::default();
        let check = |d: &mut Deadlines, secs, completed| d.check(Duration::from_secs(secs), completed);
        assert_eq!(check(&mut deadlines, 2, false), None);
        assert_eq!(check(&mut deadlines, 3, false), Some(3));
        assert_eq!(check(&mut deadlines, 4, false), None);
        assert_eq!(check(&mut deadlines, 8, false), Some(8));
        assert_eq!(check(&mut deadlines, 9, false), None);
        assert_eq!(check(&mut Deadlines::default(), 9, true), None);
    }
}
