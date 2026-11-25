//! Per-title profiles: what one title's build needs on this layer, kept in
//! the user's config directory and selected by the title's own identity.
//!
//! Nothing here knows a title. The runtime hashes the image it is about to
//! run (BLAKE3), looks for a profile whose `build` list matches that hash,
//! and makes it the current one ([`set_current`]) before any title code
//! runs. The loader (code hooks) and the GPU layer (shaders to skip) then
//! ask [`current`] what they should do.
//!
//! Profiles are user data, not repository data: a profile may name hashes of
//! the title's files, per-build offsets or shader hashes, so it lives outside
//! the tree (see `docs/title-profiles.md` and `docs/asset-policy.md`). No
//! profile ships with VIctory; a title without one runs on the documented,
//! generic path.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Environment variable naming the directory profiles are read from.
pub const DIR_ENV: &str = "VICTORY_PROFILE_DIR";

/// One title build's profile.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// What to call this build when printing it (the user's wording).
    #[serde(default)]
    pub name: Option<String>,
    /// BLAKE3 of the image, hex; a prefix of 16 or more digits matches.
    #[serde(default)]
    pub build: Vec<String>,
    /// Code patches to place in the image once it is mapped.
    #[serde(default)]
    pub hooks: Vec<Hook>,
    /// Work the GPU layer should not run.
    #[serde(default)]
    pub skip: Skip,
    /// The build's internal render scale to ask for, `0.2..=1.0` of the
    /// output size (`VICTORY_RENDER_SCALE` overrides it): the value a hook
    /// with `new_scale` writes. Per build, since the places to write live in
    /// the hooks.
    #[serde(default)]
    pub render_scale: Option<f32>,
    /// Use the title's own upscaler on every console profile, rather than
    /// the one the profile would pick (`VICTORY_OWN_UPSCALER` overrides it).
    #[serde(default)]
    pub own_upscaler: Option<bool>,
    /// The file this profile was read from (not part of the file).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

/// One code patch: the bytes at `rva` must be exactly `expect` before `new`
/// is written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// Our own description of the patch, for the log.
    pub what: String,
    /// Offset from the start of the mapped image.
    pub rva: u64,
    /// The bytes that must be there first (hex, two digits each). Not read
    /// when `expect_from` is given.
    #[serde(default)]
    pub expect: String,
    /// The bytes to write instead (same length as `expect`). Not read when
    /// `new_scale` or `new_from` is given.
    #[serde(default)]
    pub new: String,
    /// Only place this hook when these switches are on: `render_scale`,
    /// `own_upscaler`, or their negation (`!own_upscaler`). Empty: always.
    #[serde(default)]
    pub when: Vec<String>,
    /// Write the effective render scale as an f32, little-endian, instead of
    /// `new` (`expect` still says how many bytes, and must be four).
    #[serde(default)]
    pub new_scale: bool,
    /// The bytes to expect are `base + this rva`, as an 8-byte little-endian
    /// address, rather than the bytes in `expect` (a pointer to one of the
    /// image's own functions, which only the load address can give).
    #[serde(default)]
    pub expect_from: Option<u64>,
    /// The bytes to write are `base + this rva`, as an 8-byte little-endian
    /// address, rather than the bytes in `new`.
    #[serde(default)]
    pub new_from: Option<u64>,
}

/// Draws and dispatches the GPU layer leaves out.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skip {
    /// 16-digit hashes of host shaders; a pipeline holding one is skipped.
    #[serde(default)]
    pub shaders: Vec<String>,
    /// Stages (`mesh`, `compute`, `vertex`, `pixel`) whose 64-lane programs
    /// the host cannot run safely.
    #[serde(default)]
    pub stages: Vec<String>,
}

/// The profile selected for this process, if any.
static CURRENT: OnceLock<Option<Profile>> = OnceLock::new();

/// Makes `profile` the profile the loader and the GPU layer use. The first
/// call wins; later ones are ignored (there is one title per process).
pub fn set_current(profile: Option<Profile>) {
    let _ = CURRENT.set(profile);
}

/// The profile selected for this process, if any.
pub fn current() -> Option<&'static Profile> {
    CURRENT.get().and_then(Option::as_ref)
}

impl Profile {
    /// The file this profile was read from, if it came from one.
    pub fn file(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The label to print for this build: `name`, else the hash it matched.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("profile")
    }

