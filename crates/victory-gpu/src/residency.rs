//! Host-memory admission: what speculation and reusable upload storage may
//! take, and how long to wait once the driver has refused an allocation.
//!
//! Idea from codec-d/CGBJ's coolfox branch (ab75ac3e0 "Back off refused GPU
//! pool growth and defer uploads under low commit", 8cc31ab37 "32GB RAM
//! residency budgets"): that code samples a Windows host's free physical RAM
//! and commit, keeps a pressure state with hysteresis, and gates its bindless
//! prefetch and upload pool growth on it. Our target is a 16 GiB PC and a
//! 16 GiB RADV card behind vkd3d-proton, where a refused import falls back
//! silently (HansKristian-Work/vkd3d-proton#3522) and our canary
//! ([`crate::inplace`]) and the mip retry carry that failure: so this keeps
//! the floors, the hysteresis and the refusal backoff, in our units, behind
//! `VICTORY_RESIDENCY=0`.
//!
//! Nothing here makes an allocation succeed. It keeps us from asking again
//! right away, and from taking the PC's last gigabyte for something a later
//! frame would rather have.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// How long a refused allocation keeps further ones from being tried. The
/// driver has just told us the PC or the card has nothing; every texture of
/// the frame asking again costs a failed driver call each.
const REFUSAL_MS: u64 = 500;

/// How long a staging buffer or a kept chunk no list holds is kept while the
/// PC is under pressure: one second, against the RAM-scaled idle of
/// [`crate::host::ram_idle_ms`].
pub(crate) const PRESSURE_IDLE_MS: u64 = 1_000;

/// Whether the guard is on: `VICTORY_RESIDENCY=0` turns it off, at once
/// (`crate::switches`) or from the environment. Off, nothing here ever limits
/// an upload, a growth or a retry, as before.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    let base = *ON.get_or_init(|| std::env::var_os("VICTORY_RESIDENCY").is_none_or(|v| v != "0"));
    crate::switches::b("VICTORY_RESIDENCY", base)
}

static PRESSURED: AtomicBool = AtomicBool::new(false);
/// The commit sample the admission gates read, at most one per sample window.
static COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static NEXT_SAMPLE: AtomicU64 = AtomicU64::new(0);
/// When a refused allocation may be tried again.
static RETRY_AT: AtomicU64 = AtomicU64::new(0);
/// Whether a growth was deferred since the last frame line.
static DEFERRED: AtomicU64 = AtomicU64::new(0);

/// Free physical RAM and available commit (physical RAM plus page file).
fn memory_status() -> Option<(u64, u64)> {
    // SAFETY: the call fills a correctly sized structure of plain data, and a
    // failed call leaves it unused.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0)
        .then_some((status.ullAvailPhys, status.ullAvailPageFile))
}

/// One sixteenth of the PC's RAM, between 256 MiB and 1 GiB: what an upload
/// leaves free once it has taken its bytes, so that Rust and the title are not
/// aborted for the last megabyte. A 16 GiB PC (16 GiB card, our target) keeps
/// 1 GiB; an 8 GiB one keeps 512 MiB.
fn upload_floor(total: u64) -> u64 {
    (total / 16).clamp(256 * MIB, GIB)
}

/// Twice [`upload_floor`]: what a brand-new OS block of upload storage (a
/// staging buffer, a scratch chunk) leaves free. Growing a fresh block costs
/// the title a page-file reservation the size of the block; reusing bytes
/// inside a block that is already committed costs nothing.
fn growth_floor(total: u64) -> u64 {
    upload_floor(total).saturating_mul(2)
}

/// Pressure enters when free physical RAM or available commit falls to a
/// fraction of the PC's RAM, and leaves only when both are comfortably above
/// it: a title's own allocations dip and recover many times a second, and a
/// guard that followed each dip would throttle a frame and unthrottle the next.
/// One failed sample keeps the last good state.
fn next_pressure(was: bool, total: u64, physical: u64, commit: u64) -> bool {
    if was {
        physical < total / 6 || commit < total / 5
    } else {
        physical <= total / 10 || commit <= total / 8
    }
}

/// Whether `bytes` more may be taken with `commit` available, leaving the
/// floor (`growth`: the larger one) free afterwards.
fn admitted(total: u64, commit: u64, bytes: u64, growth: bool) -> bool {
    let budget = budgets(Some(total));
    let floor = if growth { budget.growth_floor } else { budget.upload_floor };
    commit >= floor.saturating_add(bytes)
}

