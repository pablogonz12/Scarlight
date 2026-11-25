//! Comparing title bytes with the CPU's vector instructions
//! (`VICTORY_AVX2_COMPARE=1`; off by default, and only where the CPU has
//! AVX2, checked at run time).
//!
//! A copy-back compares a copy with its snapshot piece by piece, and the
//! bytes a shader wrote with the title's own dword by dword
//! (`crate::shadow`): some hundred MB a frame in gameplay, and every byte
//! of it is looked at. Thirty-two bytes at a time (`vpcmpeqb`) finds a
//! run of equal bytes, its first and its last difference four times as
//! fast as eight, and a differing piece is found as quickly (the eight-word
//! walk it replaces is `changed_runs`' own).
//!
//! The idea is avatarmanz/VIctory-7900XTX's (AVX2 compares of title bytes
//! under shader writes on threads of their own); this is the compare loop
//! itself, and the threads are [`crate::shadow`]'s already.
//!
//! What is not vectorised: the merge loops that store into title memory
//! (`merge_piece`, `merge_back`), which run over the bytes found differing
//! and are bound by the stores, not the compares.

use std::sync::OnceLock;

/// Whether the compares use AVX2: `VICTORY_AVX2_COMPARE=1` and the CPU has
/// it.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_AVX2_COMPARE").is_some_and(|v| v == "1") && vector())
}

/// Whether this CPU has AVX2 (and the OS keeps its registers: the standard
/// library says both).
#[cfg(target_arch = "x86_64")]
fn vector() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

#[cfg(not(target_arch = "x86_64"))]
fn vector() -> bool {
    false
}

/// Whether `a` and `b` (of one length) hold the same bytes.
pub(crate) fn equal(a: &[u8], b: &[u8]) -> bool {
    equal_on(on(), a, b)
}

/// [`equal`], with the switch given (the tests work both paths).
fn equal_on(use_vector: bool, a: &[u8], b: &[u8]) -> bool {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    if use_vector && vector() {
        // SAFETY: this CPU has `avx2` (`vector`).
        return unsafe { vector::same(a, b) };
    }
    a == b
}

/// Where `a` and `b` (of one length) first differ.
pub(crate) fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    first_difference_on(on(), a, b)
}

/// [`first_difference`], with the switch given.
fn first_difference_on(use_vector: bool, a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    if use_vector && vector() {
        // SAFETY: this CPU has `avx2` (`vector`).
        return unsafe { vector::first(&a[..n], &b[..n]) };
    }
    let whole = n / 8 * 8;
    let from = a[..whole]
        .as_chunks::<8>().0.iter()
        .zip(b[..whole].chunks_exact(8))
        .position(|(x, y)| x != y)
        .map_or(whole, |word| word * 8);
    a[from..n].iter().zip(&b[from..n]).position(|(x, y)| x != y).map(|i| from + i)
}

/// Where `a` and `b` (of one length) last differ.
pub(crate) fn last_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    last_difference_on(on(), a, b)
}

/// [`last_difference`], with the switch given.
fn last_difference_on(use_vector: bool, a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    if use_vector && vector() {
        // SAFETY: this CPU has `avx2` (`vector`).
        return unsafe { vector::last(&a[..n], &b[..n]) };
    }
    let whole = n / 8 * 8;
    if let Some(i) = a[whole..n].iter().zip(&b[whole..n]).rposition(|(x, y)| x != y) {
        return Some(whole + i);
    }
    let word = a[..whole]
        .as_chunks::<8>().0.iter()
        .zip(b[..whole].chunks_exact(8))
        .rposition(|(x, y)| x != y)?
        * 8;
    a[word..word + 8]
        .iter()
        .zip(&b[word..word + 8])
        .rposition(|(x, y)| x != y)
        .map(|i| word + i)
}

/// The runs of bytes where `now` differs from `was` (of one length), as
/// (offset, bytes) in order: every differing byte is in a run and no equal
/// byte is.
pub(crate) fn changed_runs(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    changed_runs_on(on(), now, was)
}

/// [`changed_runs`], with the switch given.
fn changed_runs_on(use_vector: bool, now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    let n = now.len().min(was.len());
    if use_vector && vector() {
        // SAFETY: this CPU has `avx2` (`vector`).
        return unsafe { vector::runs(&now[..n], &was[..n]) };
    }
    runs_scalar(&now[..n], &was[..n])
}

