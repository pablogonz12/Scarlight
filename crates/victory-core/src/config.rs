//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! See `victory.example.toml` at the repository root.
//!
//! Per-title profiles (see [`crate::profile`]) are read from the first of
//! these that holds one for the image being run:
//! `VICTORY_PROFILE_DIR`, `%APPDATA%\victory\titles`,
//! `$XDG_CONFIG_HOME/victory/titles` (or `~/.config/victory/titles`), else
//! `%LOCALAPPDATA%\victory\titles` (the cache's parent, for a system with
//! none of those variables). A Windows build under Wine reaches the
//! last two through the `Z:` drive; the `Z:` form of a POSIX path is tried
//! before the path itself, so both spellings work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;
use crate::profile::Profile;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// Where per-title profiles are read from, in priority order (never the
    /// repository).
    pub profile_dirs: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let explicit = std::env::var_os("VICTORY_CONFIG").map(PathBuf::from);
        let path = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from("victory.toml"));
        let file = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some((path, text))
        } else if let Some(p) = explicit {
            anyhow::bail!(
                "VICTORY_CONFIG points at {}, which does not exist",
                p.display()
            );
        } else {
            None
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        Self::resolve(parsed.paths, source, |k| std::env::var_os(k))
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let test_titles = match env("VICTORY_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("VICTORY_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir("cache"), Ok)?;
        let data_dir = env_path("VICTORY_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir("data"), Ok)?;
        // Profiles are found through the user's shell environment or inside
        // the Wine prefix, not beside whichever victory.toml was passed.
        let profile_dirs = profile_dirs(&env);
        Ok(Config {
            game_dir: env_path("VICTORY_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            profile_dirs,
            source,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// The profile for an image whose BLAKE3 is `blake3_hex`: the first one
    /// found in [`Config::profile_dirs`], in priority order.
    pub fn title_profile(&self, blake3_hex: &str) -> Option<Profile> {
        self.profile_dirs
            .iter()
            .find_map(|dir| crate::profile::for_build(dir, blake3_hex))
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// The directories profiles are read from, in priority order: the
/// environment, the roaming AppData, then the user's config directory.
fn profile_dirs(env: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = env_path(crate::profile::DIR_ENV) {
        dirs.push(dir);
    }
    if let Some(appdata) = env_path("APPDATA") {
        dirs.push(appdata.join("victory").join("titles"));
    }
    let config_home =
        env_path("XDG_CONFIG_HOME").or_else(|| env_path("HOME").map(|home| home.join(".config")));
    if let Some(config_home) = config_home {
        dirs.extend(dir_aliases(&config_home.join("victory").join("titles")));
    }
    if dirs.is_empty() {
        // Profiles are optional: a system that names no home at all runs
        // without them rather than failing to start.
        if let Ok(dir) = default_dir("titles") {
            dirs.push(dir);
        }
    }
    dirs
}

/// `dir`, plus how a Windows build reaches it under Wine: the same absolute
/// POSIX path through the `Z:` drive, tried first.
fn dir_aliases(dir: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    if let Some(rest) = dir.to_string_lossy().strip_prefix('/') {
        return vec![PathBuf::from(format!("Z:\\{}", rest.replace('/', "\\"))), dir.to_owned()];
    }
    vec![dir.to_owned()]
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `%LOCALAPPDATA%/victory/<name>` (elsewhere the XDG or home cache).
fn default_dir(name: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(|| OsString::from("from-env"));
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn profile_dirs_are_env_then_appdata_then_xdg_config() {
        let env = |k: &str| match k {
            "VICTORY_PROFILE_DIR" => Some(OsString::from("from-env")),
            "APPDATA" => Some(OsString::from("appdata")),
            "XDG_CONFIG_HOME" => Some(OsString::from("xdg")),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert_eq!(cfg.profile_dirs[0], PathBuf::from("from-env"));
        let appdata = Path::new("appdata").join("victory").join("titles");
        let xdg = Path::new("xdg").join("victory").join("titles");
        let at = |dir: &Path| cfg.profile_dirs.iter().position(|d| d == dir);
        assert!(at(&appdata) > Some(0));
        assert!(at(&xdg) > at(&appdata));
    }

    #[test]
    fn profiles_are_optional_on_a_system_that_names_no_home() {
        let env = |_: &str| None;
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert!(cfg.title_profile("0123456789abcdef").is_none());
    }

    #[test]
    fn profile_dirs_fall_back_to_the_home_config_directory() {
        let env = |k: &str| (k == "HOME").then(|| OsString::from("home"));
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        let want = Path::new("home")
            .join(".config")
            .join("victory")
            .join("titles");
        assert!(cfg.profile_dirs.iter().any(|d| d == &want));
        assert!(cfg.profile_dirs.len() <= 2);
    }

    #[test]
    fn a_windows_build_also_tries_the_z_drive_form() {
        // Only a Windows build can use `Z:`; elsewhere the plain path is all
        // there is, and both spellings must not be searched twice.
        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| OsString::from("/home/x/.config"));
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        let z = cfg
            .profile_dirs
            .iter()
            .any(|d| d.to_string_lossy().starts_with("Z:"));
        assert_eq!(z, cfg!(windows));
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
