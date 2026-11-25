//! One test at a time on the software adapter.
//!
//! `d3d10warp.dll` is one software rasterizer for the whole process, and the
//! tests that ask the software adapter for a device (`indirect`'s four,
//! `inplace`'s three, and the adapter check in `host`) each open one of their
//! own; under `VICTORY_TEST_ADAPTER=hw` the same tests take it with a card,
//! so a run on the real adapter is one device test at a time too. The harness
//! runs the tests in parallel by default, and on windows-latest four of those
//! devices at once fault the test exe with
//! `STATUS_ACCESS_VIOLATION` (0xc0000005) inside WARP: `indirect`'s four
//! alone crashed 1 run in 12 with `--test-threads=4` and every one of the
//! four passed 12 of 12 alone and 12 of 12 with `--test-threads=1`, and in
//! the whole suite the parallel run also fails `warp_the_self_check_reads_
//! and_writes_our_own_page` (11 of 20, the self-check's bytes never
//! arriving). Nothing in a single test is wrong, and no test skips itself:
//! they take this lock and run in turn.

use std::sync::{Mutex, MutexGuard};

/// Held for the length of a test that opens a device on the software
/// adapter: WARP is one rasterizer, so its devices are opened and driven one
/// test at a time.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
