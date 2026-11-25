//! The environment switches of the three translator fixes from
//! JMAN730/project-flamingo: each set to 0 restores exactly what main did
//! before that fix, so a worse picture can be bisected without a rebuild.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// The variable of each switch (see the accessors below).
pub(crate) const CFG_PRUNE: &str = "VICTORY_SHADER_CFG_PRUNE";
pub(crate) const OMOD_ZERO: &str = "VICTORY_SHADER_OMOD_ZERO";
pub(crate) const SBUFFER_TERM: &str = "VICTORY_SHADER_SBUFFER_TERM";

/// `VICTORY_SHADER_CFG_PRUNE=0`: blocks no path from entry reaches are kept and
/// their edges threaded (flamingo #679), so a phi can again receive `NONE` from
/// an unreachable predecessor.
pub(crate) fn cfg_prune() -> bool {
    static PRUNE_ON: OnceLock<bool> = OnceLock::new();
    on(CFG_PRUNE, &PRUNE_ON)
}

/// `VICTORY_SHADER_OMOD_ZERO=0`: an `omod` (`mul:2`/`mul:4`/`div:2`) leaves the
/// sign of a zero result as IEEE `FMul` keeps it, without the RDNA2
/// compare-against-+0 flush (flamingo #662).
pub(crate) fn omod_zero() -> bool {
    static ZERO_ON: OnceLock<bool> = OnceLock::new();
    on(OMOD_ZERO, &ZERO_ON)
}

/// `VICTORY_SHADER_SBUFFER_TERM=0`: an `s_buffer_load` adds its address terms
/// untruncated and aligns only the final sum, as before flamingo #659.
pub(crate) fn sbuffer_term() -> bool {
    static TERM_ON: OnceLock<bool> = OnceLock::new();
    on(SBUFFER_TERM, &TERM_ON)
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
/// test can see both sides of one path in one process. The value is per thread
/// (`#[test]`s run in parallel), and a thread that never forces one sees the
/// process default.
#[cfg(test)]
pub(crate) fn force(name: &'static str, value: Option<bool>) {
    FORCED.with(|f| {
        let mut f = f.borrow_mut();
        f.retain(|&(n, _)| n != name);
        if let Some(v) = value {
            f.push((name, v));
        }
    });
}

/// The forced value of `name` on this thread, if any (see [`force`]).
#[cfg(test)]
fn forced(name: &'static str) -> Option<bool> {
    FORCED.with(|f| f.borrow().iter().find(|&&(n, _)| n == name).map(|&(_, v)| v))
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::RefCell<Vec<(&'static str, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::{
        CFG_PRUNE, OMOD_ZERO, SBUFFER_TERM, cfg_prune, force, omod_zero, sbuffer_term, set_from,
    };
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
            (CFG_PRUNE, cfg_prune as fn() -> bool),
            (OMOD_ZERO, omod_zero as fn() -> bool),
            (SBUFFER_TERM, sbuffer_term as fn() -> bool),
        ] {
            force(name, Some(false));
            assert!(!accessor(), "{name}=0 must turn the switch off");
            force(name, Some(true));
            assert!(accessor(), "{name} on must leave the switch on");
            force(name, None);
        }
        // Cleared again: the process default, on.
        assert!(cfg_prune() && omod_zero() && sbuffer_term());
    }
}
