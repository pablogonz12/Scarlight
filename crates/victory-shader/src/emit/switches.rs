//! Off switches for translation changes that alter the host shader, each read
//! once per process (`VICTORY_<NAME>=0` brings the old form back; unset is on).
//!
//! A switch here changes the translation, so every name must also be in
//! `victory-gpu`'s shader cache key (`shader_cache::build`), or a run with the
//! switch flipped is served the translations an earlier run cached without it.

use std::sync::OnceLock;

/// `name` switched on: unset or anything but `0`.
fn enabled(name: &str) -> bool {
    std::env::var_os(name).is_none_or(|v| v != "0")
}

/// The wave-wide SCC of `s_cmp_eq/lg` of two lane masks (eq iff no bit
/// differs, lg iff one does); `VICTORY_MASK_SCC_LANES=0` keeps the per-lane
/// XNOR/XOR form.
pub fn mask_scc_wave() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled("VICTORY_MASK_SCC_LANES"))
}

/// A `LaneExit::Branch` with both arms on one block lowers as a jump;
/// `VICTORY_BRANCH_SAME_ARMS=0` keeps the two-way branch.
pub fn branch_same_arms() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled("VICTORY_BRANCH_SAME_ARMS"))
}
