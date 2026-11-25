//! The shader caches the driver and the translation layer keep on disk, and
//! their sizes: a start that builds pipelines reads them, a run adds to them
//! (`victory-gpu`'s `pipeline::warm` reports both after a run).

use std::path::{Path, PathBuf};

/// Where RADV and vkd3d-proton keep their caches, by name: their own
/// variables first, then the folders they use.
pub fn dirs() -> Vec<(&'static str, PathBuf)> {
    let cache = cache_root();
    let radv = std::env::var_os("MESA_SHADER_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| cache.join("mesa_shader_cache"));
    // vkd3d-proton's: its variable first, then the folder it writes to
    // (Windows builds use LOCALAPPDATA, Wine's a cache folder).
    let local = cache.join("vkd3d-proton");
    let windows = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("vkd3d-proton"));
    let vkd3d = std::env::var_os("VKD3D_SHADER_CACHE_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::iter::once(local.as_path())
                .chain(windows.as_deref())
                .find(|d| d.is_dir())
                .map(Path::to_owned)
        })
        .unwrap_or(local);
    vec![("RADV (mesa_shader_cache)", radv), ("vkd3d-proton", vkd3d)]
}

/// `$XDG_CACHE_HOME`, else `~/.cache` (empty if there is neither).
fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => home.as_ref().map_or_else(PathBuf::new, |h| Path::new(h).join(".cache")),
    }
}

/// Files and bytes under `dir`, two levels deep: the caches keep their
/// entries in one folder, Windows' vkd3d-proton in one more.
pub fn size(dir: &Path) -> Option<(u64, u64)> {
    fn walk(dir: &Path, depth: u8, files: &mut u64, bytes: &mut u64) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && depth > 0 {
                walk(&entry.path(), depth - 1, files, bytes)?;
            } else if kind.is_file() {
                *files += 1;
                *bytes += entry.metadata().map_or(0, |m| m.len());
            }
        }
        Ok(())
    }
    if !dir.is_dir() {
        return None;
    }
    let (mut files, mut bytes) = (0, 0);
    walk(dir, 1, &mut files, &mut bytes).ok()?;
    Some((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("victory-caches-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_size_counts_the_files_one_level_down() {
        let dir = temp("size");
        std::fs::write(dir.join("a"), b"12345").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("b"), b"123").unwrap();
        assert_eq!(size(&dir), Some((2, 8)));
        assert_eq!(size(&dir.join("gone")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dirs_name_radv_and_vkd3d_under_the_cache_root() {
        // The environment is this process's: only the shape is checked.
        let dirs = dirs();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].0, "RADV (mesa_shader_cache)");
        assert_eq!(dirs[1].0, "vkd3d-proton");
        for (_, dir) in &dirs {
            assert!(dir.as_os_str().len() > "mesa_shader_cache".len());
        }
    }
}