    /// Whether this profile is the one for an image with `blake3_hex`.
    pub fn matches(&self, blake3_hex: &str) -> bool {
        self.build.iter().any(|prefix| {
            let prefix = prefix.trim();
            prefix.len() >= 16
                && blake3_hex.len() >= prefix.len()
                && blake3_hex[..prefix.len()].eq_ignore_ascii_case(prefix)
        })
    }

    /// The render scale asked for: `VICTORY_RENDER_SCALE`, else the profile's
    /// own `render_scale`. `None` when neither asks for one (or the value is
    /// outside what the title's scale can be, which is logged).
    pub fn render_scale(&self) -> Option<f32> {
        self.render_scale_with(std::env::var(RENDER_SCALE_ENV).ok().as_deref())
    }

    /// [`Profile::render_scale`] for the environment value `asked` (`None`:
    /// the variable is not set), so the answer can be asked without one.
    pub fn render_scale_with(&self, asked: Option<&str>) -> Option<f32> {
        match asked {
            Some(text) => {
                let v = parse_scale(text);
                if v.is_none() {
                    tracing::warn!(text, "{RENDER_SCALE_ENV} wants a number in 0.2..=1.0");
                }
                v
            }
            None => self.render_scale.filter(|v| (0.2..=1.0).contains(v)),
        }
    }

    /// Whether the title's own upscaler is asked for everywhere:
    /// `VICTORY_OWN_UPSCALER` (`0` meaning no), else the profile's field.
    pub fn own_upscaler(&self) -> bool {
        self.own_upscaler_with(std::env::var(OWN_UPSCALER_ENV).ok().as_deref())
    }

    /// [`Profile::own_upscaler`] for the environment value `asked` (`None`:
    /// the variable is not set).
    pub fn own_upscaler_with(&self, asked: Option<&str>) -> bool {
        match asked {
            Some(v) => v != "0",
            None => self.own_upscaler.unwrap_or(false),
        }
    }
}

impl Hook {
    /// The bytes that must be at `rva` before this hook may be applied, for a
    /// build loaded at `base` (only a hook with `expect_from` needs it).
    pub fn expected_at(&self, base: u64) -> Result<Vec<u8>> {
        match self.expect_from {
            Some(rva) => Ok((base.wrapping_add(rva)).to_le_bytes().to_vec()),
            None => {
                if self.expect.is_empty() {
                    anyhow::bail!("hook {:?}: no expect bytes", self.what);
                }
                decode_hex(&self.expect).with_context(|| format!("hook {:?}: expect", self.what))
            }
        }
    }

    /// [`Hook::expected_at`] for a build loaded at address 0: the bytes as
    /// written, with any `base + rva` counted from 0.
    pub fn expected(&self) -> Result<Vec<u8>> {
        self.expected_at(0)
    }

    /// The bytes to write once [`Hook::expected`] matched.
    pub fn replacement(&self) -> Result<Vec<u8>> {
        self.replacement_at(0, None)
    }

    /// The bytes to write, for a build loaded at `base` and a render scale of
    /// `scale` (`None`: no scale is asked for, and a hook that needs one is
    /// refused).
    pub fn replacement_at(&self, base: u64, scale: Option<f32>) -> Result<Vec<u8>> {
        if self.new_scale {
            let scale =
                scale.with_context(|| format!("hook {:?}: no render scale to write", self.what))?;
            return Ok(scale.to_le_bytes().to_vec());
        }
        match self.new_from {
            Some(rva) => Ok((base.wrapping_add(rva)).to_le_bytes().to_vec()),
            None => {
                if self.new.is_empty() {
                    anyhow::bail!("hook {:?}: no new bytes", self.what);
                }
                decode_hex(&self.new).with_context(|| format!("hook {:?}: new", self.what))
            }
        }
    }

    /// Whether this hook's `when` switches are on, for a build whose profile
    /// asks for `scale` and `own`.
    pub fn wanted(&self, scale: Option<f32>, own: bool) -> bool {
        self.when.iter().all(|name| {
            let (negated, name) = match name.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, name.as_str()),
            };
            let on = match name {
                "render_scale" => scale.is_some(),
                "own_upscaler" => own,
                // A switch we do not know is one the user meant to matter:
                // never place the hook, whatever its negation would say.
                _ => return false,
            };
            on != negated
        })
    }
}

/// Environment variable that asks for the build's own render scale.
pub const RENDER_SCALE_ENV: &str = "VICTORY_RENDER_SCALE";
/// Environment variable that asks for the title's own upscaler everywhere.
pub const OWN_UPSCALER_ENV: &str = "VICTORY_OWN_UPSCALER";

