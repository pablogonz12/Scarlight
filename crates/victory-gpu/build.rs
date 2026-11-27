use std::path::{Path, PathBuf};

const EXTRA_SOURCES: &[&str] = &[
    "crates/victory-shader/Cargo.toml",
    "crates/victory-gpu/src/shader_cache.rs",
    "crates/victory-gpu/src/shader_meta.rs",
    "crates/victory-gpu/src/pipeline.rs",
    "crates/victory-gpu/src/pipeline/host.rs",
    "crates/victory-gpu/build.rs",
];

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in std::fs::read_dir(dir).expect("read shader source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    collect(&root.join("crates/victory-shader/src"), &mut files);
    for path in EXTRA_SOURCES {
        files.push(root.join(path));
    }
    files.sort();
    let mut hash = blake3::Hasher::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.strip_prefix(&root).expect("source inside workspace");
        let name = name.to_str().expect("source path is UTF-8").as_bytes();
        let data = std::fs::read(&path).expect("read source");
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update(&(data.len() as u64).to_le_bytes());
        hash.update(&data);
    }
    // zlib-rs is the translator's only runtime dependency that processes shader
    // data. dxil-spirv runs downstream and its output is not stored here.
    let lock = root.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let lock = std::fs::read_to_string(lock).expect("read lockfile");
    let zlib = lock
        .split("[[package]]")
        .find(|p| p.lines().any(|l| l == "name = \"zlib-rs\""))
        .expect("zlib-rs lock entry");
    hash.update(zlib.as_bytes());
    println!(
        "cargo:rustc-env=VICTORY_TRANSLATOR_KEY={}",
        hash.finalize().to_hex()
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_computation_is_fingerprinted() {
        assert!(super::EXTRA_SOURCES.contains(&"crates/victory-gpu/src/pipeline.rs"));
    }
}
