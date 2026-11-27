//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! The pools' own pages (xmem's GPU memory, committed privately) can be
//! guarded instead of asked about: their asking costs the span, a guard costs
//! only what was written ([`guard_pools`], `VICTORY_GUARD_POOLS`).
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::writeback::Done;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
#[cfg(test)]
const KERNEL_PAGE: usize = 4096;

/// Numbers the askings; 0 is "never".
///
/// One sequence for every shard ([`crate::journal`]): a page's number is
/// compared with the asking a copy took, so a number from one shard has to
/// mean the same as one from another. Overlapping resets and their numbering
/// are ordered so each page's numbers reflect the order writes were observed.
static ASKED: AtomicU64 = AtomicU64::new(0);

static RESET_ORDER: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
static RESET_READY: std::sync::Condvar = std::sync::Condvar::new();

struct ResetRange(usize, usize);

impl Drop for ResetRange {
    fn drop(&mut self) {
        let mut active = RESET_ORDER.lock().unwrap_or_else(|e| e.into_inner());
        let index = active.iter().position(|r| *r == (self.0, self.1)).unwrap();
        active.swap_remove(index);
        RESET_READY.notify_all();
    }
}

/// Overlapping ranges remain ordered across reset, numbering and recording.
fn reset_order(first: usize, last: usize) -> ResetRange {
    #[cfg(test)]
    let started = std::time::Instant::now();
    let mut active = RESET_ORDER.lock().unwrap_or_else(|e| e.into_inner());
    while active.iter().any(|&(from, to)| first < to && from < last) {
        active = RESET_READY.wait(active).unwrap_or_else(|e| e.into_inner());
    }
    active.push((first, last));
    drop(active);
    #[cfg(test)]
    BENCH_WAIT.with(|v| v.set(v.get() + started.elapsed().as_nanos()));
    ResetRange(first, last)
}

fn reset_all() -> ResetRange {
    reset_order(0, usize::MAX)
}

#[cfg(test)]
thread_local! {
    static BENCH_WAIT: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
}

/// Whether a submission's one asking stands ([`ask_once`]): what [`wrote`]
/// asks before it records, without taking the asking's lock.
static KEPT: AtomicBool = AtomicBool::new(false);

/// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`]) and
/// the asking that did: `writes` answers for a range inside one from the
/// journal, without another kernel call.
struct Ahead {
    /// In page order and apart, for the searches [`covers`] makes.
    ranges: Vec<(usize, usize)>,
    /// The asking that stood for them.
    asked: u64,
}

static AHEAD: RwLock<Ahead> = RwLock::new(Ahead {
    ranges: Vec::new(),
    asked: 0,
});

fn ahead() -> RwLockReadGuard<'static, Ahead> {
    AHEAD.read().unwrap_or_else(|e| e.into_inner())
}

fn asking() -> RwLockWriteGuard<'static, Ahead> {
    AHEAD.write().unwrap_or_else(|e| e.into_inner())
}

/// The next asking's number: every asking of every thread and every record
/// gets its own, higher than all before it.
fn next_asking() -> u64 {
    ASKED.fetch_add(1, Relaxed) + 1
}

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    let _order = reset_all();
    ask_ahead_locked(ranges);
}

fn ask_ahead_locked(ranges: &[(usize, usize)]) {
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut a = asking();
    let a = &mut *a;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if KEPT.load(Relaxed) {
        return;
    }
    a.asked = next_asking();
    a.ranges.clear();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len()
            && pieces[k].0 <= last + AHEAD_GAP
            && last.max(pieces[k].1) - first <= AHEAD_MAX
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(a, &pieces[i..k]);
        i = k;
    }
    normalize_ahead(&mut a.ranges);
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    if !KEPT.load(Relaxed) {
        asking().ranges.clear();
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`VICTORY_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    let _order = reset_all();
    ask_ahead_locked(ranges);
    let mut a = asking();
    let a = &mut *a;
    if a.ranges.is_empty() {
        // `ask_ahead` asked nothing (fewer than two ranges).
        a.asked = next_asking();
    }
    // `ask_ahead` leaves a range that lies alone to `writes`, which asks
    // once per flush: with one asking for the submission that was every
    // such range asked again at each of its 70 flushes (run 76: 2,300
    // askings a frame with the switch on, as without it). They are asked
    // here, once; what is not write-watched stays with `writes`.
    for &(start, end) in ranges {
        if start >= end {
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        if !a.ranges.iter().any(|&(from, to)| from <= first && last <= to)
            && ask_kernel(a, first, last)
        {
            a.ranges.push((first, last));
        }
    }
    // In order and apart, for the searches [`covers`] makes: the ranges
    // above come in no order, and one may overlap what an asking already
    // holds (`[0, 12000)` and `[8000, 20000)` are each asked alone).
    normalize_ahead(&mut a.ranges);
    KEPT.store(!a.ranges.is_empty(), Relaxed);
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut a = asking();
    if KEPT.swap(false, Relaxed) {
        a.ranges.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    if len == 0 || !KEPT.load(Relaxed) {
        return;
    }
    let _order = reset_order(at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let now = next_asking();
    crate::journal::record(at / PAGE, at.saturating_add(len).div_ceil(PAGE), now);
}
/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`].
fn ask_run(a: &mut Ahead, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(a, first, last) {
        a.ranges.push((first, last));
        return;
    }
    let (left, right) = pieces.split_at(pieces.len() / 2);
    ask_run(a, left);
    ask_run(a, right);
}

/// Asks the kernel for the written pages of pages `[first, last)` and notes
/// them in the journal under the ahead asking; false (and nothing changed)
/// if the range is not write-watched memory of one reservation.
fn ask_kernel(a: &mut Ahead, first: usize, last: usize) -> bool {
    // A pool the guards hold is left to `writes`, which costs what was
    // written in it, not what its span is ([`guard_pools`]).
    if guard_pools()
        && first < last
        && (crate::guards::block_known(first * PAGE) || is_pool(first * PAGE))
    {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let Some(answer) = crate::journal::ask(first, last) else {
        return false;
    };
    answer.record(a.asked);
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    true
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let a = ahead();
    covers(&a.ranges, first, last).then(|| (a.asked, crate::journal::pages(first, last)))
}

/// One test at a time on the journal: the journal is one for the whole
/// process, and the harness runs the tests in parallel.
///
/// Held for the length of a test that asks ([`ask_once`], [`asked`],
/// [`written_since`], [`forget_askings`]) or notes writes ([`writing`],
/// [`wrote`], [`copied`], [`title_memory_remapped`]) in the journal. Without
/// it another test's asking resets `ahead` between this test's `ask_once`
/// and its `asked` ("asked ahead" failing), and its write lands under a
/// higher asking number in this test's pages, so a range nothing was
/// written in reads as written (`a_range_is_known_clean_until_an_asking_
/// finds_it_written` failed on `main`'s CI run 45835616327, and 1 run in 5
/// locally in parallel; it passes 5 of 5 with `--test-threads=1`). Nothing
/// in a single test is wrong, and no test skips itself: they take this lock
/// and run in turn.
///
/// Taken as the first thing a test does, before any device lock
/// (`crate::warptest`), so the two never form a cycle. Taking it also
/// [`forget_askings`]s, so each test starts from a journal of its own and
/// leaves none for the next.
#[cfg(test)]
pub(crate) fn journal_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    forget_askings();
    guard
}

/// Forgets every asking and every page found written: for a test that
/// starts from a journal of its own and leaves none behind (take
/// [`journal_test_lock`], which does this, so no other test asks or writes
/// meanwhile).
#[cfg(test)]
pub(crate) fn forget_askings() {
    ASKED.store(0, Relaxed);
    KEPT.store(false, Relaxed);
    let mut a = asking();
    a.ranges.clear();
    a.asked = 0;
    crate::journal::clear();
}

/// Whether a page of `[start, end)` was written after asking `since`, from
/// the journal: `None` where the journal holds no asking as new as `since`
/// covering the range (the kernel must be asked, [`writes`]).
///
/// Where it does, every write up to that asking was reported to it, so no
/// page written since leaves no entry: a copy whose pages were all copied
/// at `since` and gets `Some(false)` here has nothing to copy, without the
/// page walk and the copy's own lock that [`writes`] and the copy lead to.
pub(crate) fn written_since(start: usize, end: usize, since: u64) -> Option<bool> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    if first >= last {
        return Some(false);
    }
    let a = ahead();
    let covered = a.asked >= since && covers(&a.ranges, first, last);
    covered.then(|| crate::journal::any_since(first, last, since))
}

/// The ahead pieces as one range per run of pieces that meet or overlap,
/// sorted: every page range an asking of the submission covered lies in one
/// of them, so [`covers`]'s search finds it. `ask_once` asks a range an
/// asking already holds in part alone, which leaves pieces that overlap
/// (`[0, 12000)` and `[8000, 20000)`); a range in the second would not be
/// found, and `writes` would ask the kernel again for what the journal has.
fn normalize_ahead(pieces: &mut Vec<(usize, usize)>) {
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(pieces.len());
    for &(first, last) in pieces.iter() {
        match runs.last_mut() {
            Some(run) if first <= run.1 => run.1 = run.1.max(last),
            _ => runs.push((first, last)),
        }
    }
    *pieces = runs;
}

