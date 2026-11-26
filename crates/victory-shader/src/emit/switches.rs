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

/// A `LaneExit::Branch` with both arms on one block lowers as a jump;
/// `VICTORY_BRANCH_SAME_ARMS=0` keeps the two-way branch.
pub fn branch_same_arms() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled("VICTORY_BRANCH_SAME_ARMS"))
}

/// VOP3 integer `clamp` on add/sub saturates unsigned (carry → all ones,
/// borrow → zero); `VICTORY_INT_CLAMP=0` refuses the modifier again.
pub fn int_clamp() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled("VICTORY_INT_CLAMP"))
}