/// A soft per-frame limit on speculative upload, as his bindless prefetch
/// queue had: a single texture can cross it, and whatever is not admitted
/// waits for a later frame. Under pressure only 16 MiB, on a PC with little
/// RAM once staged bytes pass 1 GiB (his 32 GiB budget, ours by staged bytes)
/// 64 MiB; otherwise no limit of ours.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, limited: bool, staging: u64) -> u64 {
    if pressure {
        bytes.min(16 * MIB)
    } else if limited && staging >= GIB {
        bytes.min(64 * MIB)
    } else {
        bytes
    }
}

/// What this PC may keep for uploads, and for how long: one table by total
/// RAM instead of a constant at each site (his "32GB RAM residency budgets",
/// ours by tier). Unknown RAM gets the middle row's numbers.
pub(crate) struct Budgets {
    /// Scratch chunks no list holds that are kept for the next list.
    pub(crate) spare_upload: u64,
    /// How long idle upload storage is kept before it is freed.
    pub(crate) idle_ms: u64,
    /// Free commit left after an upload has taken its bytes.
    pub(crate) upload_floor: u64,
    /// Free commit left after a brand-new block of upload storage.
    pub(crate) growth_floor: u64,
}

pub(crate) fn budgets(total_ram: Option<u64>) -> Budgets {
    let spare_upload = match total_ram {
        Some(ram) if ram <= 16 * GIB => 64 * MIB,
        Some(ram) if ram >= 30 * GIB => 256 * MIB,
        _ => 128 * MIB,
    };
    // A small PC gives idle upload storage back sooner: it has less to spare
    // and the title's own allocations are what it is competing with.
    let idle_ms = match total_ram {
        Some(ram) if ram <= 12 * GIB => 5_000,
        _ => 10_000,
    };
    let total = total_ram.unwrap_or(0);
    Budgets {
        spare_upload,
        idle_ms,
        upload_floor: upload_floor(total),
        growth_floor: growth_floor(total),
    }
}

/// A refused allocation: `E_OUTOFMEMORY` from the driver means wait, not ask
/// the next texture's turn as well. Other errors (a bad descriptor, a dropped
/// device) are not about memory and do not back off.
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if !enabled() {
        return;
    }
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        deadline(crate::host::now_ms().saturating_add(REFUSAL_MS));
    }
}

fn deadline(at: u64) {
    RETRY_AT.fetch_max(at, Ordering::Relaxed);
}

/// Whether an allocation was refused less than [`REFUSAL_MS`] ago.
pub(crate) fn refused() -> bool {
    enabled() && crate::host::now_ms() < RETRY_AT.load(Ordering::Relaxed)
}

/// Counts a growth this guard held back, so a frame line can say so.
pub(crate) fn note_deferred() {
    DEFERRED.fetch_add(1, Ordering::Relaxed);
}

/// The growths deferred since the last call (0 when the guard is off).
pub(crate) fn take_deferred() -> u64 {
    DEFERRED.swap(0, Ordering::Relaxed)
}

/// Whether a staging buffer of `bytes` may be made now. A texture the title
/// keeps sampling is not speculative, so the first one of a frame is always
/// admitted: a guard that can starve the only upload of a frame is worse than
/// the OOM it avoids.
pub(crate) fn admit_upload(bytes: u64) -> bool {
    if !enabled() {
        return true;
    }
    if refused() {
        note_deferred();
        return false;
    }
    let total = crate::total_system_ram_bytes().unwrap_or(0);
    if total == 0 {
        return true;
    }
    admitted(total, sampled_commit(), bytes, false)
}

/// Whether a new OS block of upload storage of `bytes` may be committed. Free
/// ranges inside blocks already committed are not this: they cost nothing.
pub(crate) fn admit_growth(bytes: u64) -> bool {
    if !enabled() {
        return true;
    }
    if refused() {
        note_deferred();
        return false;
    }
    let total = crate::total_system_ram_bytes().unwrap_or(0);
    if total == 0 {
        return true;
    }
    admitted(total, sampled_commit(), bytes, true)
}

/// The commit sample, refreshed at most every 32 ms: a texture sweep runs for
/// seconds inside one submitted frame, and each texture asking the OS would
/// cost more than the decision.
fn sampled_commit() -> u64 {
    let now = crate::host::now_ms();
    let next = NEXT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some((_, commit)) = memory_status()
    {
        COMMIT.store(commit, Ordering::Relaxed);
    }
    COMMIT.load(Ordering::Relaxed)
}

