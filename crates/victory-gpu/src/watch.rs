//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crate::writeback::Done;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`])
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

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
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if j.kept {
        return;
    }
    j.asked += 1;
    j.ahead_asked = j.asked;
    j.ahead.clear();
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
        ask_run(j, &pieces[i..k]);
        i = k;
    }
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
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        j.ahead.clear();
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
    ask_ahead(ranges);
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if j.ahead.is_empty() {
        // `ask_ahead` asked nothing (fewer than two ranges).
        j.asked += 1;
        j.ahead_asked = j.asked;
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
        if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b) && ask_kernel(j, first, last) {
            j.ahead.push((first, last));
        }
    }
    j.kept = !j.ahead.is_empty();
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept || len == 0 {
        return;
    }
    j.asked += 1;
    let now = j.asked;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        j.written.insert(page, now);
    }
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`].
fn ask_run(j: &mut Journal, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a);
    ask_run(j, b);
}

/// Asks the kernel for the written pages of pages `[first, last)` and notes
/// them in the journal under the ahead asking; false (and nothing changed)
/// if the range is not write-watched memory of one reservation.
fn ask_kernel(j: &mut Journal, first: usize, last: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    let now = j.ahead_asked;
    for k in 0..count {
        let page = j.found[k] / PAGE;
        j.written.insert(page, now);
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
    let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.ahead
        .iter()
        .any(|&(a, b)| a <= first && last <= b)
        .then(|| (j.ahead_asked, j.written.range(first..last).map(|(&p, &w)| (p, w)).collect()))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // Asked ahead with the submission's other ranges: the journal has it.
    if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
        return Some((
            j.ahead_asked,
            j.written
                .range(first..last)
                .map(|(&p, &w)| (p, w))
                .collect(),
        ));
    }
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    let guarded = if failed {
        count = 0;
        if !guarded {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let guarding = std::time::Instant::now();
        let written = guard(first * PAGE, last * PAGE);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardUs, guarding.elapsed().as_micros() as u64);
        written?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let pages = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded);
    for page in pages.collect::<Vec<_>>() {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

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
        (start, end, generation) = (s, end.max(e), generation.min(g));
        map.remove(&s);
    }
    loop {
        let next = map.range(start..).next().map(|(&s, &(e, g))| (s, e, g));
        match next {
            Some((s, e, g)) if s <= end => {
                end = end.max(e);
                generation = generation.min(g);
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
    for (&s, &(e, _)) in map.range(..to) {
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
    let covered: Vec<(usize, usize, u64)> = map
        .range(..to)
        .filter(|&(_, &(e, _))| e > from)
        .map(|(&s, &(e, g))| (s, e, g))
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

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let known = !*ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1")) && {
        let g = guards();
        let first = start / BLOCK;
        g.protected.contains(&first) || g.absent.contains(&first)
    };
    if !known && !is_view(start, end) {
        return None;
    }
    install();
    let mut g = guards();
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // The range's clean blocks, in order: read off the set in one pass,
    // not looked up one block at a time. A frame asks about some 10 GB of
    // view memory, 150,000 blocks, nearly all clean: the lookups were most
    // of the 60-90 ms a frame of "guards" (run 54). What the loop adds to
    // the set meanwhile is the block it stands at or ones before it.
    let clean: Vec<usize> = g.clean.range(first..last).copied().collect();
    if clean.len() == last - first {
        // Nothing written, mapped or new since the range was last taken.
        return Some(written);
    }
    let mut next_clean = 0;
    let mut uncommitted_until = first;
    for block in first..=last {
        while next_clean < clean.len() && clean[next_clean] < block {
            next_clean += 1;
        }
        let is_clean = next_clean < clean.len() && clean[next_clean] == block;
        let mut open = block < last && !is_clean;
        if open && g.absent.contains(&block) {
            // Ordinary commits need no runtime mapping notification. Reuse
            // this asking's query for the following blocks in the same region.
            if block >= uncommitted_until {
                uncommitted_until = uncommitted_end(block);
            }
            open = block >= uncommitted_until;
            if open {
                g.absent.remove(&block);
            }
        }
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(&mut g, from, block);
        }
    }
    Some(written)
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

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else if block < uncommitted_end(block) {
            g.absent.insert(block);
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
    }
    !blocks.is_empty()
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
            let mut map = back();
            for block in a..b {
                // A block an earlier job holds keeps the protection it had
                // before the first job took it: `back_release` gives that
                // back when the last job is done with the block.
                let old = map.get(&block).map_or(old, |(_, old)| *old);
                map.insert(block, (done.clone(), old));
                held.push(block);
            }
            return 0;
        }
        -1
    };
    if take(first, last, &mut held) == 0 {
        return held;
    }
    let mut at = first;
    while at < last {
        let hole = uncommitted_end(at);
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
        done.wait();
    }
}

/// Before a kernel write into title memory `[at, at + len)`: the blocks are
/// given back writable, as [`open`] does for the guarded ones (the write is
/// the title's, or the job's own, so nothing waits for the job there).
pub(crate) fn back_open(at: usize, len: usize) {
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
        let mut g = guards();
        for block in opened {
            g.protected.remove(&block);
            g.clean.remove(&block);
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
    open(&mut guards(), at, at.saturating_add(len));
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
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    let mut g = guards();
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
    for block in absent {
        g.absent.remove(&block);
    }
    let blocks: Vec<usize> = g.protected.range(range).copied().collect();
    for block in blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
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
        let is_write = r.ExceptionInformation[0] == WRITE;
        let at = r.ExceptionInformation[1];
        // A write-back job has this block (inaccessible to reads and writes
        // alike): wait for the GPU's bytes to be put back before the
        // instruction reads or writes them.
        if let Some(done) = back_pending(at) {
            // The worker itself must never wait for its own job: the block
            // is opened and the instruction goes on.
            if crate::writeback::this_is_worker() {
                back_open(at, 1);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            done.wait();
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
        let mut g = guards();
        if open(&mut g, at, at + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
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
                let g = guards();
                tracing::warn!(
                    at = format_args!("{at:#x}"),
                    block = format_args!("{:#x}", at / BLOCK),
                    written = is_write,
                    protect = format_args!("{:#x}", if n != 0 { info.Protect } else { 0 }),
                    noaccess = n != 0 && info.Protect == PAGE_NOACCESS,
                    guarded = g.protected.contains(&(at / BLOCK)),
                    absent = g.absent.contains(&(at / BLOCK)),
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

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
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

    /// A write-back job's blocks are inaccessible to the title until the
    /// job released them, and a thread that faulted there waits for it.
    #[test]
    fn write_back_blocks_wait_for_their_job() {
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
        assert!(!writable(at), "held inaccessible while the job runs");
        assert!(back_pending(at + BLOCK).is_some());
        done.finish();
        back_release(&blocks, &done);
        assert!(writable(at), "given back after the job");
        assert!(back_pending(at).is_none());
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A block a later job holds stays inaccessible when an earlier job
    /// releases it: only the job that last held it gives it back.
    #[test]
    fn a_held_block_passes_to_the_next_job() {
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
        assert!(!writable(at), "still inaccessible: the later job's bytes are not in");
        second.finish();
        back_release(&taken_again, &second);
        assert!(writable(at), "given back by the job that last held it");
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
        assert!(!writable(at), "held inaccessible while the job runs");
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
        assert!(writable(at));
        assert!(back_pending(at).is_none());
        // SAFETY: our memory, written just above.
        assert_eq!(unsafe { ((at + BLOCK - 4) as *const u32).read() }, 0x9abc_def0);
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
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
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
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
            assert!(writable(at));
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
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
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
}