/// Whether `pieces` (page ranges, in order, apart) hold `[first, last)`
/// whole.
fn covers(pieces: &[(usize, usize)], first: usize, last: usize) -> bool {
    // The first piece ending after `first` is the only one that can hold it.
    let i = pieces.partition_point(|&(_, b)| b <= first);
    pieces.get(i).is_some_and(|&(a, b)| a <= first && last <= b)
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded, nor a pool the guards
/// hold, [`guard_pools`]).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    #[cfg(feature = "diagnostics")]
    let _trace = crate::trace::span("watch writes");
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    // Asked ahead with the submission's other ranges: the journal has it.
    // The asking's lock is held while its pages are read, so a range that
    // reads as covered reads with every page of the asking recorded.
    let covered = {
        let a = ahead();
        covers(&a.ranges, first, last).then_some(a.asked)
    };
    if let Some(now) = covered {
        return Some((now, crate::journal::pages(first, last)));
    }
    // The guard reset, kernel reset, number assignment and journal records
    // must be ordered together, including across the two reset paths.
    #[cfg(feature = "diagnostics")]
    let order_wait = crate::trace::span("reset_order wait");
    let _order = reset_order(first, last);
    #[cfg(feature = "diagnostics")]
    drop(order_wait);
    #[cfg(feature = "diagnostics")]
    let _order_hold = crate::trace::span("reset_order hold");
    // A pool the guards hold answers out of its own blocks, without a walk of
    // the pages asked about ([`guard_pools`]). Pages only a verifying asking
    // found are kept too: the copy needs them.
    let mut verified: Vec<usize> = Vec::new();
    if guard_pools() && !guarded {
        if !verify_due(first)
            && let Some(written) = guarded_range(start, end, true)
        {
            return Some(guarded_answer(first, last, &written));
        }
        if verify_due(first) {
            #[cfg(feature = "diagnostics")]
            let _verify = crate::trace::span("verify");
            verified = guarded_range(start, end, true).unwrap_or_default();
        }
    }
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let now = next_asking();
    let answer = crate::journal::ask(first, last);
    let guarded = if answer.is_some() {
        Vec::new()
    } else {
        if !guarded {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let guarding = std::time::Instant::now();
        // Verified already on this asking: the guards' answer is taken. A
        // copy (`guarded`) whose pages are a pool the kernel will not
        // write-watch is guarded too, instead of being copied whole at
        // every refresh ([`guard_copy_pools`], `VICTORY_GUARD_COPY_POOLS=0`
        // for the old way).
        let pools = !guarded || !verified.is_empty() || guard_copy_pools();
        // A range that is not whole blocks is not one we own: the block it
        // rounds into holds memory somebody else writes (our own heap, the
        // title's private allocations), and making that read-only faults in
        // its writer. Our own allocator writes there, so the fault came back
        // into this thread's own guard locks and the run hung
        // (`watch::tests`: a guarded page of the CRT heap). Refused and
        // counted; the copy takes its range whole, as it did before the pool
        // guard existed.
        let ours = start.is_multiple_of(PAGE) && end.is_multiple_of(PAGE);
        let written = match if pools && !ours {
            REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        } else {
            guarded_range(first * PAGE, last * PAGE, pools)
        } {
            Some(written) => written,
            None if verified.is_empty() => return None,
            None => verified.clone(),
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardUs, guarding.elapsed().as_micros() as u64);
        written
    };
    // The kernel's pages, the guarded ones and the verified ones all count as
    // written by this asking.
    #[cfg(feature = "diagnostics")]
    let kernel = answer.as_ref().map(|answer| answer.pages());
    if let Some(answer) = &answer {
        answer.record(now);
    }
    for page in guarded.into_iter().chain(verified.iter().copied()) {
        crate::journal::record(page, page + 1, now);
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
        if !verified.is_empty() {
            // What the kernel found and the guards had not: a write that made
            // no fault (the card's, or one through another view).
            let found: std::collections::BTreeSet<usize> = kernel
                .into_iter()
                .flatten()
                .map(|&at| at / PAGE)
                .collect();
            add(C::GuardMissed, verified.iter().filter(|p| !found.contains(p)).count() as u64);
        }
    }
    Some((now, crate::journal::pages(first, last)))
}

/// [`writes`]' answer for a range the guards answered: the asking's number
/// and the journal's pages of the range, with the pages the guards found
/// written noted under it.
fn guarded_answer(first: usize, last: usize, written: &[usize]) -> (u64, Vec<(usize, u64)>) {
    let now = next_asking();
    for &page in written {
        crate::journal::record(page, page + 1, now);
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Guarded, 1);
        add(C::GuardSpan, ((last - first) * PAGE) as u64);
    }
    (now, crate::journal::pages(first, last))
}

/// Whether a copy whose pages the kernel will not write-watch (a pool's
/// memory, committed privately: `GetWriteWatch` refuses it) is guarded
/// instead of copied whole at every refresh: on.
///
/// The class this removes: copies of 1 MiB and up that went to the card with
/// the 1 MiB floor (`VICTORY_CARD_BUFFERS_MIN_MB`) and whose refresh found
/// no answer, so `Shadow::update` marked the copy `NOT_WATCHED` and took its
/// whole window at every refresh for the rest of the run (run 11.14.5: 97
/// copies, 941 MB, 758-1,373 MB sent a frame). A guard costs what is
/// written: the first asking reports every block (the whole range once, as
/// before), and after it only the blocks the title wrote, 8-9 us a block.
///
/// `VICTORY_GUARD_POOLS=0` or `VICTORY_GUARD_COPY_POOLS=0` puts the
/// whole-range copy back. The kernel
/// cannot verify this class (that is why it cannot answer), so a write that
/// makes no fault (the card's into host memory, one through another view of
/// the same section) is not seen: `crate::guards` never guards an aliased
/// block, and a pool the runtime shows twice is the aliased case.
pub(crate) fn guard_copy_pools() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::COPY_POOLS.with(|v| v.get()) {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_GUARD_POOLS").is_none_or(|v| v != "0")
            && std::env::var_os("VICTORY_GUARD_COPY_POOLS").is_none_or(|v| v != "0")
    })
}

/// Whether a range the kernel will not answer for is guarded as a pool: for
/// the test that proves the class removed (`None` is the default).
#[cfg(test)]
pub(crate) fn set_guard_copy_pools(on: Option<bool>) -> Option<bool> {
    tests::COPY_POOLS.with(|v| v.replace(on))
}

/// Whether the pools' own pages are watched by our guards instead of the
/// kernel's page walk: `VICTORY_GUARD_POOLS=1`, off unless asked for (the
/// kernel is asked about every range, as before).
///
/// An asking costs what its span is, not what was written in it: the world's
/// 2,409 askings over 10.7 GB a frame are 2.0 us each in the quiet, 625 us
/// apiece while other threads map and unmap memory (the title's own churn,
/// `tools/copybench` b). A guard costs what is written: one fault per 64 KiB
/// block, 8.2 us round trip in the same bench (d), and an asking about a span
/// whose blocks are all clean costs a search of the guards' sets (e: 1.0 ms
/// a frame against 4.7, quiet, with 240 blocks written a frame).
///
/// What a guard cannot see is a write that makes no fault: the card's into
/// host memory, or one through another view of the same pages. So the kernel
/// still answers about each range as the verifier, on one frame in
/// [`verify_every`], and the pages only it found are counted
/// (`copystat::C::GuardMissed`).
pub(crate) fn guard_pools() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::POOLS.with(|v| v.get()) {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Off unless asked for: a guard that misses a write leaves a texture
    // silently wrong, so the asking stays the default until a run's counter
    // reads no pages missed.
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_POOLS").is_some_and(|v| v == "1"))
}

/// Every how many frames the kernel verifies a range's guards (the range's
/// first block decides which frame, so the askings of a frame are a share of
/// the spans, not all of them at once). `VICTORY_GUARD_VERIFY=1` verifies at
/// every asking, `0` never.
fn verify_every() -> u64 {
    #[cfg(test)]
    if let Some(n) = tests::VERIFY.with(|v| v.get()) {
        return n;
    }
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_GUARD_VERIFY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    })
}

/// Whether this asking of the range starting at `first` is also verified by
/// the kernel.
fn verify_due(first: usize) -> bool {
    let every = verify_every();
    if every == 0 {
        return false;
    }
    crate::frame::current()
        .wrapping_add((first as u64) >> 6)
        .is_multiple_of(every)
}

/// Whether `start` is memory the guards can hold as a pool: committed private
/// memory (the title's allocations, xmem's GPU memory) the layer's own copies
/// may make read-only.
fn is_pool(start: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(start as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Type == MEM_PRIVATE && info.Protect == PAGE_READWRITE
}

/// The unit a write-back job holds and a copy guards: one page.
pub(crate) const BLOCK: usize = PAGE;

/// Title memory the runtime unmapped or remapped, as ranges (start, (end,
/// generation)), since a copy took its bytes: the copy's snapshot of them is
/// stale, so its copy-back leaves them alone ([`copy_back_skips_remapped`]).
/// A copy that takes a range afresh clears it ([`remapped_taken`]), but only
/// where the mark is no newer than the generation it saw before reading
/// ([`remap_generation`]); a range no copy takes again costs one entry.
static REMAPPED: Mutex<BTreeMap<usize, (usize, u64)>> = Mutex::new(BTreeMap::new());

/// Numbers the remappings: a copy that sees the same number before and after
/// reading title memory read it after every remapping recorded (see
/// [`remapped_taken`]).
static REMAP_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn remapped() -> std::sync::MutexGuard<'static, BTreeMap<usize, (usize, u64)>> {
    REMAPPED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a copy-back leaves blocks the runtime remapped alone
/// (`VICTORY_SKIP_REMAPPED=0` puts back its bytes as before).
pub(crate) fn copy_back_skips_remapped() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SKIP_REMAPPED").is_none_or(|v| v != "0"))
}