/// [`changed_runs`] eight bytes at a time: the fallback, and what the
/// vector walk falls back to for its tail.
fn runs_scalar(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    #[inline]
    fn all_differ_8(a: u64, b: u64) -> bool {
        let d = a ^ b;
        d.wrapping_sub(0x0101_0101_0101_0101) & !d & 0x8080_8080_8080_8080 == 0
    }
    #[inline]
    unsafe fn load_u64(ptr: *const u8, off: usize) -> u64 {
        unsafe { ptr.add(off).cast::<u64>().read_unaligned() }
    }
    let n = now.len().min(was.len());
    let (np, wp) = (now.as_ptr(), was.as_ptr());
    let mut runs = Vec::new();
    let mut i = 0;
    while i < n {
        // Equal bytes: 64 bytes at a time, then 8 bytes at a time.
        while i + 64 <= n {
            let eq = unsafe {
                load_u64(np, i) == load_u64(wp, i)
                    && load_u64(np, i + 8) == load_u64(wp, i + 8)
                    && load_u64(np, i + 16) == load_u64(wp, i + 16)
                    && load_u64(np, i + 24) == load_u64(wp, i + 24)
                    && load_u64(np, i + 32) == load_u64(wp, i + 32)
                    && load_u64(np, i + 40) == load_u64(wp, i + 40)
                    && load_u64(np, i + 48) == load_u64(wp, i + 48)
                    && load_u64(np, i + 56) == load_u64(wp, i + 56)
            };
            if !eq {
                break;
            }
            i += 64;
        }
        while i + 8 <= n && unsafe { load_u64(np, i) == load_u64(wp, i) } {
            i += 8;
        }
        while i < n && unsafe { *np.add(i) == *wp.add(i) } {
            i += 1;
        }
        if i >= n {
            break;
        }
        let first = i;
        // Differing bytes: 64 bytes at a time, then 8 bytes at a time.
        while i + 64 <= n {
            let diff = unsafe {
                all_differ_8(load_u64(np, i), load_u64(wp, i))
                    && all_differ_8(load_u64(np, i + 8), load_u64(wp, i + 8))
                    && all_differ_8(load_u64(np, i + 16), load_u64(wp, i + 16))
                    && all_differ_8(load_u64(np, i + 24), load_u64(wp, i + 24))
                    && all_differ_8(load_u64(np, i + 32), load_u64(wp, i + 32))
                    && all_differ_8(load_u64(np, i + 40), load_u64(wp, i + 40))
                    && all_differ_8(load_u64(np, i + 48), load_u64(wp, i + 48))
                    && all_differ_8(load_u64(np, i + 56), load_u64(wp, i + 56))
            };
            if !diff {
                break;
            }
            i += 64;
        }
        while i + 8 <= n && unsafe { all_differ_8(load_u64(np, i), load_u64(wp, i)) } {
            i += 8;
        }
        while i < n && unsafe { *np.add(i) != *wp.add(i) } {
            i += 1;
        }
        runs.push((first, i - first));
    }
    runs
}

