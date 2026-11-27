//! A pixel shader's quad permutations through quad reads (`VICTORY_DX_PIXEL_QUAD`).
//!
//! The host's pixel quads are the console's: threads 4n..4n+3 of a wave, the
//! 2x2 of a pixel. A quad read is defined on helper lanes too, so a
//! broadcast or an exchange at a triangle's edge takes the value of a lane
//! the other triangle owns (the console's `ds_swizzle_b32` and DPP do); a
//! wave-lane read leaves it to the lane's own activity. With the switch off,
//! those reads go through wave lanes as they did before.

/// Whether a pixel shader's quad permutations read through quad operations.
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DX_PIXEL_QUAD").is_none_or(|v| v != "0"))
}