/// Once a second, from the sweep ([`crate::host::sweep`]): samples the PC's
/// memory, moves the pressure state, and says when it changes. A failed sample
/// keeps the last good state.
pub(crate) fn tick() {
    let Some((physical, commit)) = memory_status() else {
        return;
    };
    COMMIT.store(commit, Ordering::Relaxed);
    if !enabled() {
        PRESSURED.store(false, Ordering::Relaxed);
        return;
    }
    let total = crate::total_system_ram_bytes().unwrap_or(0);
    if total == 0 {
        return;
    }
    let was = pressured();
    let pressure = next_pressure(was, total, physical, commit);
    if pressure == was {
        return;
    }
    PRESSURED.store(pressure, Ordering::Relaxed);
    // Staging figures only with the diagnostics build (the memory line's own).
    #[cfg(feature = "diagnostics")]
    let (staging_buffers, texture_staging_mib) = {
        let staged = crate::texture::memory();
        (staged[0], staged[1] >> 20)
    };
    #[cfg(not(feature = "diagnostics"))]
    let (staging_buffers, texture_staging_mib) = (0u64, 0u64);
    if pressure {
        tracing::warn!(
            pid = std::process::id(),
            frame = crate::frame::current(),
            total_ram_mib = total >> 20,
            physical_free_mib = physical >> 20,
            available_commit_mib = commit >> 20,
            staging_buffers,
            texture_staging_mib,
            upload_floor_mib = budgets(Some(total)).upload_floor >> 20,
            growth_floor_mib = budgets(Some(total)).growth_floor >> 20,
            "host memory pressure: deferring new upload storage and speculative uploads (VICTORY_RESIDENCY=0 turns this off)"
        );
    } else {
        tracing::info!(
            physical_free_mib = physical >> 20,
            available_commit_mib = commit >> 20,
            "host memory pressure cleared"
        );
    }
}

/// Whether the PC is short of memory right now (see [`next_pressure`]).
pub(crate) fn pressured() -> bool {
    enabled() && PRESSURED.load(Ordering::Relaxed)
}