#[cfg(target_arch = "x86_64")]
mod vector {
    use std::arch::x86_64::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};

    /// Thirty-two bytes of `p` at `off`, unaligned.
    ///
    /// # Safety
    /// `p.add(off)` is valid for 32 bytes (`avx2`, which the caller checked
    /// at run time).
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load(p: *const u8, off: usize) -> __m256i {
        unsafe { _mm256_loadu_si256(p.add(off).cast()) }
    }

    /// The bytes of `a` and `b` at `off` that are equal, one bit each (bit
    /// 31 is the 32nd).
    ///
    /// # Safety
    /// Both are valid for 32 bytes past `off`.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn same_mask(a: *const u8, b: *const u8, off: usize) -> i32 {
        unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(load(a, off), load(b, off))) }
    }

    /// Whether `a` and `b` (of one length) hold the same bytes.
    ///
    /// # Safety
    /// The caller checked `avx2`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn same(a: &[u8], b: &[u8]) -> bool {
        let n = a.len().min(b.len());
        let (ap, bp) = (a.as_ptr(), b.as_ptr());
        let mut i = 0;
        while i + 32 <= n {
            // SAFETY: 32 bytes are there (`i + 32 <= n`).
            if unsafe { same_mask(ap, bp, i) } != -1 {
                return false;
            }
            i += 32;
        }
        unsafe { a.get_unchecked(i..n) == b.get_unchecked(i..n) }
    }

    /// Where `a` and `b` (of one length) first differ.
    ///
    /// # Safety
    /// The caller checked `avx2`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn first(a: &[u8], b: &[u8]) -> Option<usize> {
        let n = a.len().min(b.len());
        let (ap, bp) = (a.as_ptr(), b.as_ptr());
        let mut i = 0;
        while i + 32 <= n {
            // SAFETY: 32 bytes are there.
            let mask = unsafe { same_mask(ap, bp, i) };
            if mask != -1 {
                return Some(i + (!mask as u32).trailing_zeros() as usize);
            }
            i += 32;
        }
        unsafe { a.get_unchecked(i..n) }
            .iter()
            .zip(unsafe { b.get_unchecked(i..n) })
            .position(|(x, y)| x != y)
            .map(|at| i + at)
    }

    /// Where `a` and `b` (of one length) last differ.
    ///
    /// # Safety
    /// The caller checked `avx2`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn last(a: &[u8], b: &[u8]) -> Option<usize> {
        let n = a.len().min(b.len());
        let (ap, bp) = (a.as_ptr(), b.as_ptr());
        // The tail no 32-byte block holds, then the blocks back to front.
        let tail = n % 32;
        if tail > 0 {
            let at = n - tail;
            if let Some(i) = unsafe { a.get_unchecked(at..n) }
                .iter()
                .zip(unsafe { b.get_unchecked(at..n) })
                .rposition(|(x, y)| x != y)
            {
                return Some(at + i);
            }
        }
        let mut i = n - tail;
        while i >= 32 {
            i -= 32;
            // SAFETY: 32 bytes are there (`i + 32 <= n`).
            let mask = unsafe { same_mask(ap, bp, i) };
            if mask != -1 {
                return Some(i + 31 - (!mask as u32).leading_zeros() as usize);
            }
        }
        None
    }

    /// The runs of bytes where `now` differs from `was`.
    ///
    /// # Safety
    /// The caller checked `avx2`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn runs(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
        let n = now.len().min(was.len());
        let (np, wp) = (now.as_ptr(), was.as_ptr());
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0;
        'runs: while pos < n {
            // The first differing byte from `pos`: whole blocks at a time.
            let first = loop {
                if pos + 32 <= n {
                    // SAFETY: 32 bytes are there.
                    let mask = unsafe { same_mask(np, wp, pos) } as u32;
                    if mask != u32::MAX {
                        break pos + (!mask).trailing_zeros() as usize;
                    }
                    pos += 32;
                } else {
                    while pos < n && unsafe { *np.add(pos) == *wp.add(pos) } {
                        pos += 1;
                    }
                    if pos >= n {
                        break 'runs;
                    }
                    break pos;
                }
            };
            // Its end: the first equal byte from `first` on (the byte there
            // differs, so the block's mask is not all ones).
            let mut end = first;
            loop {
                if end + 32 <= n {
                    // SAFETY: 32 bytes are there.
                    let mask = unsafe { same_mask(np, wp, end) } as u32;
                    // (One bit per byte, set where they are equal.)
                    if mask != 0 {
                        end += mask.trailing_zeros() as usize;
                        break;
                    }
                    end += 32;
                } else {
                    while end < n && unsafe { *np.add(end) != *wp.add(end) } {
                        end += 1;
                    }
                    break;
                }
            }
            out.push((first, end - first));
            pos = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{
        changed_runs, changed_runs_on, equal, equal_on, first_difference, first_difference_on, last_difference,
        last_difference_on, runs_scalar,
    };

    /// A deterministic stand-in for a random number generator (no crate of
    /// its own, no clock).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// The byte-by-byte answers, which every path must give.
    fn plain(a: &[u8], b: &[u8]) -> (bool, Option<usize>, Option<usize>, Vec<(usize, usize)>) {
        let same = a == b;
        let first = a.iter().zip(b).position(|(x, y)| x != y);
        let last = a.iter().zip(b).rposition(|(x, y)| x != y);
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (at, (x, y)) in a.iter().zip(b).enumerate() {
            if x != y {
                match runs.last_mut() {
                    Some((start, len)) if *start + *len == at => *len += 1,
                    _ => runs.push((at, 1)),
                }
            }
        }
        (same, first, last, runs)
    }

    /// Equal, first difference, last difference and the runs agree with the
    /// byte-by-byte walk on ranges of every length and at every place, with
    /// the vector path where the CPU has it: both paths against the same
    /// answer.
    #[test]
    fn the_walks_agree_byte_by_byte() {
        let mut rng = Rng(0x1234_5678_9abc_def0);
        let mut lengths = vec![0usize, 1, 2, 7, 8, 31, 32, 33, 63, 64, 65, 96, 4097];
        for _ in 0..24 {
            lengths.push(rng.below(3000));
        }
        for &len in &lengths {
            let base: Vec<u8> = (0..len).map(|i| (i * 31 + i / 7) as u8).collect();
            // As it is, one byte off at many places, and random.
            let mut cases: Vec<Vec<u8>> = vec![base.clone()];
            for _ in 0..6 {
                let mut other = base.clone();
                for _ in 0..1 + rng.below(6) {
                    if len > 0 {
                        let at = rng.below(len);
                        other[at] ^= 0x5a;
                    }
                }
                cases.push(other);
            }
            for other in cases {
                let (same, first, last, runs) = plain(&base, &other);
                for use_vector in [false, true] {
                    assert_eq!(equal_on(use_vector, &base, &other), same, "{len} {use_vector}");
                    assert_eq!(first_difference_on(use_vector, &base, &other), first, "{len} {use_vector}");
                    assert_eq!(last_difference_on(use_vector, &base, &other), last, "{len} {use_vector}");
                    assert_eq!(changed_runs_on(use_vector, &base, &other), runs, "{len} {use_vector}");
                    // The same, the other way round: the last difference is
                    // the first of the reversed range.
                    assert_eq!(last_difference_on(use_vector, &other, &base), last, "{len} {use_vector}");
                }
                assert_eq!(runs_scalar(&base, &other), runs, "{len}");
                // What the switch gives (`on()` is off unless it is set).
                assert_eq!(equal(&base, &other), same, "{len}");
                assert_eq!(first_difference(&base, &other), first, "{len}");
                assert_eq!(last_difference(&base, &other), last, "{len}");
                assert_eq!(changed_runs(&base, &other), runs, "{len}");
            }
        }
    }

    /// What the compare loop of 4,096 pages costs, eight bytes at a time
    /// and with AVX2: the piece walk a copy-back runs over a range whose
    /// snapshot it compares.
    ///
    /// `cargo test --release -p victory-gpu --lib compare_of_4k_pages -- --nocapture`
    #[test]
    fn compare_of_4k_pages_scalar_and_vector() {
        use std::time::Instant;
        const PAGES: usize = 4096;
        const PAGE: usize = 64 << 10;
        let len = PAGES * PAGE;
        let mut data = vec![0x5au8; len];
        let before = vec![0x5au8; len];
        // 1 byte in a page changed, on 64 of them: what a frame's shaders
        // leave in a range declared whole.
        for page in (0..PAGES).step_by(64) {
            data[page * PAGE + 7] ^= 0x01;
        }
        let time = |f: &dyn Fn()| {
            let started = Instant::now();
            f();
            started.elapsed()
        };
        let scalar = time(&|| {
            let mut pieces = 0;
            for page in 0..PAGES {
                let (at, n) = (page * PAGE, PAGE);
                if !runs_scalar(&data[at..at + n], &before[at..at + n]).is_empty() {
                    pieces += 1;
                }
            }
            assert_eq!(pieces, 64);
        });
        let vector_us = time(&|| {
            let mut pieces = 0;
            for page in 0..PAGES {
                let (at, n) = (page * PAGE, PAGE);
                if !changed_runs_on(true, &data[at..at + n], &before[at..at + n]).is_empty() {
                    pieces += 1;
                }
            }
            assert_eq!(pieces, 64);
        });
        // The other loop of a copy-back: one piece's first and last
        // differing byte, piece by piece (a 64 KiB piece is in the cache
        // when it is looked at the second time).
        let piece_len = PAGE;
        let mut piece = vec![0x33u8; piece_len];
        let piece_before = piece.clone();
        piece[3_000] ^= 0x01;
        piece[60_000] ^= 0x80;
        let ends_scalar = time(&|| {
            for _ in 0..PAGES {
                assert_eq!(first_difference_on(false, &piece, &piece_before), Some(3_000));
                assert_eq!(last_difference_on(false, &piece, &piece_before), Some(60_000));
            }
        });
        let ends_vector = time(&|| {
            for _ in 0..PAGES {
                assert_eq!(first_difference_on(true, &piece, &piece_before), Some(3_000));
                assert_eq!(last_difference_on(true, &piece, &piece_before), Some(60_000));
            }
        });
        eprintln!(
            "4,096 pages: piece walk eight bytes at a time {scalar:?}, thirty-two {vector_us:?}; \
             first and last difference {ends_scalar:?}, {ends_vector:?} (this CPU has AVX2: {})",
            super::vector()
        );
    }
}
