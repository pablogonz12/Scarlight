//! The frame budget: a frame past [`REPORT`] says what bound it.
//!
//! At 30 frames a second a frame has 33 ms for the title's work, ours and
//! the GPU's. A world that stalls says `1.0 fps, 1012.4 ms/frame` in the
//! one-second line, a mean over its frames: one frame of 900 ms among 30
//! quick ones leaves no trace there, and a mean says nothing about which
//! part of the frame the time was in. This module keeps what a mean cannot:
//! the frame itself, split into the parts the submission path already
//! measures, with the largest named as what bound it ([`Phases::bound`]),
//! said once a second at most ([`say`]).
//!
//! The parts are `crate::host`'s `SUBMISSION_US` (preparing the submissions
//! on the CPU, of which title memory copies and texture uploads are
//! [`Phases::copies`] and [`Phases::textures`]; waiting for the GPU) and
//! `COPY_BACK` (what shaders wrote, copied back into title memory), and the
//! rest: [`Phases::outside`], the title's own work on its threads and the
//! translation of its command-list calls as it makes them. Nothing here
//! asks the GPU for timestamps and nothing takes a lock: a handful of
//! relaxed loads and one `Instant` a present, and a line at most once a
//! second.
//!
//! `VICTORY_FRAME_BUDGET=0` turns it off: nothing is read at a present and
//! nothing is said, so the report costs nothing when it is not wanted.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Frames longer than this are reported: a frame at 30 a second is 33.3 ms,
/// and past this one has slipped.
pub(crate) const REPORT: Duration = Duration::from_millis(50);

/// The least time between two lines: a stall is many slow frames, and one
/// line a second is enough to see what it is bound by.
const SAY_EVERY: Duration = Duration::from_secs(1);

/// Whether the report is on. `VICTORY_FRAME_BUDGET=0` turns it off; read
/// once, at the first present, and asked before anything is measured.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_BUDGET").is_none_or(|v| v != "0"))
}

/// What each part of one frame took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Phases {
    /// Preparing submissions on the CPU: copies of title memory, texture
    /// uploads, and the rest of the recording we do at submission.
    pub(crate) preparing: Duration,
    /// Of `preparing`, title memory copied up to the card.
    pub(crate) copies: Duration,
    /// Of `preparing`, textures detiled and uploaded.
    pub(crate) textures: Duration,
    /// Waiting for the GPU: a submission's fence, from send to signal.
    pub(crate) gpu: Duration,
    /// After submissions, what shaders wrote copied back into title memory.
    pub(crate) back: Duration,
}

impl Phases {
    /// The frame's time outside submissions: the title's own work on its
    /// threads, and the translation of its command-list calls as it makes
    /// them. Saturates at zero: the parts can be more than the frame when a
    /// present was counted with work of the frame before it.
    pub(crate) fn outside(&self, frame: Duration) -> Duration {
        frame.saturating_sub(self.preparing + self.gpu + self.back)
    }

    /// The largest part of the frame, as the text that names it and its
    /// time: what the frame was bound by. Ties go to the earlier part.
    pub(crate) fn bound(&self, frame: Duration) -> (&'static str, Duration) {
        let parts = [
            ("waiting for the GPU", self.gpu),
            ("our work around submissions", self.preparing),
            ("copying back what shaders wrote", self.back),
            ("outside submissions", self.outside(frame)),
        ];
        let mut bound = parts[0];
        for part in &parts[1..] {
            if part.1 > bound.1 {
                bound = *part;
            }
        }
        bound
    }

    /// The line for this frame, or nothing when it kept to the budget.
    fn line(&self, frame: Duration) -> Option<String> {
        if frame <= REPORT {
            return None;
        }
        let (bound, took) = self.bound(frame);
        Some(format!(
            "a frame took {:.1} ms: {:.1} ms {bound} (the bound), {:.1} ms our work around submissions \
             ({:.1} ms of title memory copies, {:.1} ms of texture uploads), {:.1} ms waiting for the \
             GPU, {:.1} ms copying back what shaders wrote, {:.1} ms outside submissions (the title's \
             own threads, and our translation of its calls)",
            ms(frame),
            ms(took),
            ms(self.preparing),
            ms(self.copies),
            ms(self.textures),
            ms(self.gpu),
            ms(self.back),
            ms(self.outside(frame)),
        ))
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Says what a frame past the budget was bound by, at most once a second.
/// `None` when the frame kept to it, when the report is off
/// ([`enabled`]), or when a line was said within the last second.
///
/// `said` is the caller's own (`host::Meter`, which counts the presents and
/// has it to itself): the throttle needs no lock, so a present takes none.
pub(crate) fn say(
    frame: Duration,
    phases: &Phases,
    said: &mut Option<Instant>,
) -> Option<String> {
    if !enabled() {
        return None;
    }
    let line = phases.line(frame)?;
    let now = Instant::now();
    if said.is_some_and(|last| now.duration_since(last) < SAY_EVERY) {
        return None;
    }
    *said = Some(now);
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(preparing: u64, gpu: u64, back: u64, frame: u64) -> (Duration, Phases) {
        let phases = Phases {
            preparing: Duration::from_millis(preparing),
            copies: Duration::from_millis(preparing / 2),
            textures: Duration::from_millis(preparing / 4),
            gpu: Duration::from_millis(gpu),
            back: Duration::from_millis(back),
        };
        (Duration::from_millis(frame), phases)
    }

    #[test]
    fn a_frame_within_the_budget_says_nothing() {
        let (frame, phases) = frame(10, 20, 5, 45);
        assert_eq!(phases.line(frame), None);
    }

    #[test]
    fn the_gpu_is_named_when_waiting_for_it_took_the_frame() {
        let (frame, phases) = frame(10, 600, 5, 640);
        let line = phases.line(frame).expect("past the budget");
        assert!(line.contains("600.0 ms waiting for the GPU (the bound)"), "{line}");
        assert_eq!(phases.bound(frame).0, "waiting for the GPU");
    }

    #[test]
    fn the_title_is_named_when_the_frame_was_outside_submissions() {
        let (frame, phases) = frame(20, 100, 10, 700);
        assert_eq!(phases.bound(frame).0, "outside submissions");
        assert_eq!(phases.outside(frame), Duration::from_millis(570));
        let line = phases.line(frame).expect("past the budget");
        assert!(line.contains("570.0 ms outside submissions (the title's own threads"), "{line}");
    }

    #[test]
    fn our_work_is_named_when_preparing_took_the_frame() {
        let (frame, phases) = frame(400, 50, 80, 560);
        assert_eq!(phases.bound(frame).0, "our work around submissions");
        let line = phases.line(frame).expect("past the budget");
        assert!(line.contains("400.0 ms our work around submissions (the bound)"), "{line}");
        assert!(line.contains("200.0 ms of title memory copies, 100.0 ms of texture uploads"), "{line}");
    }

    #[test]
    fn the_copy_back_is_named_and_ties_go_to_the_earlier_part() {
        let (took, phases) = frame(10, 200, 300, 520);
        assert_eq!(phases.bound(took).0, "copying back what shaders wrote");
        // The same time on both sides of the submissions: the GPU, named first.
        let (tied, phases) = frame(0, 100, 100, 210);
        assert_eq!(phases.bound(tied).0, "waiting for the GPU");
    }

    #[test]
    fn the_parts_more_than_the_frame_leave_nothing_outside_it() {
        // A present counted with work of the frame before it.
        let (frame, phases) = frame(100, 100, 100, 120);
        assert_eq!(phases.outside(frame), Duration::ZERO);
        assert!(phases.line(frame).is_some());
    }
}