/// Whether the PC has 32 GiB or less: its RAM budgets scale down, as the
/// caller's did.
pub(crate) fn limited_host_ram() -> bool {
    crate::total_system_ram_bytes().is_some_and(|ram| ram <= 32 * GIB)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: u64 = 8 * GIB;
    const OURS: u64 = 16 * GIB;
    const BIG: u64 = 32 * GIB;

    /// The floor is one sixteenth of the PC, and a small or a huge PC stays
    /// inside the bounds: 256 MiB (no PC we run on keeps less) and 1 GiB.
    #[test]
    fn floors_scale_with_the_pc_between_their_bounds() {
        assert_eq!(upload_floor(SMALL), 512 * MIB);
        assert_eq!(upload_floor(OURS), GIB);
        assert_eq!(upload_floor(BIG), GIB);
        assert_eq!(upload_floor(2 * GIB), 256 * MIB);
        assert_eq!(growth_floor(SMALL), GIB);
        assert_eq!(growth_floor(OURS), 2 * GIB);
        for total in [1 * GIB, SMALL, 12 * GIB, OURS, 30 * GIB, BIG, 64 * GIB] {
            assert!(upload_floor(total) >= 256 * MIB && upload_floor(total) <= GIB);
            assert!(growth_floor(total) > upload_floor(total));
        }
    }

    /// A growth needs twice the room an upload does, and both count the bytes
    /// about to be taken on top of the floor.
    #[test]
    fn an_upload_costs_its_bytes_on_top_of_the_floor() {
        let total = OURS;
        let floor = upload_floor(total);
        assert!(admitted(total, floor, 0, false));
        assert!(!admitted(total, floor - 1, 0, false));
        assert!(!admitted(total, floor + MIB, 2 * MIB, false));
        assert!(admitted(total, floor + 2 * MIB, 2 * MIB, false));
        let growth = growth_floor(total);
        assert!(admitted(total, growth, 64 * MIB, true));
        assert!(!admitted(total, growth - 1, 64 * MIB, true));
        // Room for a small upload is not room for a block.
        assert!(!admitted(total, floor + 64 * MIB, 64 * MIB, true));
    }

    /// Either constraint enters pressure, and both have to recover: a frame
    /// that dips once does not unthrottle the next one.
    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        let total = OURS;
        // Entering: free physical RAM or commit at a tenth/eighth of the PC.
        assert!(next_pressure(false, total, total / 10, total));
        assert!(next_pressure(false, total, total, total / 8));
        assert!(!next_pressure(false, total, total / 9, total / 7));
        // Leaving: both above a sixth/fifth.
        assert!(next_pressure(true, total, total / 6, total));
        assert!(next_pressure(true, total, total, total / 5));
        assert!(!next_pressure(true, total, total / 5, total / 4));
        // The state is sticky in one direction only.
        for total in [SMALL, OURS, BIG] {
            assert!(!next_pressure(true, total, total / 4, total / 4));
            assert!(next_pressure(false, total, total / 20, total / 20));
        }
    }

    /// The refusal deadline is a max, so two refusals in one frame do not
    /// shorten it, and the guard reads the clock, not a count.
    #[test]
    fn a_refused_allocation_sets_one_deadline_ahead() {
        let start = crate::host::now_ms();
        deadline(start.saturating_add(REFUSAL_MS));
        deadline(start);
        assert!(crate::host::now_ms() < RETRY_AT.load(Ordering::Relaxed));
    }

    /// A speculative upload is capped under pressure, and by staged bytes on a
    /// small PC; a single texture may still cross either.
    #[test]
    fn speculative_upload_is_capped_and_recovers() {
        assert_eq!(prefetch_limit(512 * MIB, true, false, 0), 16 * MIB);
        assert_eq!(prefetch_limit(8 * MIB, true, false, 0), 8 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, false, true, GIB), 64 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, false, true, GIB - 1), 512 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, false, false, 2 * GIB), 512 * MIB);
        assert_eq!(prefetch_limit(0, false, false, 0), 0);
        assert_eq!(prefetch_limit(192 * MIB, false, false, 0), 192 * MIB);
    }

    /// The budget table: the target PC (16 GiB) keeps 64 MiB of idle upload
    /// storage, the 30 GiB desktop 256 MiB, a 12 GiB one gives it back in 5 s,
    /// and the floors follow the RAM.
    #[test]
    fn the_budget_table_follows_the_pc() {
        let small = budgets(Some(12 * GIB));
        assert_eq!(small.spare_upload, 64 * MIB);
        assert_eq!(small.idle_ms, 5_000);
        let ours = budgets(Some(OURS));
        assert_eq!(ours.spare_upload, 64 * MIB);
        assert_eq!(ours.idle_ms, 10_000);
        assert_eq!(ours.upload_floor, GIB);
        assert_eq!(ours.growth_floor, 2 * GIB);
        let desktop = budgets(Some(30 * GIB));
        assert_eq!(desktop.spare_upload, 256 * MIB);
        assert_eq!(desktop.idle_ms, 10_000);
        let unknown = budgets(None);
        assert_eq!(unknown.spare_upload, 128 * MIB);
        assert_eq!(unknown.idle_ms, 10_000);
    }

    /// Microbench: the whole admission decision is arithmetic, so it has to be
    /// far cheaper than the driver call it stands in front of (a failed
    /// `CreateCommittedResource` is tens of microseconds; a `GlobalMemoryStatus`
    /// sample is a syscall). 1e6 decisions against 1e6 samples' worth of time.
    #[test]
    fn deciding_is_nothing_next_to_asking_the_driver() {
        let total = OURS;
        let commit = 3 * GIB;
        let started = std::time::Instant::now();
        let mut admitted_count = 0u64;
        for i in 0..1_000_000u64 {
            let bytes = (i % 4096) << 10;
            if admitted(total, commit, bytes, i % 2 == 0) {
                admitted_count += 1;
            }
        }
        let elapsed = started.elapsed();
        assert!(admitted_count > 0);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "1e6 admission decisions took {elapsed:?}"
        );
    }

    /// Off, the guard admits everything and reads nothing.
    #[test]
    fn the_switch_has_a_default_and_turns_every_gate_off() {
        if std::env::var_os("VICTORY_RESIDENCY").is_none() {
            assert!(enabled());
        }
        // A refused deadline set while on does not hold a guard that is off:
        // `refused` asks `enabled` first.
        deadline(u64::MAX);
        if !enabled() {
            assert!(admit_upload(u64::MAX));
            assert!(admit_growth(u64::MAX));
            assert!(!refused());
            assert!(!pressured());
        }
        RETRY_AT.store(0, Ordering::Relaxed);
    }
}