/// The render scale in an environment value, if it is one we accept.
pub fn parse_scale(text: &str) -> Option<f32> {
    let v: f32 = text.trim().parse().ok()?;
    (0.2..=1.0).contains(&v).then_some(v)
}

/// Reads the profile in `text`, from the file at `path` (for the log).
///
/// Hooks are read one at a time: a hook carrying a field this version does
/// not know was written for a newer loader, and is left out on its own with
/// a line naming it, rather than the whole profile going with it. (A hook
/// `when` needs a loader that knows `when`; the rest of a profile there is
/// no reason to lose.)
fn parse(text: &str, path: &Path) -> Result<Profile> {
    let mut value: toml::Value = toml::from_str(text)?;
    // Taken out so the rest of the profile is read without them, and each
    // hook is then read on its own.
    let hooks = value
        .get_mut("hooks")
        .and_then(|hooks| hooks.as_array_mut())
        .map(std::mem::take)
        .unwrap_or_default();
    let mut profile: Profile = value.try_into()?;
    for (at, hook) in hooks.into_iter().enumerate() {
        let name = hook
            .get("what")
            .and_then(|what| what.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("hook {at}"));
        match hook.try_into() {
            Ok(hook) => profile.hooks.push(hook),
            // The name in the message, not only as a field: the usual log
            // format prints the message alone.
            Err(e) => tracing::warn!(
                path = %path.display(),
                "hook {name:?} not placed: {e}"
            ),
        }
    }
    Ok(profile)
}

/// Reads every `*.toml` in `dir` (a missing directory is an empty list).
/// A file that does not parse is reported and skipped: a profile is opt-in,
/// and one bad file must not keep the title from running.
pub fn load_dir(dir: &Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml") && p.is_file())
        .collect();
    paths.sort();
    let mut profiles = Vec::new();
    for path in paths {
        let loaded = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse(&text, &path));
        match loaded {
            Ok(mut profile) => {
                profile.source = Some(path);
                profiles.push(profile);
            }
            Err(e) => tracing::warn!(path = %path.display(), "title profile not read: {e:#}"),
        }
    }
    profiles
}

