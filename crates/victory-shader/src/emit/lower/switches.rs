//! The environment switches of the shader singles: each set to 0 restores what
//! main did before one of the fixes, so a worse picture can be bisected.

/// `VICTORY_BITWISE_MODS=0`: the source `abs`/`neg` of `v_mov_b32` and
/// `v_cndmask_b32` take the float path again (a host `fabs`/`fneg`, which may
/// flush a denormal or quiet an sNaN).
pub(super) fn mods() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BITWISE_MODS").is_none_or(|v| v != "0"))
}

/// `VICTORY_F16_FMA_ONCE=0`: a fused 16-bit multiply-add is FMad in f32 again,
/// rounding the sum twice.
pub(super) fn fma_once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_F16_FMA_ONCE").is_none_or(|v| v != "0"))
}

/// `VICTORY_FIELD_DWORD=0`: a byte or half load at a constant offset aligns the
/// offset with `off & !3` at run time again, which vkd3d-proton turns into a
/// per-component raw access.
pub(super) fn field() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FIELD_DWORD").is_none_or(|v| v != "0"))
}
