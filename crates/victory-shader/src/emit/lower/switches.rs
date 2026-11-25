//! The environment switches of the shader singles: each set to 0 restores what
//! main did before one of the fixes, so a worse picture can be bisected.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// `VICTORY_BITWISE_MODS=0`: the source `abs`/`neg` of `v_mov_b32` and
/// `v_cndmask_b32` take the float path again (a host `fabs`/`fneg`, which may
/// flush a denormal or quiet an sNaN).
pub(super) fn mods() -> bool {
    static MODS: OnceLock<bool> = OnceLock::new();
    on("VICTORY_BITWISE_MODS", &MODS)
}

/// `VICTORY_F16_FMA_ONCE=0`: a fused 16-bit multiply-add is FMad in f32 again,
/// rounding the sum twice.
pub(super) fn fma_once() -> bool {
    static FMA_ONCE: OnceLock<bool> = OnceLock::new();
    on("VICTORY_F16_FMA_ONCE", &FMA_ONCE)
}

/// `VICTORY_FIELD_DWORD=0`: a byte or half load at a constant offset aligns the
/// offset with `off & !3` at run time again, which vkd3d-proton turns into a
/// per-component raw access.
pub(super) fn field() -> bool {
    static FIELD: OnceLock<bool> = OnceLock::new();
    on("VICTORY_FIELD_DWORD", &FIELD)
}

/// A switch's value as `name` sets it: `0` turns it off, unset or anything else
/// leaves it on. Read once per process; a switch is set before a run, not during.
fn on(name: &'static str, slot: &'static OnceLock<bool>) -> bool {
    #[cfg(test)]
    if let Some(on) = forced(name) {
        return on;
    }
    *slot.get_or_init(|| set_from(std::env::var_os(name).as_deref()))
}

/// Whether a switch's variable says on (see [`on`]).
fn set_from(v: Option<&OsStr>) -> bool {
    v.is_none_or(|v| v != "0")
}

/// Test-only: the value a switch takes without reading the environment, so a
/// test can see both sides of one path in one process.
#[cfg(test)]
pub(super) fn force(name: &'static str, value: Option<bool>) {
    let mut f = FORCED.lock().unwrap_or_else(|e| e.into_inner());
    f.retain(|&(n, _)| n != name);
    if let Some(v) = value {
        f.push((name, v));
    }
}

/// The forced values, if any (see [`force`]).
#[cfg(test)]
fn forced(name: &'static str) -> Option<bool> {
    let f = FORCED.lock().unwrap_or_else(|e| e.into_inner());
    f.iter().find(|&&(n, _)| n == name).map(|&(_, v)| v)
}

#[cfg(test)]
static FORCED: std::sync::Mutex<Vec<(&'static str, bool)>> = std::sync::Mutex::new(Vec::new());

/// Test-only: held by every test that forces a switch, so two of them never see
/// each other's value.
#[cfg(test)]
pub(super) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static HELD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{field, fma_once, force, mods, set_from};
    use std::ffi::OsStr;

    #[test]
    fn a_switch_is_on_unless_its_variable_is_0() {
        assert!(set_from(None));
        assert!(set_from(Some(OsStr::new("1"))));
        assert!(set_from(Some(OsStr::new(""))));
        assert!(set_from(Some(OsStr::new(" 0"))));
        assert!(!set_from(Some(OsStr::new("0"))));
    }

    /// Each accessor reads its own variable, and `force` is what its path sees.
    #[test]
    fn each_switch_turns_its_own_path_off() {
        for (name, accessor) in [
            ("VICTORY_BITWISE_MODS", mods as fn() -> bool),
            ("VICTORY_F16_FMA_ONCE", fma_once as fn() -> bool),
            ("VICTORY_FIELD_DWORD", field as fn() -> bool),
        ] {
            force(name, Some(false));
            assert!(!accessor(), "{name}=0 must turn the switch off");
            force(name, Some(true));
            assert!(accessor(), "{name} on must leave the switch on");
            force(name, None);
        }
        // Cleared again: the process default, on.
        assert!(mods() && fma_once() && field());
    }
}