/// The profile in `dir` that belongs to an image with `blake3_hex`.
pub fn for_build(dir: &Path, blake3_hex: &str) -> Option<Profile> {
    load_dir(dir).into_iter().find(|p| p.matches(blake3_hex))
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'_')
        .collect();
    if !digits.len().is_multiple_of(2) {
        anyhow::bail!("an odd number of hex digits");
    }
    digits
        .chunks(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair)?;
            u8::from_str_radix(s, 16).map_err(anyhow::Error::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn profile(text: &str) -> Profile {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_profile_matches_by_build_prefix_and_ignores_case() {
        let p = profile(r#"build = ["0123456789ABCDEF"]"#);
        assert!(p.matches(IMAGE));
        assert!(p.matches(&IMAGE.to_uppercase()));
        assert!(!p.matches("fedcba9876543210fedcba9876543210"));
    }

    #[test]
    fn a_short_or_empty_build_entry_matches_nothing() {
        assert!(!profile(r#"build = ["0123"]"#).matches(IMAGE));
        assert!(!profile("").matches(IMAGE));
    }

    #[test]
    fn hooks_decode_their_bytes() {
        let p = profile(
            r#"
            [[hooks]]
            what = "one"
            rva = 0x10
            expect = "48 8b 05"
            new = "eb_18_90"
            "#,
        );
        let hook = &p.hooks[0];
        assert_eq!(hook.rva, 0x10);
        assert_eq!(hook.expected().unwrap(), [0x48, 0x8b, 0x05]);
        assert_eq!(hook.replacement().unwrap(), [0xeb, 0x18, 0x90]);
    }

    #[test]
    fn a_bad_hook_byte_is_an_error_not_a_panic() {
        let p = profile(
            r#"
            [[hooks]]
            what = "one"
            rva = 0
            expect = "zz"
            new = "00"
            "#,
        );
        assert!(p.hooks[0].expected().is_err());
        let odd = profile(
            r#"
            [[hooks]]
            what = "two"
            rva = 0
            expect = "123"
            new = "00"
            "#,
        );
        assert!(odd.hooks[0].expected().is_err());
    }

    #[test]
    fn a_hook_with_a_scale_writes_the_f32_asked_for() {
        let p = profile(
            r#"
            render_scale = 0.5
            own_upscaler = true
            [[hooks]]
            what = "minimum"
            rva = 0x10
            expect = "00000000"
            new_scale = true
            when = ["render_scale"]
            [[hooks]]
            what = "fsr"
            rva = 0x20
            expect = "00000000"
            new = "11111111"
            when = ["render_scale", "!own_upscaler"]
            "#,
        );
        let scale = p.render_scale_with(None).unwrap();
        assert_eq!(scale, 0.5);
        assert_eq!(p.render_scale_with(Some("0.75")), Some(0.75), "the environment wins");
        assert_eq!(p.render_scale_with(Some("2")), None, "and an invalid one is no scale");
        assert!(p.own_upscaler_with(None));
        assert!(!p.own_upscaler_with(Some("0")));
        assert_eq!(p.hooks[0].replacement_at(0, Some(scale)).unwrap(), 0.5f32.to_le_bytes());
        assert!(p.hooks[0].wanted(Some(scale), true));
        assert!(!p.hooks[1].wanted(Some(scale), true), "an own-upscaler build takes no FSR patch");
        assert!(p.hooks[1].wanted(Some(scale), false));
        assert!(!p.hooks[1].wanted(None, false), "no scale, no render-scale hooks");
        assert!(p.hooks[0].replacement_at(0, None).is_err());
    }

    #[test]
    fn a_hook_can_read_and_write_the_load_address() {
        let p = profile(
            r#"
            [[hooks]]
            what = "a vtable slot"
            rva = 0x30
            expect_from = 0x4000
            new_from = 0x5000
            "#,
        );
        let hook = &p.hooks[0];
        let base = 0x7ff0_0000_0000u64;
        assert_eq!(hook.expected_at(base).unwrap(), (base + 0x4000).to_le_bytes());
        assert_eq!(hook.replacement_at(base, None).unwrap(), (base + 0x5000).to_le_bytes());
        assert!(hook.wanted(None, false), "no switch asked for: always placed");
    }

    #[test]
    fn a_scale_outside_the_floor_is_refused() {
        assert_eq!(parse_scale("0.5"), Some(0.5));
        assert_eq!(parse_scale(" 1 "), Some(1.0));
        assert_eq!(parse_scale("0.1"), None);
        assert_eq!(parse_scale("1.5"), None);
        assert_eq!(parse_scale("half"), None);
        assert_eq!(profile("render_scale = 1.5").render_scale(), None);
    }

    #[test]
    fn a_switch_this_loader_does_not_know_keeps_the_hook_out() {
        let p = profile(
            r#"
            [[hooks]]
            what = "for a later loader"
            rva = 0x10
            expect = "00000000"
            new = "11111111"
            when = ["!future_switch"]
            "#,
        );
        assert!(
            !p.hooks[0].wanted(Some(0.5), true),
            "an unknown switch is never on, negated or not"
        );
        assert!(!p.hooks[0].wanted(None, false));
    }

    #[test]
    fn skip_lists_default_to_empty() {
        let p = profile("");
        assert!(p.skip.shaders.is_empty() && p.skip.stages.is_empty());
        let q = profile("[skip]\nshaders = [\"0123456789abcdef\"]\nstages = [\"mesh\"]\n");
        assert_eq!(q.skip.shaders.len(), 1);
        assert_eq!(q.skip.stages, ["mesh"]);
    }

    #[test]
    fn a_hook_with_a_field_this_version_does_not_know_is_skipped_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p.toml"),
            r#"name = "ours"
build = ["0123456789abcdef"]

[[hooks]]
what = "kept"
rva = 0x10
expect = "90"
new = "91"

[[hooks]]
what = "written for a later loader"
rva = 0x20
expect = "90"
new = "91"
shinier = true

[[hooks]]
what = "kept too"
rva = 0x30
expect = "90"
new = "91"
"#,
        )
        .unwrap();
        let found = for_build(dir.path(), IMAGE).unwrap();
        let names: Vec<&str> = found.hooks.iter().map(|hook| hook.what.as_str()).collect();
        assert_eq!(names, ["kept", "kept too"]);
    }

    #[test]
    fn a_directory_without_profiles_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_dir(&dir.path().join("missing")).is_empty());
        assert!(for_build(dir.path(), IMAGE).is_none());
    }

    #[test]
    fn loading_picks_the_profile_for_the_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            "name = \"other\"\nbuild = [\"ffffffffffffffff\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            "name = \"ours\"\nbuild = [\"0123456789abcdef\"]\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.toml"), "name = ").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let found = for_build(dir.path(), IMAGE).unwrap();
        assert_eq!(found.label(), "ours");
        assert_eq!(found.file(), Some(dir.path().join("b.toml").as_path()));
        assert!(found.skip.shaders.is_empty() && found.hooks.is_empty());
    }
}