/// The remappings recorded so far: a copy takes this before it reads title
/// memory and gives it to [`remapped_taken`], which clears only marks no
/// newer than it (a remap that landed during the read stays marked).
pub(crate) fn remap_generation() -> u64 {
    REMAP_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Notes that title memory `[at, at + len)` was unmapped or remapped: what a
/// copy took of it before is stale.
fn remapped_range(at: usize, len: usize) {
    if !copy_back_skips_remapped() {
        return;
    }
    let (from, to) = (at, at.saturating_add(len));
    if to <= from {
        return;
    }
    let mut map = remapped();
    // Under the map's lock, so a copy that reads the generation next sees
    // this mark (or does not, if it cleared first).
    let generation = REMAP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let (mut start, mut end, mut generation) = (from, to, generation);
    if let Some((&s, &(e, g))) = map.range(..=from).next_back()
        && e >= from
    {
        (start, end, generation) = (s, end.max(e), generation.max(g));
        map.remove(&s);
    }
    loop {
        let next = map.range(start..).next().map(|(&s, &(e, g))| (s, e, g));
        match next {
            Some((s, e, g)) if s <= end => {
                end = end.max(e);
                // The newest remap of the range is the one a copy has to
                // have read: a mark older than it must not clear it.
                generation = generation.max(g);
                map.remove(&s);
            }
            _ => break,
        }
    }
    map.insert(start, (end, generation));
}

/// Whether any byte of `[at, at + len)` was unmapped or remapped by the
/// runtime since a copy took it: the copy-back must not put the snapshot's
/// bytes there.
pub(crate) fn remapped_any(at: usize, len: usize) -> bool {
    if !copy_back_skips_remapped() || len == 0 {
        return false;
    }
    let map = remapped();
    map.range(..at + len).next_back().is_some_and(|(_, &(e, _))| e > at)
}

/// The runs of `[at, at + len)` no byte of which was remapped since a copy
/// took it: what a copy-back may write (an empty `Vec` when all of it is
/// marked, `[(at, at + len)]` when none is).
pub(crate) fn unremapped_runs(at: usize, len: usize) -> Vec<(usize, usize)> {
    let to = at.saturating_add(len);
    if to <= at {
        return Vec::new();
    }
    if !remapped_any(at, len) {
        return vec![(at, to)];
    }
    let map = remapped();
    let mut runs = Vec::new();
    let mut run = at;
    // The one range that may start before `at`, then the ones that start
    // inside: nothing below them can overlap.
    let mut keys: Vec<usize> = Vec::new();
    if let Some((&s, &(e, _))) = map.range(..=at).next_back()
        && e > at
    {
        keys.push(s);
    }
    keys.extend(map.range(at..to).map(|(&s, _)| s));
    for s in keys {
        let e = map[&s].0;
        if e <= run {
            continue;
        }
        if s >= to {
            break;
        }
        if s > run {
            runs.push((run, s));
        }
        run = run.max(e);
    }
    if run < to {
        runs.push((run, to));
    }
    runs
}

/// Takes the marks of `[from, to)` no newer than `generation` out of the
/// map; the newer ones stay (a remap a copy did not read).
fn clear_marks(map: &mut BTreeMap<usize, (usize, u64)>, from: usize, to: usize, generation: u64) {
    if to <= from {
        return;
    }
    // The one range that may start before `from`, then the ones that start
    // inside: nothing below them can overlap.
    let before = map.range(..=from).next_back().map(|(&s, &(e, g))| (s, e, g));
    let covered: Vec<(usize, usize, u64)> = before
        .filter(|&(_, e, _)| e > from)
        .into_iter()
        .chain(map.range(from..to).map(|(&s, &(e, g))| (s, e, g)))
        .collect();
    for (s, e, g) in covered {
        if g > generation {
            continue;
        }
        map.remove(&s);
        if s < from {
            map.insert(s, (from, g));
        }
        if e > to {
            map.insert(to, (e, g));
        }
    }
}

/// A copy took the bytes of `[at, at + len)` afresh: what the runtime did to
/// them before `generation` (taken with [`remap_generation`] before the read)
/// is in its snapshot now; a remap after it is not.
pub(crate) fn remapped_taken(generation: u64, at: usize, len: usize) {
    if !copy_back_skips_remapped() {
        return;
    }
    let (from, to) = (at, at.saturating_add(len));
    if to <= from {
        return;
    }
    // Blocks a write-back job holds have not been merged yet, and the merge
    // reads the mark: it stands there until the job is done and a copy takes
    // the blocks again.
    let held: Vec<(usize, usize)> = {
        let jobs = back();
        jobs.range(from / BLOCK..to.div_ceil(BLOCK))
            .map(|(&block, _)| (block * BLOCK, (block + 1) * BLOCK))
            .collect()
    };
    let mut map = remapped();
    let mut run = from;
    for (held_from, held_to) in held {
        if held_to <= run {
            continue;
        }
        if held_from >= to {
            break;
        }
        if held_from > run {
            clear_marks(&mut map, run, held_from.min(to), generation);
        }
        run = run.max(held_to);
    }
    if run < to {
        clear_marks(&mut map, run, to, generation);
    }
}

/// Regions of title memory the destination check ([`writable`]) has asked
/// about: the first block it asked at → (the first block after the region,
/// whether every page of it is committed read-write). A region's pages share
/// their state and protection, so one answer covers every copy inside it;
/// it is forgotten when the range's protection changes ([`title_memory_remapped`],
/// our own guards and write-back jobs) or a kernel write into it fails.
/// Range askings whose guard was refused because the range is not whole
/// blocks ([`writes`]): memory we did not allocate.
static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many askings were answered without a guard because the range was not
/// whole blocks (asked by a copy whose destination is not ours to guard).
#[cfg(any(test, feature = "diagnostics"))]
pub(crate) fn guard_refusals() -> u64 {
    REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

static DESTINATIONS: Mutex<BTreeMap<usize, (usize, bool)>> = Mutex::new(BTreeMap::new());

/// Forgets the destination answers that overlap `[at, at + len)`: what was
/// asked there no longer holds. Every answer whose region reaches into the
/// range goes, not only the one asked at its first block: an answer asked at
/// an earlier block (`[0, 4)` after one at `[2, 4)`) still covers blocks of
/// the range and would otherwise stay positive for a page that changed.
pub(crate) fn forget(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut regions = DESTINATIONS.lock().unwrap_or_else(|e| e.into_inner());
    let stale: Vec<usize> = regions
        .range(..last)
        .filter(|&(_, &(end, _))| end > first)
        .map(|(&start, _)| start)
        .collect();
    for start in stale {
        regions.remove(&start);
    }
}

/// Whether every 64 KiB block of `[at, at + len)` is committed read-write
/// now: the destination of a plain copy into title memory. `VirtualQuery`
/// walks the page tables to the end of the region sharing `at`'s state
/// (milliseconds in the title's big pools), so its answer is kept per region
/// ([`DESTINATIONS`]) instead of being asked at every copy. A copy that
/// cannot tell writes through the kernel instead, which fails per page
/// rather than faulting.
pub(crate) fn writable(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    if len == 0 {
        return false;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    // A block our own guard made read-only is not writable until it is
    // opened for the write ([`writing`]).
    if crate::guards::any_protected(first, last) {
        return false;
    }
    let mut regions = DESTINATIONS.lock().unwrap_or_else(|e| e.into_inner());
    let mut block = first;
    while block < last {
        if let Some((_, &(end, ok))) = regions.range(..=block).next_back()
            && end > block
        {
            if !ok {
                return false;
            }
            block = end;
            continue;
        }
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        let end = (info.BaseAddress as usize)
            .saturating_add(info.RegionSize)
            / BLOCK;
        // Only whole blocks are kept: a region that ends inside one says
        // nothing about the block as a whole, and the copy asks again (a
        // title whose protections differ inside a block is rare).
        if end <= block {
            return false;
        }
        let ok = info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE;
        regions.insert(block, (end, ok));
        if !ok {
            return false;
        }
        block = end;
    }
    true
}

/// A kernel write into title memory `[at, at + len)` failed: a title may
/// have made the range read-only without telling us, so what the destination
/// check knew of it is not trusted and the next copy asks again.
pub(crate) fn write_failed(at: usize, len: usize) {
    forget(at, len);
}

/// For view memory, and for a pool with `pools`: the pages of `[start, end)`'s
/// blocks written since they were last guarded (all of a block's pages),
/// guarding them again before the caller copies them. `None` if the memory is
/// neither a view nor, with `pools`, a pool the guards can hold.
fn guarded_range(start: usize, end: usize, pools: bool) -> Option<Vec<usize>> {
    #[cfg(feature = "diagnostics")]
    let _trace = crate::trace::span_bytes("guarded_range", end.saturating_sub(start));
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // A range inside a runtime placeholder reservation is view memory (the
    // runtime maps its pages through `xmem::map`, which tells us), and its
    // holes are in the views: no question to Windows either way.
    let in_placeholder = crate::inplace::placeholder_range(first * BLOCK, last * BLOCK);
    if in_placeholder {
        crate::inplace::PLACEHOLDER_HITS[0]
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let known = in_placeholder
        || (!*ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1"))
            && crate::guards::block_known(start));
    if !known && !is_view(start, end) && !(pools && is_pool(start)) {
        return None;
    }
    install();
    Some(crate::guards::reguard(first, last))
}

/// [`guarded_range`] for view memory alone (the tests' entry).
#[cfg(test)]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    guarded_range(start, end, false)
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes = *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// The runtime mapped views of bytes we hold view guards on, at other
/// addresses than the ones guarded (a section shown twice): every guarded
/// block of them is given back to write. A page guarded read-only at one
/// address can be written through another, where no handler of ours sees
/// it, so the guard of such a page covers nothing and the copies through it
/// must go to the kernel (after avatarmanz 2be0515c).
pub fn opened_beside(views: &[(usize, usize)]) {
    if views.is_empty() || !crate::guards::aliases() {
        return;
    }
    let mut opened = Vec::new();
    for &(at, len) in views {
        if len != 0 && crate::guards::open(at, at.saturating_add(len)) {
            opened.push((at, len));
        }
    }
    if !opened.is_empty() {
        tracing::info!(?opened, "views opened beside guarded ones (VICTORY_GUARD_ALIASES)");
    }
}

/// Whether every 64 KiB block of `[at, at + len)` is title memory the layer
/// knows is there ([`copied`]) or has guarded: then a copy can go at the
/// bytes with `ptr::copy` instead of a kernel call, which on Wine costs a
/// wineserver roundtrip for every write (Wine 11: `NtWriteVirtualMemory` has
/// no self path) and a guarded `memmove` for every read.
pub(crate) fn mapped(at: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    // One pass over each shard's set, not a lookup a block at a time.
    crate::guards::all_mapped(first, last)
}

/// A kernel copy took all of `[at, at + len)` of title memory: every block
/// of it is committed and mapped, so later copies of it (and of ranges
/// inside it) may go at the bytes ([`mapped`]). Cleared for a range the
/// runtime unmaps ([`title_memory_remapped`]).
pub(crate) fn copied(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    crate::guards::mark_mapped(first, last);
}

/// Blocks a write-back job holds `(a flag, whether the block was being given
/// back)`; see [`crate::writeback`]. Kept apart from the guards above: a
/// block may be both a view's guarded block and a job's.
static BACK: Mutex<BTreeMap<usize, (std::sync::Arc<Done>, u32)>> =
    Mutex::new(BTreeMap::new());

fn back() -> std::sync::MutexGuard<'static, BTreeMap<usize, (std::sync::Arc<Done>, u32)>> {
    BACK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A job's copy-back covers title memory `[from, to)` (`crate::writeback`):
/// the blocks are made inaccessible to both reads and writes until the job
/// has put the GPU's bytes back (`back_release`, or the job writing them).
/// Returns the blocks newly held, for [`back_release`].
///
/// Contiguous runs are protected in one call; a run that fails is walked
/// block by block, and a block that starts an uncommitted range (a view's
/// hole) is given up: the title has nothing there to read.
pub(crate) fn back_begin(done: &std::sync::Arc<Done>, from: usize, to: usize) -> Vec<usize> {
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, VirtualProtect};
    let (first, last) = (from / BLOCK, to.div_ceil(BLOCK));
    if first >= last {
        return Vec::new();
    }
    // Before a page is made inaccessible: the title's own writes there take
    // the access fault this handler resolves (run cb-1: a write to a held
    // block with no handler installed yet was the run's crash).
    install();
    let mut held = Vec::new();
    let take = |a: usize, b: usize, held: &mut Vec<usize>| {
        // The map is taken *before* the pages are made inaccessible: a title
        // thread that faults in between would find no job for the block
        // (`back_pending`), so the handler could not wait for the merge and
        // would pass the fault on. With the map held across the protect, that
        // fault waits here and then finds the job. Nothing inside this block
        // touches title memory, so the handler cannot be entered on this
        // thread while it holds the map.
        let mut map = back();
        let mut old = 0;
        // SAFETY: title memory; only its protection changes.
        if unsafe {
            VirtualProtect(
                (a * BLOCK) as *const _,
                (b - a) * BLOCK,
                PAGE_NOACCESS,
                &mut old,
            )
        } != 0
        {
            for block in a..b {
                // A block an earlier job holds keeps the protection it had
                // before the first job took it: `back_release` gives that
                // back when the last job is done with the block.
                let old = map.get(&block).map_or(old, |(_, old)| *old);
                map.insert(block, (done.clone(), old));
                held.push(block);
            }
            drop(map);
            // Inaccessible now: not a plain copy's destination.
            forget(a * BLOCK, (b - a) * BLOCK);
            return 0;
        }
        -1
    };
    if take(first, last, &mut held) == 0 {
        return held;
    }
    let mut at = first;
    while at < last {
        let hole = crate::guards::uncommitted_end(at);
        if hole > at {
            at = hole;
            continue;
        }
        if take(at, at + 1, &mut held) != 0 {
            // Not ours to protect (a view's read-only block, say).
        }
        at += 1;
    }
    held
}

/// The job, if any, whose write-back holds block holding `at`: a thread that
/// faulted there waits for it.
pub(crate) fn back_pending(at: usize) -> Option<std::sync::Arc<Done>> {
    back().get(&(at / BLOCK)).map(|(done, _)| done.clone())
}

/// The blocks `[from, to)` a job holds, waited for: for a kernel write of
/// ours into title memory that the job's own write-back is not the one to
/// order. Nothing to wait for if no job holds them.
pub(crate) fn back_wait(from: usize, to: usize) {
    let jobs: Vec<std::sync::Arc<Done>> = {
        let map = back();
        map.range(from / BLOCK..to.div_ceil(BLOCK))
            .map(|(_, (done, _))| done.clone())
            .collect()
    };
    for done in jobs {
        let _ = done.wait_for(
            "a fence signal behind a write-back job",
            crate::writeback::MERGE_DEADLINE,
        );
    }
}

/// Waits for the job that holds `at`, if one does; on the deadline the job's
/// hold is given up (`back_open_now`: the merge is lost, and said so by
/// `Done::wait_for`) so the title's access can go through instead of the run
/// stopping on a job that will never finish. The worker never waits for its
/// own job: it opens the block and goes on.
pub(crate) fn wait_held(at: usize) {
    let Some(done) = back_pending(at) else {
        return;
    };
    if crate::writeback::this_is_worker() {
        back_open_now(at, 1);
        return;
    }
    if !done.wait_for(
        "a title access to a block a write-back job holds",
        crate::writeback::MERGE_DEADLINE,
    ) {
        back_open_now(at, 1);
    }
}

/// Before a kernel write into title memory `[at, at + len)`: the blocks are
/// given back writable, as [`open`] does for the guarded ones. A job that
/// holds one is waited for first (bounded: [`wait_held`] and the deadline):
/// opening the block under it would let the job's merge put the GPU's older
/// bytes over the title's write afterwards, i.e. lose the merge *and* the
/// title's bytes.
pub(crate) fn back_open(at: usize, len: usize) {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    if !crate::writeback::this_is_worker() {
        let jobs: Vec<std::sync::Arc<Done>> = {
            let map = back();
            map.range(first..last).map(|(_, (done, _))| done.clone()).collect()
        };
        for done in jobs {
            let _ = done.wait_for(
                "a write into a block a write-back job holds",
                crate::writeback::MERGE_DEADLINE,
            );
        }
    }
    back_open_now(at, len);
}

/// [`back_open`] without waiting for the job: the worker's own write into its
/// blocks, and the repair after a wait gave up.
pub(crate) fn back_open_now(at: usize, len: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut opened = Vec::new();
    {
        let mut map = back();
        for block in first..last {
            if map.remove(&block).is_some() {
                let mut old = 0;
                // SAFETY: a block a write-back job made inaccessible.
                unsafe {
                    VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old)
                };
                opened.push(block);
            }
        }
    }
    // A guarded block in range must not stay recorded as clean or held: the
    // kernel's write did not fault into `open`.
    if !opened.is_empty() {
        crate::guards::opened(&opened);
        // Opened writable for a kernel write.
        if let (Some(first), Some(last)) = (opened.first(), opened.last()) {
            forget(first * BLOCK, (last + 1 - first) * BLOCK);
        }
    }
}

/// The job is done with `blocks` (from [`back_begin`]): each still held by
/// it is given back the protection it had.
pub(crate) fn back_release(blocks: &[usize], done: &std::sync::Arc<Done>) {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    let mut open_blocks = Vec::new();
    {
        let mut map = back();
        for block in blocks {
            if map.get(block).is_some_and(|(d, _)| std::sync::Arc::ptr_eq(d, done))
                && let Some((_, old)) = map.remove(block)
            {
                open_blocks.push((*block, old));
            }
        }
    }
    for (block, old) in open_blocks {
        let mut was = 0;
        // SAFETY: a block the job made inaccessible.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut was) };
        // Its protection is the job's again: ask before a plain copy.
        forget(block * BLOCK, BLOCK);
    }
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    // A block a write-back job holds is this write's now.
    back_open(at, len);
    crate::guards::open(at, at.saturating_add(len));
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>);

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again.
pub fn title_memory_remapping() -> Remapping {
    Remapping(MAPPING.write().unwrap_or_else(|e| e.into_inner()))
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory.
pub fn title_memory_reading() -> Reading {
    Reading(MAPPING.read().unwrap_or_else(|e| e.into_inner()))
}

/// [`title_memory_reading`] if it can be had at once: `None` while the
/// runtime changes a mapping (or waits to), for a caller that has another
/// way to read and would rather not wait.
pub fn title_memory_reading_now() -> Option<Reading> {
    match MAPPING.try_read() {
        Ok(guard) => Some(Reading(guard)),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(e.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// [`title_memory_reading`] if it can be had within `limit`: `None` when the
/// runtime keeps changing title memory for longer than that.
///
/// For a caller that has another way to read (`crate::texture::refresh`):
/// the runtime's remapping lock is taken again and again while the world
/// streams in (1,033 ms of one frame's texture refreshes waited for it in
/// run fx-d3dc-1, 507 in another), and waiting for it can never take less
/// than the remapping does.
pub fn title_memory_reading_within(limit: std::time::Duration) -> Option<Reading> {
    let started = std::time::Instant::now();
    loop {
        match MAPPING.try_read() {
            Ok(guard) => return Some(Reading(guard)),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(Reading(e.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= limit {
            return None;
        }
        // Yield, don't sleep: a Windows sleep is 1-15 ms whatever is asked,
        // which would spend the whole budget at once.
        std::thread::yield_now();
    }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    crate::texback::remapped(at, len);
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    // What the destination check knew of the range (committed, writable)
    // may be the old mapping's.
    forget(at, len);
    crate::guards::remapped(at, len);
    // A copy whose submission is still pending took what was here before:
    // its copy-back must leave the new mapping's bytes alone.
    remapped_range(at, len);
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        #[cfg(feature = "diagnostics")]
        let _trace = crate::trace::fault_span();
        let is_write = r.ExceptionInformation[0] == WRITE;
        let at = r.ExceptionInformation[1];
        // A write-back job has this block (inaccessible to reads and writes
        // alike): wait for the GPU's bytes to be put back before the
        // instruction reads or writes them, but never past the deadline with
        // no word said: `wait_held` gives the block up, names the job it
        // waited for, and this instruction goes on with the title's range.
        if back_pending(at).is_some() {
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            wait_held(at);
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(crate::copystat::C::AsyncWaits, 1);
                crate::copystat::add(
                    crate::copystat::C::AsyncWaitUs,
                    started.elapsed().as_micros() as u64,
                );
            }
            // The job gave the block back; a newer job's block faults here
            // again.
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if !is_write {
            // A read of memory that is not ours: as before, someone else's.
            // (A read of a block a job holds is answered above.)
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        // Both answers under the block's one shard lock: another thread
        // cannot open it and a copy guard it again between them.
        if crate::guards::open_block(at, || writable_now(at)) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            // Not a block we hold and not memory the title may write: say who
            // knows the address, so a fault like cb-1's is diagnosable (which
            // registry, whether a job still holds its block or the block was
            // given back, and the protection it has now).
            static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                use windows_sys::Win32::System::Memory::{
                    MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
                };
                // SAFETY: plain data filled in by the query.
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: a valid out pointer of the right size.
                let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
                let (guarded, absent) = crate::guards::holds(at);
                tracing::warn!(
                    at = format_args!("{at:#x}"),
                    block = format_args!("{:#x}", at / BLOCK),
                    written = is_write,
                    protect = format_args!("{:#x}", if n != 0 { info.Protect } else { 0 }),
                    noaccess = n != 0 && info.Protect == PAGE_NOACCESS,
                    guarded,
                    absent,
                    owed = back().get(&(at / BLOCK)).is_some(),
                    "an access fault this handler cannot resolve: not writable, not a write-back job's block"
                );
            }
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now (asked every time).
fn writable_now(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    thread_local! {
        /// The kernel askings `writes` and the ahead asking made on this
        /// thread (each test runs on its own).
        /// `VICTORY_GUARD_POOLS` and `VICTORY_GUARD_VERIFY` for one test: the
        /// environment is read once a process.
        pub(super) static POOLS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
        pub(super) static VERIFY: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
        /// `VICTORY_GUARD_COPY_POOLS` for one test.
        pub(super) static COPY_POOLS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    fn kernel_asked() -> usize {
        crate::journal::kernel_asks()
    }

    #[test]
    fn disjoint_resets_proceed_while_overlapping_resets_wait() {
        let _journal = journal_test_lock();
        let held = reset_order(100, 200);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let tx2 = tx.clone();
            scope.spawn(move || {
                let _range = reset_order(200, 300);
                tx2.send(1).unwrap();
            });
            assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), 1);
            scope.spawn(move || {
                let _range = reset_order(150, 250);
                tx.send(2).unwrap();
            });
            assert!(rx.recv_timeout(std::time::Duration::from_millis(50)).is_err());
            drop(held);
            assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), 2);
        });
    }

    #[test]
    fn watch_microbench() {
        let _journal = journal_test_lock();
        const ITER: u128 = 100;
        for size in [PAGE, 1 << 20, 50 << 20] {
            for threads in [1, 7] {
                let barrier = std::sync::Barrier::new(threads);
                let results = std::thread::scope(|scope| {
                    let mut workers = Vec::new();
                    for _ in 0..threads {
                        let barrier = &barrier;
                        workers.push(scope.spawn(move || {
                            let at = unsafe {
                                VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
                            } as usize;
                            assert_ne!(at, 0);
                            POOLS.with(|v| v.set(Some(false)));
                            BENCH_WAIT.with(|v| v.set(0));
                            crate::journal::BENCH_KERNEL.with(|v| v.set(0));
                            barrier.wait();
                            let started = std::time::Instant::now();
                            for i in 0..ITER {
                                unsafe { (at as *mut u8).write_volatile(i as u8) };
                                let (now, pages) = writes(at, at + size, false).unwrap();
                                assert!(pages.iter().any(|&(page, generation)| page == at / PAGE && generation == now));
                            }
                            let elapsed = started.elapsed().as_nanos();
                            let wait = BENCH_WAIT.with(|v| v.get());
                            let kernel = crate::journal::BENCH_KERNEL.with(|v| v.get());
                            unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
                            (elapsed, wait, kernel)
                        }));
                    }
                    workers.into_iter().map(|w| w.join().unwrap()).collect::<Vec<_>>()
                });
                let n = ITER * threads as u128;
                let elapsed: u128 = results.iter().map(|v| v.0).sum();
                let wait: u128 = results.iter().map(|v| v.1).sum();
                let kernel: u128 = results.iter().map(|v| v.2).sum();
                println!("watch-bench size={size} threads={threads} ns/call total={} wait={} kernel={} journal_and_other={} guards=0", elapsed/n, wait/n, kernel/n, (elapsed-wait-kernel)/n);
            }
        }
    }

    #[test]
    fn reset_order_also_orders_numbers_and_records() {
        let _journal = journal_test_lock();
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        unsafe { (at as *mut u8).write_volatile(1) };
        let page = at / PAGE;
        let first = reset_order(page, page + 1);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                POOLS.with(|value| value.set(Some(false)));
                started_tx.send(()).unwrap();
                done_tx.send(writes(at, at + PAGE, false).unwrap()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(done_rx.try_recv().is_err());
            let (older, _) = guarded_answer(page, page + 1, &[page]);
            drop(first);
            let (newer, pages) = done_rx.recv().unwrap();
            assert!(newer > older);
            assert_eq!(pages, [(page, newer)]);
        });
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A copy's whole range is known mapped, and only that: not one byte
    /// past its end, and nothing before it was copied.
    #[test]
    fn a_copy_marks_the_blocks_it_took() {
        let _journal = journal_test_lock();
        let at = 0x7000_0000_0000usize;
        let len = 3 * BLOCK;
        assert!(!mapped(at, len), "nothing copied there yet");
        copied(at, len);
        assert!(mapped(at, len));
        assert!(mapped(at + BLOCK, BLOCK), "a range inside it");
        assert!(!mapped(at - BLOCK, BLOCK), "a block before it");
        assert!(!mapped(at + len, BLOCK), "a block past it");
        assert!(!mapped(at, len + 1), "a range that runs past it");
        assert!(!mapped(at, 0));
    }

    /// A write-back job's blocks are inaccessible to the title until the
    /// job released them, and a thread that faulted there waits for it.
    #[test]
    fn write_back_blocks_wait_for_their_job() {
        let _journal = journal_test_lock();
        // SAFETY: two blocks of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        let done = std::sync::Arc::new(Done::default());
        let blocks = back_begin(&done, at, at + 2 * BLOCK);
        assert_eq!(blocks, [at / BLOCK, at / BLOCK + 1]);
        assert!(!writable_now(at), "held inaccessible while the job runs");
        assert!(back_pending(at + BLOCK).is_some());
        done.finish();
        back_release(&blocks, &done);
        assert!(writable_now(at), "given back after the job");
        assert!(back_pending(at).is_none());
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A block a later job holds stays inaccessible when an earlier job
    /// releases it: only the job that last held it gives it back.
    #[test]
    fn a_held_block_passes_to_the_next_job() {
        let _journal = journal_test_lock();
        // SAFETY: one block of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        let first = std::sync::Arc::new(Done::default());
        let second = std::sync::Arc::new(Done::default());
        let taken = back_begin(&first, at, at + BLOCK);
        assert_eq!(taken, [at / BLOCK]);
        let taken_again = back_begin(&second, at, at + BLOCK);
        assert_eq!(taken_again, [at / BLOCK], "the later job takes the block");
        first.finish();
        back_release(&taken, &first);
        assert!(!writable_now(at), "still inaccessible: the later job's bytes are not in");
        second.finish();
        back_release(&taken_again, &second);
        assert!(writable_now(at), "given back by the job that last held it");
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A title write into a block a write-back job holds takes the access
    /// fault, and the handler opens the block and lets the write through.
    /// Run cb-1 died here: the handler had not been installed (nothing had
    /// guarded a view in that run yet), so the title's own store to a held
    /// block was an unhandled 0xc0000005.
    #[test]
    fn a_title_write_into_a_held_block_is_resolved() {
        let _journal = journal_test_lock();
        // SAFETY: two blocks of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        let done = std::sync::Arc::new(Done::default());
        let held = back_begin(&done, at, at + 2 * BLOCK);
        assert_eq!(held, [at / BLOCK, at / BLOCK + 1]);
        assert!(!writable_now(at), "held inaccessible while the job runs");
        // The job finishes while the title's store is on its way (as the
        // worker does: the blocks are given back, then the job is done).
        let job = {
            let done = done.clone();
            let held = held.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                back_release(&held, &done);
                done.finish();
            })
        };
        // A store in the first block, then one that straddles the two: the
        // handler waits for the job, which opens both.
        // SAFETY: our own memory; the instruction runs again once the block
        // is given back.
        unsafe {
            (at as *mut u64).write_volatile(0x1234_5678);
            // (the last four bytes of the first block and the first of the
            // second: unaligned on purpose)
            ((at + BLOCK - 4) as *mut u32).write_volatile(0x9abc_def0);
        }
        job.join().expect("the job's thread");
        assert!(writable_now(at));
        assert!(back_pending(at).is_none());
        // SAFETY: our memory, written just above.
        assert_eq!(unsafe { ((at + BLOCK - 4) as *const u32).read() }, 0x9abc_def0);
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        let _journal = journal_test_lock();
        // The kernel's answering: the pools' guarding is off unless
        // `VICTORY_GUARD_POOLS=1`.
        tests::POOLS.with(|v| v.set(Some(false)));
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        // Nor is a plain allocation write-watched: with the copies' pool
        // guarding off (the old way, `VICTORY_GUARD_COPY_POOLS=0`) its asking
        // is not answered at all, and a copy takes the whole range at every
        // refresh for the rest of the run.
        // A private region of ours, page aligned: a copy's destination is
        // title memory we own. The old fixture was a page of the CRT heap,
        // unaligned, so the guard took in the block after it — our own
        // allocator's memory. Making that read-only made our allocator fault
        // inside the guard's own shard lock, and the run hung.
        let plain = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(plain, 0);
        let (at, plane_end) = (plain, plain + PAGE);
        set_guard_copy_pools(Some(false));
        assert!(writes(at, plane_end, true).is_none(), "not answered, as before");
        // With the pool guarded for a copy (the default) the range is
        // answered: its blocks once, then nothing newer. Set explicitly, so
        // a run with `VICTORY_GUARD_COPY_POOLS=0` in its environment still
        // tests the guarded path.
        set_guard_copy_pools(Some(true));
        let (first, held) = writes(at, plane_end, true).expect("guarded for a copy");
        // The blocks the range covers: PAGE bytes of heap are not PAGE
        // aligned, so the range may take in two of them (under WARP's
        // allocator it does).
        let blocks = (at / PAGE..plane_end.div_ceil(PAGE)).collect::<Vec<_>>();
        assert_eq!(
            held.iter().map(|&(page, _)| page).collect::<Vec<_>>(),
            blocks,
            "the blocks, once"
        );
        let (_, again) = writes(at, plane_end, true).expect("guarded");
        assert_eq!(again.iter().filter(|&&(_, w)| w > first).count(), 0, "nothing written since");
        crate::guards::open(at, plane_end);
        tests::POOLS.with(|v| v.set(None));
        // SAFETY: our allocations.
        unsafe {
            VirtualFree(plain as *mut _, 0, MEM_RELEASE);
            VirtualFree(p as *mut _, 0, MEM_RELEASE);
        }
    }

    /// A copy's destination that is not whole blocks is not ours to guard:
    /// the guard would make the block it rounds into read-only, and that
    /// block holds memory somebody else writes — our own heap, which faulted
    /// back into the guard's own lock and hung the run. Refused and counted,
    /// the copy takes its range whole.
    #[test]
    fn a_range_that_is_not_whole_blocks_is_not_guarded() {
        tests::POOLS.with(|v| v.set(Some(false)));
        set_guard_copy_pools(Some(true));
        let mut plain = vec![0u8; PAGE];
        plain[0] = 1;
        let at = plain.as_mut_ptr() as usize;
        // A page of heap is not page aligned (the fixture that hung).
        let before = guard_refusals();
        assert!(writes(at, at + PAGE, true).is_none(), "refused");
        assert_eq!(guard_refusals(), before + 1, "counted");
        set_guard_copy_pools(None);
        tests::POOLS.with(|v| v.set(None));
    }

    /// A pool's asking is the guards' to answer: the kernel is not asked at
    /// all, the title's write is reported at the next asking, and a span
    /// nothing was written in reports nothing. The default off asks the
    /// default off) asks the kernel, as every asking did before.
    #[test]
    fn a_pools_asking_is_answered_by_the_guards_not_the_kernel() {
        let _journal = journal_test_lock();
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        forget_askings();
        tests::POOLS.with(|v| v.set(Some(true)));
        tests::VERIFY.with(|v| v.set(Some(0)));
        let (start, end) = (p, p + 4 * PAGE);
        let before = kernel_asked();
        let (now, written) = writes(start, end, false).expect("a pool the guards hold");
        // A block that was never guarded counts as written: the first asking
        // reports the whole range (the copy takes it whole, as a new
        // texture's first refresh does anyway).
        assert_eq!(written.len(), 4, "a block never guarded counts as written");
        assert_eq!(kernel_asked(), before, "the guards answered, the kernel was not asked");
        let (_, written) = writes(start, end, false).expect("a pool the guards hold");
        assert!(written.iter().all(|&(_, w)| w <= now), "nothing written since");
        assert_eq!(kernel_asked(), before, "still the guards");
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (later, written) = writes(start, end, false).expect("a pool the guards hold");
        assert!(later > now);
        assert_eq!(
            written.iter().filter(|&&(_, w)| w == later).map(|&(p, _)| p).collect::<Vec<_>>(),
            [p / PAGE + 1],
            "the page the title wrote, under this asking"
        );
        assert_eq!(kernel_asked(), before, "still the guards");
        tests::VERIFY.with(|v| v.set(None));
        // The default: the kernel answers the same range, as before.
        tests::POOLS.with(|v| v.set(Some(false)));
        let (_, off) = writes(start, end, false).expect("watched");
        assert!(kernel_asked() > before, "off, the kernel is asked");
        assert!(off.iter().any(|&(page, _)| page == p / PAGE + 1));
        forget_askings();
        // The pool is gone: the guards drop what they held of it, so the next
        // allocation at these addresses starts unknown (as `XMemFree` tells
        // the layer).
        title_memory_remapped(start, 4 * PAGE);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// The kernel's asking verifies the guards: on the frame a range is due it
    /// is asked as well, and a page only it found (a write that made no fault:
    /// the card's, or one through another view) is kept in the journal, so no
    /// copy goes without it.
    #[test]
    fn the_kernels_asking_verifies_what_the_guards_hold() {
        let _journal = journal_test_lock();
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        forget_askings();
        tests::POOLS.with(|v| v.set(Some(true)));
        tests::VERIFY.with(|v| v.set(Some(1)));
        let (start, end) = (p, p + 4 * PAGE);
        let (_, written) = writes(start, end, false).expect("a pool the guards hold");
        assert_eq!(written.len(), 4, "a block never guarded counts as written");
        let before = kernel_asked();
        // SAFETY: committed pages of ours.
        unsafe { ((p + 2 * PAGE + 8) as *mut u8).write(1) };
        let (now, written) = writes(start, end, false).expect("a pool the guards hold");
        assert!(kernel_asked() > before, "the verifier asked the kernel");
        assert!(
            written.iter().any(|&(page, w)| page == p / PAGE + 2 && w == now),
            "the page the asking found is kept"
        );
        tests::POOLS.with(|v| v.set(None));
        tests::VERIFY.with(|v| v.set(None));
        forget_askings();
        title_memory_remapped(start, 4 * PAGE);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A range no asking of the submission since reported written is known
    /// clean from the journal, and the write it did report is not taken for
    /// a write in another range of the same asking: a copy is clean while
    /// nothing in *its* range was written.
    #[test]
    fn a_range_is_known_clean_until_an_asking_finds_it_written() {
        let _journal = journal_test_lock();
        // A pool the guards hold is left to `writes`, not asked ahead: this
        // is the asking of everything else, with the pools' guarding off
        // (its default).
        tests::POOLS.with(|v| v.set(Some(false)));
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        // A journal of its own: the askings and pages of the tests that ran
        // before stay out of it, and its own out of theirs.
        forget_askings();
        let (a, b) = (p, p + 2 * PAGE);
        let (c, d) = (p + 2 * PAGE, p + 4 * PAGE);
        ask_once(&[(a, b), (c, d)]);
        let (since, pages) = asked(a, b).expect("asked ahead");
        assert!(pages.is_empty(), "nothing written yet");
        assert_eq!(written_since(a, b, since), Some(false));
        assert_eq!(written_since(c, d, since), Some(false));
        // SAFETY: our pages.
        unsafe { ((c + 8) as *mut u8).write_volatile(7) };
        // The submission asks again: the write is the new asking's, and it
        // is the other range's.
        ask_once(&[(a, b), (c, d)]);
        let (now, _) = asked(a, b).expect("asked again");
        assert!(now > since);
        assert_eq!(written_since(a, b, since), Some(false), "another range's write");
        assert_eq!(written_since(c, d, since), Some(true), "its own write");
        // Asked from the asking that found it: nothing since.
        assert_eq!(written_since(c, d, now), Some(false));
        // A range no asking holds is not answered for: the kernel must be.
        let mut plain = vec![0u8; PAGE];
        assert_eq!(
            written_since(plain.as_mut_ptr() as usize, plain.as_ptr() as usize + PAGE, since),
            None
        );
        ask_once_end();
        forget_askings();
        tests::POOLS.with(|v| v.set(None));
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// Askings of one submission that meet or overlap are one range each
    /// run, so a page range lying in any of them is covered: the journal
    /// answers for it instead of the kernel being asked again.
    #[test]
    fn overlapping_askings_are_merged_so_a_range_inside_any_is_covered() {
        let _journal = journal_test_lock();
        let mut ahead = vec![(0usize, 12_000usize), (8_000, 20_000)];
        normalize_ahead(&mut ahead);
        assert_eq!(ahead, [(0, 20_000)], "the second overlapping the first");
        assert!(covers(&ahead, 0, 12_000));
        assert!(covers(&ahead, 8_000, 20_000), "the one the scan missed");
        assert!(covers(&ahead, 5_000, 15_000), "across both");
        assert!(!covers(&ahead, 12_000, 21_000), "past both");
        assert!(!covers(&ahead, 0, 20_001), "one page more");
        // Ends that meet are one range too, and the order they come in does
        // not matter.
        let mut ahead = vec![(40, 50), (0, 5), (5, 40)];
        normalize_ahead(&mut ahead);
        assert_eq!(ahead, [(0, 50)]);
        // Ranges apart stay apart.
        let mut ahead = vec![(0, 5), (40, 50), (10, 20)];
        normalize_ahead(&mut ahead);
        assert_eq!(ahead, [(0, 5), (10, 20), (40, 50)]);
        assert!(!covers(&ahead, 5, 10), "the gap between them");
        assert!(covers(&ahead, 10, 20));
    }

    /// What a copy's "nothing wrote it" check costs, per copy, over the
    /// copies one submission asks about together: the refresh's [`writes`]
    /// (the journal's answer, after which the copy's pages are walked)
    /// against [`written_since`], which the flush's skip asks instead. Run
    /// under Wine:
    /// `cargo xwin test --no-run --target x86_64-pc-windows-msvc -p victory-gpu`,
    /// then the exe with `--ignored --nocapture clean_copies_bench`.
    #[test]
    #[ignore]
    fn clean_copies_bench() {
        let _journal = journal_test_lock();
        use std::time::Instant;
        for pages in [1024usize, 4096] {
            // SAFETY: a fresh write-watched allocation, freed at the end.
            let p = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    pages * PAGE,
                    MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(p, 0);
            forget_askings();
            let ranges: Vec<(usize, usize)> = (0..pages).map(|i| (p + i * PAGE, p + (i + 1) * PAGE)).collect();
            ask_once(&ranges);
            let (since, _) = asked(ranges[0].0, ranges[0].1).expect("asked ahead");
            // The refresh's ask, as `copy_stale` makes it.
            let started = Instant::now();
            let mut reported = 0;
            for &(a, b) in &ranges {
                reported += writes(a, b, true).map_or(0, |(_, written)| written.len());
            }
            let before = started.elapsed();
            // The flush's skip.
            let started = Instant::now();
            let clean = ranges.iter().filter(|&&(a, b)| written_since(a, b, since) == Some(false)).count();
            let after = started.elapsed();
            println!(
                "{pages} copies: writes() {} ns each, written_since() {} ns each; {clean} clean, {reported} pages reported",
                before.as_nanos() / pages as u128,
                after.as_nanos() / pages as u128,
            );
            ask_once_end();
            forget_askings();
            // SAFETY: our allocation.
            unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        let _journal = journal_test_lock();
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable_now(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()), "nothing written since");
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let _journal = journal_test_lock();
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    /// The destination check's answers, from hand-built regions: committed
    /// read-write is a destination, reserved or read-only is not, and a
    /// runtime remap (or a failed kernel write) makes it ask again.
    #[test]
    fn a_destination_is_writable_only_where_the_region_is_committed_read_write() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        // SAFETY: a fresh allocation of ours, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * BLOCK,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        assert!(writable(at, 4 * BLOCK));
        assert!(writable(at + BLOCK, BLOCK), "a range inside the region");
        assert!(!writable(at, 0), "an empty range is never a destination");
        // Reserved but not committed: no destination, and the copy falls
        // back to the kernel (which fails) instead of faulting.
        // SAFETY: a fresh reservation of ours, freed at the end.
        let hole = unsafe {
            VirtualAlloc(std::ptr::null(), BLOCK, MEM_RESERVE, PAGE_READWRITE)
        } as usize;
        assert_ne!(hole, 0);
        assert!(!writable(hole, BLOCK));
        assert!(!writable(hole - BLOCK, 2 * BLOCK), "a range reaching into it");
        // The title commits it with no notification: the old answer stands
        // until a failed write says otherwise.
        // SAFETY: committing our own reservation.
        let committed = unsafe { VirtualAlloc(hole as *mut _, BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(committed as usize, hole);
        assert!(!writable(hole, BLOCK));
        write_failed(hole, BLOCK);
        assert!(writable(hole, BLOCK), "asked again after a failed write");
        // The runtime makes a block of the first region read-only: it asks
        // again and the block is no destination.
        let mut old = 0;
        assert_ne!(
            unsafe { VirtualProtect((at + 2 * BLOCK) as *mut _, BLOCK, PAGE_READONLY, &mut old) },
            0
        );
        title_memory_remapped(at + 2 * BLOCK, BLOCK);
        assert!(writable(at, 2 * BLOCK), "the blocks before it are still so");
        assert!(!writable(at, 3 * BLOCK), "a read-only block in the range");
        assert!(!writable(at + 2 * BLOCK, BLOCK));
        // SAFETY: the allocations above.
        unsafe {
            title_memory_remapped(at, 4 * BLOCK);
            title_memory_remapped(hole, BLOCK);
            VirtualFree(at as *mut _, 0, MEM_RELEASE);
            VirtualFree(hole as *mut _, 0, MEM_RELEASE);
        }
    }

    /// An answer kept for a later block does not hide a change in a block
    /// that an earlier answer also covers.
    #[test]
    fn a_change_forgets_every_answer_that_reaches_into_it() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        // SAFETY: a fresh allocation of ours, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * BLOCK,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        // An answer asked at the second block, then one asked at the first
        // that covers it too: two entries, `[2, 4)` and `[0, 4)`.
        assert!(writable(at + 2 * BLOCK, 2 * BLOCK));
        assert!(writable(at, 4 * BLOCK));
        // The title makes the last block read-only and tells us: both
        // answers reach into it and must go.
        let mut old = 0;
        assert_ne!(
            unsafe { VirtualProtect((at + 3 * BLOCK) as *mut _, BLOCK, PAGE_READONLY, &mut old) },
            0
        );
        title_memory_remapped(at + 3 * BLOCK, BLOCK);
        assert!(!writable(at + 3 * BLOCK, BLOCK), "the `[0, 4)` answer must go too");
        assert!(writable(at, 3 * BLOCK), "what did not change is still known");
        // SAFETY: the allocation above.
        unsafe {
            title_memory_remapped(at, 4 * BLOCK);
            VirtualFree(at as *mut _, 0, MEM_RELEASE);
        }
    }

    /// A block our own guard made read-only is no destination until a write
    /// opens it ([`writing`]).
    #[test]
    fn a_guarded_block_is_no_destination_until_the_write_opens_it() {
        let _journal = journal_test_lock();
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some());
        assert!(!writable(view, BLOCK), "read-only by our hand");
        assert!(!writable(view, 2 * BLOCK));
        // The kernel write's announcement opens it first.
        writing(view, BLOCK);
        assert!(writable(view, BLOCK));
        assert!(!writable(view, 2 * BLOCK), "the other block stays guarded");
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ });
        }
        title_memory_remapped(view, 2 * BLOCK);
    }

    /// The cached destination check costs well under 2 % of the copy it
    /// guards (the perf ledger has the numbers): 64 cached answers against
    /// copies of the same 16 MiB range.
    #[test]
    fn the_destination_check_costs_under_two_percent_of_the_copy() {
        let _journal = journal_test_lock();
        use std::time::Instant;
        const N: usize = 16 << 20;
        const ROUNDS: u32 = 64;
        // SAFETY: two regions of ours, freed at the end.
        let (src, dst) = unsafe {
            (
                VirtualAlloc(std::ptr::null(), N, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
                    .cast::<u8>(),
                VirtualAlloc(std::ptr::null(), N, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
                    .cast::<u8>(),
            )
        };
        assert!(!src.is_null() && !dst.is_null());
        assert!(writable(dst as usize, N), "the first answer asks");
        let started = Instant::now();
        let mut all = true;
        for _ in 0..ROUNDS {
            all &= writable(dst as usize, N);
        }
        let check = started.elapsed();
        let started = Instant::now();
        for _ in 0..ROUNDS {
            // SAFETY: two mapped regions of ours, apart; the source is read
            // and the destination written.
            unsafe { std::ptr::copy_nonoverlapping(src, dst, N) };
        }
        let copy = started.elapsed();
        assert!(all);
        println!(
            "destination check {} ns a copy, {N} bytes copied in {} us, {:.4} % of the copy",
            check.as_nanos() / ROUNDS as u128,
            copy.as_micros() / ROUNDS as u128,
            check.as_nanos() as f64 / copy.as_nanos() as f64 * 100.0,
        );
        assert!(
            check * 50 < copy,
            "the check costs {check:?} of {copy:?} in copies"
        );
        // SAFETY: the allocations above.
        unsafe {
            title_memory_remapped(dst as usize, N);
            VirtualFree(src as *mut _, 0, MEM_RELEASE);
            VirtualFree(dst as *mut _, 0, MEM_RELEASE);
        }
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{
        BLOCK, copy_back_skips_remapped, remapped, remapped_any, remapped_taken, title_memory_reading,
        title_memory_remapped, title_memory_remapping,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// A change of mapping waits for a read in place that is under way, and
    /// a read in place waits for a change of mapping.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        let _journal = super::journal_test_lock();
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!read.load(Ordering::SeqCst), "a read in place began during a change of mapping");
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }

    /// A range the runtime unmapped or remapped is marked once, joined with
    /// the marks next to it, until a copy takes it afresh: that is what keeps
    /// a pending copy-back's bytes out of a new mapping.
    #[test]
    fn remapping_marks_blocks_until_a_copy_takes_them_again() {
        let _journal = super::journal_test_lock();
        if !copy_back_skips_remapped() {
            return; // VICTORY_SKIP_REMAPPED=0: nothing is marked.
        }
        // SAFETY: two blocks of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(std::ptr::null(), 2 * BLOCK, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        } as usize;
        assert_ne!(at, 0, "the pool allocates");
        // Counted over this allocation only: other tests remap memory of
        // their own, and the runner may have them in the air.
        let marks = |at: usize| {
            remapped().range(..at + 2 * BLOCK).filter(|&(&s, &(e, _))| e > at && s < at + 2 * BLOCK).count()
        };
        let saved = remapped().clone();
        remapped().clear();
        title_memory_remapped(at, BLOCK);
        assert!(remapped_any(at, BLOCK), "a remapped range is marked");
        assert!(!remapped_any(at + BLOCK, BLOCK), "the range after it is not");
        assert_eq!(marks(at), 1, "one range holds both blocks");
        remapped_taken(crate::watch::remap_generation(), at, BLOCK);
        assert!(!remapped_any(at, BLOCK), "a copy that takes it clears the mark");
        // Two ranges remapped apart join; a copy taking one leaves the other.
        title_memory_remapped(at + BLOCK, BLOCK);
        title_memory_remapped(at, BLOCK);
        assert_eq!(marks(at), 1, "the marks next to each other join");
        assert!(remapped_any(at, 2 * BLOCK));
        remapped_taken(crate::watch::remap_generation(), at + BLOCK, BLOCK);
        assert!(!remapped_any(at + BLOCK, BLOCK), "the taken bytes are current");
        assert!(remapped_any(at, BLOCK), "the range no copy took stays marked");
        assert_eq!(crate::watch::unremapped_runs(at, 2 * BLOCK), [(at + BLOCK, at + 2 * BLOCK)]);
        remapped_taken(u64::MAX, at, 2 * BLOCK);
        assert!(!remapped_any(at, 2 * BLOCK));
        assert_eq!(crate::watch::unremapped_runs(at, 2 * BLOCK), [(at, at + 2 * BLOCK)]);
        // A remap that lands after the copy read its bytes stays marked: the
        // snapshot holds the old mapping's bytes.
        let generation = crate::watch::remap_generation();
        title_memory_remapped(at, BLOCK);
        remapped_taken(generation, at, BLOCK);
        assert!(remapped_any(at, BLOCK), "a mark the copy did not read stands");
        remapped_taken(crate::watch::remap_generation(), at, BLOCK);
        assert!(!remapped_any(at, BLOCK), "a copy that read it clears it");
        *remapped() = saved;
        // SAFETY: our allocation.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A pool the kernel will not write-watch (private, committed, not
    /// reserved with `MEM_WRITE_WATCH`: exactly what `GetWriteWatch`
    /// refuses) is guarded for a copy instead of its whole range being
    /// copied at every refresh. The first asking names every block (the
    /// whole range once, what the old path copied), and after it only the
    /// blocks the title wrote: never more than was written. Fails on the
    /// old path at the second step (`None`: the copy is `NOT_WATCHED` and
    /// takes its whole window at every refresh for the rest of the run).
    #[test]
    fn a_pool_the_kernel_cannot_watch_is_guarded_for_a_copy() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        const N: usize = 4 * BLOCK;
        // SAFETY: a fresh private region of four blocks, read-write, and not
        // write-watched.
        let start = unsafe { VirtualAlloc(std::ptr::null(), N, MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(start, 0);
        struct Pool(usize);
        impl Drop for Pool {
            fn drop(&mut self) {
                crate::guards::open(self.0, self.0 + N);
                // The guards' sets for the range go with the memory, so a
                // later allocation at the same address starts clean.
                super::title_memory_remapped(self.0, N);
                // SAFETY: our allocation.
                unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
            }
        }
        let _pool = Pool(start);
        super::forget_askings();
        let pages = |written: &[(usize, u64)]| written.iter().map(|&(page, _)| page).collect::<Vec<_>>();
        // The old way: no answer, so the caller copies the whole range.
        super::set_guard_copy_pools(Some(false));
        assert_eq!(super::writes(start, start + N, true), None, "the kernel does not watch a pool");
        // The switch is set for the test, not inherited from the environment,
        // so this holds whatever a run's `VICTORY_GUARD_COPY_POOLS` says.
        super::set_guard_copy_pools(Some(true));
        // The first asking takes every block: the whole range, once.
        let (first, written) = super::writes(start, start + N, true).expect("guarded for a copy");
        assert_eq!(pages(&written), (start / super::PAGE..(start + N) / super::PAGE).collect::<Vec<_>>());
        // The caller copies a page whose write number is newer than its last
        // copy (`Shadow::copy_stale`): nothing here is.
        let newer = |written: &[(usize, u64)], since: u64| written.iter().filter(|&&(_, w)| w > since).count();
        let (quiet, written) = super::writes(start, start + N, true).expect("guarded");
        assert_eq!(newer(&written, first), 0, "nothing written since: nothing newer");
        // One byte in the third block: that block, and only it.
        // SAFETY: inside our committed region; the guard's fault hands the
        // block back before the write goes on.
        unsafe { ((start + 2 * BLOCK + 7) as *mut u8).write_volatile(1) };
        let (found, written) = super::writes(start, start + N, true).expect("guarded");
        let fresh: Vec<usize> = written.iter().filter(|&&(_, w)| w > quiet).map(|&(page, _)| page).collect();
        assert_eq!(fresh, vec![(start + 2 * BLOCK) / super::PAGE], "one block written, one named");
        // And the next asking, with nothing written, is quiet again: nothing
        // newer than the asking that found the write (the journal keeps the
        // write's number; an asking with nothing to report does not re-date
        // it).
        let (_, written) = super::writes(start, start + N, true).expect("guarded");
        assert_eq!(newer(&written, found), 0, "{written:?}");
    }

    /// The class removed, measured: a 64 MiB pool the kernel refuses, three
    /// samples a side. The old path answers `None` (the copy takes its whole
    /// window, 64 MiB, at every refresh); the guarded path names the 16
    /// blocks written and nothing else.
    #[test]
    #[ignore]
    fn bench_copy_pools_guard() {
        use std::time::Instant;
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        const N: usize = 64 << 20;
        // SAFETY: a fresh private region, read-write, not write-watched.
        let start = unsafe { VirtualAlloc(std::ptr::null(), N, MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(start, 0);
        super::forget_askings();
        let was = super::set_guard_copy_pools(Some(true));
        // What a copy takes is the blocks whose write number is newer than
        // its last copy (`Shadow::copy_stale`): `writes` answers with the
        // range's whole journal, so counting its length would name all 1,024
        // blocks for ever, written or not.
        let fresh = |written: &[(usize, u64)], since: u64| {
            written.iter().filter(|&&(_, w)| w > since).count()
        };
        let (mut since, first) = super::writes(start, start + N, true).expect("the first asking takes every block");
        println!("{N} pool: first asking names {} blocks (the whole range once)", first.len());
        let (_, quiet) = super::writes(start, start + N, true).expect("guarded");
        assert_eq!(fresh(&quiet, since), 0, "nothing written yet: nothing named");
        let mut named = 0;
        let mut us = 0;
        for _ in 0..3 {
            for block in 0..16 {
                // SAFETY: inside the region; the guard hands the block back.
                unsafe { ((start + block * BLOCK + 3) as *mut u8).write_volatile(1) };
            }
            let started = Instant::now();
            let (now, written) = super::writes(start, start + N, true).expect("guarded");
            let took = started.elapsed();
            let took_blocks = fresh(&written, since);
            since = now;
            assert_eq!(took_blocks, 16, "16 blocks written, 16 named");
            named += took_blocks;
            us += took.as_micros() as u64;
            println!("sample: 16 blocks written, {took_blocks} blocks named, {} us, {} bytes copied (was {N})", took.as_micros(), took_blocks * BLOCK);
        }
        println!("three samples: {named} blocks named, {us} us, {} bytes (the whole-range path copied {N} bytes each time)", named * BLOCK);
        super::set_guard_copy_pools(was);
        crate::guards::open(start, start + N);
        // SAFETY: our allocation.
        unsafe { VirtualFree(start as *mut _, 0, MEM_RELEASE) };
    }
}
#[cfg(test)]
mod back_tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// A kernel write of ours into a block a job holds waits for the job
    /// before it opens it: opening first would let the job's merge write the
    /// GPU's older bytes over the title's, and lose the merge.
    #[test]
    fn back_open_waits_for_the_job_that_holds_the_block() {
        let _journal = super::journal_test_lock();
        use std::sync::mpsc;
        // SAFETY: one block of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(std::ptr::null(), BLOCK, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        } as usize;
        assert_ne!(at, 0);
        let done = std::sync::Arc::new(Done::default());
        let blocks = back_begin(&done, at, at + BLOCK);
        assert_eq!(blocks, [at / BLOCK]);
        let (opened, when) = mpsc::channel();
        let opener = std::thread::spawn(move || {
            back_open(at, 1);
            opened.send(()).unwrap();
        });
        assert!(
            when.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "back_open opened a block a job still holds (the job's merge is lost)"
        );
        assert!(!writable_now(at));
        done.finish();
        assert!(
            when.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "back_open goes on once the job is done"
        );
        assert!(writable_now(at), "the title's write has the block");
        assert!(back_pending(at).is_none());
        opener.join().unwrap();
        back_release(&blocks, &done);
        // SAFETY: our allocation.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A read of a block a job holds returns within the deadline and names
    /// the job it waited for; on the deadline the block is the title's again,
    /// so the instruction that faulted goes through instead of the title's
    /// thread never running again.
    #[test]
    fn a_read_of_a_held_block_returns_within_the_deadline_and_names_the_job() {
        let _journal = super::journal_test_lock();
        use std::sync::mpsc;
        // SAFETY: one block of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(std::ptr::null(), BLOCK, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        } as usize;
        assert_ne!(at, 0);
        let done = std::sync::Arc::new(Done::default());
        let id = done.id();
        let blocks = back_begin(&done, at, at + BLOCK);
        assert_eq!(blocks, [at / BLOCK]);
        let (back, when) = mpsc::channel();
        let waited = done.clone();
        let reader = std::thread::spawn(move || {
            wait_held(at);
            back.send((waited.id(), waited.finished())).unwrap();
        });
        assert!(
            when.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "the read waits while the job is outstanding"
        );
        let (named, finished) = when
            .recv_timeout(crate::writeback::MERGE_DEADLINE + std::time::Duration::from_secs(2))
            .expect("the read returns within the deadline");
        assert_eq!(named, id, "the wait names the job it waited for");
        assert!(!finished, "the job never finished: only the deadline returned it");
        assert!(writable_now(at), "the block is given back to the title");
        assert!(back_pending(at).is_none(), "and no job holds it any more");
        reader.join().unwrap();
        back_release(&blocks, &done);
        // SAFETY: our allocation.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }
}
