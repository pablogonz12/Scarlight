//! Persistent per-shader resource metadata cache (`shader-meta.bin`).
//!
//! Stores what one parse of a console shader container tells us about it —
//! the [`UsedResource`] slice its code reads and whether its programs can
//! store ([`crate::pipeline`]'s `stores`) — keyed by the 32-byte BLAKE3 of
//! the container, so a warm launch skips `Container::parse` and the zlib
//! `XBCH` decompression (`native_programs`) on cached shader hits.
//!
//! The file is only served to the build that wrote it: its header carries
//! [`build`](crate::shader_cache::build), the BLAKE3 of our executable and
//! of the settings the parse and translation run with, so a rebuilt
//! translator (or one whose switches differ) starts from an empty store
//! instead of reading metadata a different build computed. A file of
//! another format version, another build, or a truncated one is ignored,
//! as is a bad entry (the parse runs again).
//!
//! `VICTORY_SHADER_META=0`: the cache is off — nothing is loaded, nothing is
//! kept and nothing is written, so every container is parsed again.
//!
//! Layout, all little endian: `VSM1`, `u32` version, 32-byte build
//! fingerprint, `u32` entry count, then per entry a 32-byte container
//! hash, a `u8` of flags (`1` = stores), a `u32` resource count and that
//! many resources.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use victory_shader::{ResourceType, RootType, UsedResource};

const FILE_NAME: &str = "shader-meta.bin";
const MAGIC: &[u8; 4] = b"VSM1";
const VERSION: u32 = 1;
/// The header's build fingerprint sits here.
const FINGERPRINT_AT: usize = 8;
const HEADER_BYTES: usize = FINGERPRINT_AT + 32 + 4;

/// What one parse of a container gives: the resources its code reads and
/// whether its programs can store to memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) used: Vec<UsedResource>,
    pub(crate) stores: bool,
}

type MetaMap = HashMap<[u8; 32], Meta>;

struct MetaStore {
    map: MetaMap,
    dirty: bool,
}

impl MetaStore {
    fn empty() -> Self {
        Self {
            map: HashMap::new(),
            dirty: false,
        }
    }

    fn load(dir: &Path, fingerprint: &[u8; 32]) -> Self {
        Self {
            map: load_from(&dir.join(FILE_NAME), fingerprint).unwrap_or_default(),
            dirty: false,
        }
    }
}

struct Store {
    fingerprint: [u8; 32],
    meta: RwLock<MetaStore>,
    save: Mutex<()>,
}

impl Store {
    fn empty(fingerprint: [u8; 32]) -> Self {
        Self {
            fingerprint,
            meta: RwLock::new(MetaStore::empty()),
            save: Mutex::new(()),
        }
    }

    fn load(dir: &Path, fingerprint: [u8; 32]) -> Self {
        Self {
            fingerprint,
            meta: RwLock::new(MetaStore::load(dir, &fingerprint)),
            save: Mutex::new(()),
        }
    }
}

static STORE: OnceLock<Store> = OnceLock::new();
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Whether the cache is used at all; `VICTORY_SHADER_META=0` turns it off.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHADER_META").is_none_or(|v| v != "0"))
}

/// What identifies this build, and so which `shader-meta.bin` is ours.
fn fingerprint() -> [u8; 32] {
    *crate::shader_cache::build()
}

fn global_store() -> &'static Store {
    STORE.get_or_init(|| Store::empty(fingerprint()))
}

/// Loads `dir/shader-meta.bin` into the global metadata store.
///
/// Missing, corrupt or other-build files silently start an empty store.
pub(crate) fn init(dir: &Path) {
    let loaded = match enabled() {
        true => Store::load(dir, fingerprint()),
        false => Store::empty(fingerprint()),
    };
    if let Err(store) = STORE.set(loaded) {
        let mut guard = global_store().meta.write().unwrap_or_else(|e| e.into_inner());
        *guard = store.meta.into_inner().unwrap_or_else(|e| e.into_inner());
    }
}

/// Returns cached metadata for `hash`, or computes and stores it on a miss.
pub(crate) fn get_or_compute<F>(hash: &[u8; 32], compute: F) -> Result<Meta, String>
where
    F: FnOnce() -> Result<Meta, String>,
{
    get_or_compute_in(global_store(), hash, compute)
}

/// Atomically flushes new metadata entries to `dir/shader-meta.bin` if dirty.
pub(crate) fn save(dir: &Path) {
    save_in(global_store(), dir);
}

fn get_or_compute_in<F>(store: &Store, hash: &[u8; 32], compute: F) -> Result<Meta, String>
where
    F: FnOnce() -> Result<Meta, String>,
{
    if enabled() {
        let guard = store.meta.read().unwrap_or_else(|e| e.into_inner());
        if let Some(meta) = guard.map.get(hash) {
            return Ok(meta.clone());
        }
    }

    let meta = compute()?;
    if !enabled() {
        return Ok(meta);
    }

    {
        let mut guard = store.meta.write().unwrap_or_else(|e| e.into_inner());
        let mut inserted = false;
        guard.map.entry(*hash).or_insert_with(|| {
            inserted = true;
            meta.clone()
        });
        if inserted {
            guard.dirty = true;
        }
    }

    Ok(meta)
}

fn save_in(store: &Store, dir: &Path) {
    save_in_with_hook(store, dir, || {});
}

fn save_in_with_hook(store: &Store, dir: &Path, before_rename: impl FnOnce()) {
    if !enabled() {
        return;
    }
    let _save_guard = store.save.lock().unwrap_or_else(|e| e.into_inner());

    {
        let guard = store.meta.read().unwrap_or_else(|e| e.into_inner());
        if !guard.dirty {
            return;
        }
    }

    let bytes = {
        let mut guard = store.meta.write().unwrap_or_else(|e| e.into_inner());
        if !guard.dirty {
            return;
        }
        guard.dirty = false;
        encode(&guard.map, &store.fingerprint)
    };

    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{FILE_NAME}.tmp-{}-{n}", std::process::id()));
    let target = dir.join(FILE_NAME);
    if let Err(e) = write_atomic(&tmp, &target, &bytes, before_rename) {
        let _ = std::fs::remove_file(&tmp);
        let mut guard = store.meta.write().unwrap_or_else(|e| e.into_inner());
        guard.dirty = true;
        tracing::warn!(path = %target.display(), error = %e, "failed to save shader metadata cache");
    }
}

fn write_atomic(
    tmp: &Path,
    target: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(),
) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(tmp, bytes)?;
    before_rename();
    std::fs::rename(tmp, target)?;
    Ok(())
}

fn load_from(path: &Path, fingerprint: &[u8; 32]) -> Option<MetaMap> {
    let bytes = std::fs::read(path).ok()?;
    decode(&bytes, fingerprint)
}

fn encode_resource_type(out: &mut Vec<u8>, kind: ResourceType) {
    match kind {
        ResourceType::Srv => out.push(0),
        ResourceType::Sampler => out.push(1),
        ResourceType::Cbv => out.push(2),
        ResourceType::Uav => out.push(3),
        ResourceType::Other(x) => {
            out.push(4);
            out.push(x);
        }
    }
}

fn encode_root_type(out: &mut Vec<u8>, root: RootType) {
    match root {
        RootType::Constants => out.push(0),
        RootType::Descriptor => out.push(1),
        RootType::Table => out.push(2),
        RootType::ImmediateSampler => out.push(3),
        RootType::ResourceHeap => out.push(4),
        RootType::SamplerHeap => out.push(5),
        RootType::Other(x) => {
            out.push(6);
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
}

fn encode(map: &MetaMap, fingerprint: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + map.len() * 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(fingerprint);
    out.extend_from_slice(&(map.len() as u32).to_le_bytes());
    for (hash, meta) in map {
        out.extend_from_slice(hash);
        out.push(u8::from(meta.stores));
        out.extend_from_slice(&(meta.used.len() as u32).to_le_bytes());
        for res in &meta.used {
            encode_resource_type(&mut out, res.kind);
            encode_root_type(&mut out, res.root);
            out.extend_from_slice(&res.table_offset_dwords.to_le_bytes());
            out.extend_from_slice(&res.lower_bound.to_le_bytes());
            out.extend_from_slice(&res.stride.to_le_bytes());
            out.push(res.dwords);
            out.push(res.user_data);
            out.push(res.slot);
        }
    }
    out
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn hash(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }

    fn resource_type(&mut self) -> Option<ResourceType> {
        match self.u8()? {
            0 => Some(ResourceType::Srv),
            1 => Some(ResourceType::Sampler),
            2 => Some(ResourceType::Cbv),
            3 => Some(ResourceType::Uav),
            4 => Some(ResourceType::Other(self.u8()?)),
            _ => None,
        }
    }

    fn root_type(&mut self) -> Option<RootType> {
        match self.u8()? {
            0 => Some(RootType::Constants),
            1 => Some(RootType::Descriptor),
            2 => Some(RootType::Table),
            3 => Some(RootType::ImmediateSampler),
            4 => Some(RootType::ResourceHeap),
            5 => Some(RootType::SamplerHeap),
            6 => Some(RootType::Other(self.u32()?)),
            _ => None,
        }
    }
}

/// The map a file holds, or `None` when it is not ours to serve: another
/// magic or version, another build, a truncated or malformed body.
fn decode(bytes: &[u8], fingerprint: &[u8; 32]) -> Option<MetaMap> {
    const MIN_ENTRY_BYTES: usize = 32 + 1 + 4;
    const MIN_RESOURCE_BYTES: usize = 1 + 1 + 4 + 4 + 4 + 1 + 1 + 1;

    if bytes.len() < HEADER_BYTES {
        return None;
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    if r.u32()? != VERSION {
        return None;
    }
    if r.hash()? != *fingerprint {
        return None;
    }

    let count = r.u32()? as usize;
    if count.checked_mul(MIN_ENTRY_BYTES)? > r.0.len() {
        return None;
    }

    let mut map = HashMap::with_capacity(count.min(256));
    for _ in 0..count {
        let hash = r.hash()?;
        let flags = r.u8()?;
        if flags & !1 != 0 {
            return None;
        }
        let used_len = r.u32()? as usize;
        if used_len.checked_mul(MIN_RESOURCE_BYTES)? > r.0.len() {
            return None;
        }
        let mut used = Vec::with_capacity(used_len.min(256));
        for _ in 0..used_len {
            used.push(UsedResource {
                kind: r.resource_type()?,
                root: r.root_type()?,
                table_offset_dwords: r.u32()?,
                lower_bound: r.u32()?,
                stride: r.u32()?,
                dwords: r.u8()?,
                user_data: r.u8()?,
                slot: r.u8()?,
            });
        }
        map.insert(
            hash,
            Meta {
                used,
                stores: flags & 1 != 0,
            },
        );
    }

    r.0.is_empty().then_some(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use tempfile::TempDir;

    const FP: [u8; 32] = [0x5Au8; 32];

    fn sample_resources() -> Vec<UsedResource> {
        let kinds = [
            ResourceType::Srv,
            ResourceType::Sampler,
            ResourceType::Cbv,
            ResourceType::Uav,
            ResourceType::Other(0),
            ResourceType::Other(255),
        ];
        let roots = [
            RootType::Constants,
            RootType::Descriptor,
            RootType::Table,
            RootType::ImmediateSampler,
            RootType::ResourceHeap,
            RootType::SamplerHeap,
            RootType::Other(0),
            RootType::Other(u32::MAX),
        ];
        kinds
            .iter()
            .cycle()
            .zip(roots.iter().cycle())
            .take(8)
            .enumerate()
            .map(|(i, (&kind, &root))| UsedResource {
                kind,
                root,
                table_offset_dwords: i as u32 * 4,
                lower_bound: 10 + i as u32,
                stride: 32 + i as u32,
                dwords: (4 + i) as u8,
                user_data: i as u8,
                slot: (i + 1) as u8,
            })
            .collect()
    }

    fn sample_meta() -> Meta {
        Meta {
            used: sample_resources(),
            stores: true,
        }
    }

    #[test]
    fn encode_decode_roundtrip_and_corruption() {
        let mut map = HashMap::new();
        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];
        map.insert(h1, sample_meta());
        map.insert(
            h2,
            Meta {
                used: Vec::new(),
                stores: false,
            },
        );

        let bytes = encode(&map, &FP);
        let decoded = decode(&bytes, &FP).expect("valid encoded map must decode");
        assert_eq!(decoded, map);

        let mut bad_magic = bytes.clone();
        bad_magic[3] = b'0';
        assert_eq!(decode(&bad_magic, &FP), None);

        let mut bad_ver = bytes.clone();
        bad_ver[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(decode(&bad_ver, &FP), None);

        // Another build's file is not ours: a rebuilt translator (or one
        // whose switches differ) must not read metadata it did not compute.
        assert_eq!(decode(&bytes, &[0xA5u8; 32]), None);
        let mut other_build = bytes.clone();
        other_build[FINGERPRINT_AT] ^= 1;
        assert_eq!(decode(&other_build, &FP), None);

        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut], &FP), None, "truncated at {cut}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing, &FP), None);
    }

    #[test]
    fn store_persists_to_disk_and_skips_recompute() {
        let dir = TempDir::new().unwrap();
        let store = Store::load(dir.path(), FP);
        assert!(!store.meta.read().unwrap().dirty);

        let hash = [0xABu8; 32];
        let expected = sample_meta();
        let runs = AtomicU64::new(0);

        let got = get_or_compute_in(&store, &hash, || {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(expected.clone())
        })
        .unwrap();
        assert_eq!(got, expected);
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert!(store.meta.read().unwrap().dirty);

        let got2 = get_or_compute_in(&store, &hash, || {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(sample_meta())
        })
        .unwrap();
        assert_eq!(got2, expected);
        assert_eq!(runs.load(Ordering::Relaxed), 1);

        let err_hash = [0xCDu8; 32];
        assert!(get_or_compute_in(&store, &err_hash, || Err("boom".to_owned())).is_err());

        save_in(&store, dir.path());
        assert!(dir.path().join(FILE_NAME).is_file());
        assert!(!store.meta.read().unwrap().dirty);

        save_in(&store, dir.path());
        assert!(!store.meta.read().unwrap().dirty);

        let reloaded = Store::load(dir.path(), FP);
        let got3 = get_or_compute_in(&reloaded, &hash, || {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(sample_meta())
        })
        .unwrap();
        assert_eq!(got3, expected);
        assert_eq!(runs.load(Ordering::Relaxed), 1);

        // A store of another build reads none of it, and overwrites it on
        // its own next save.
        let other = Store::load(dir.path(), [0xA5u8; 32]);
        assert!(other.meta.read().unwrap().map.is_empty());
    }

    #[test]
    fn concurrent_saves_publish_in_order_and_never_overwrite_newer_entries() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(Store::empty(FP));

        let hash_a = [0xAA; 32];
        let val_a = sample_meta();
        get_or_compute_in(&store, &hash_a, || Ok(val_a.clone())).unwrap();

        let (at_rename_tx, at_rename_rx) = mpsc::channel();
        let (allow_rename_tx, allow_rename_rx) = mpsc::channel();

        let store_worker = Arc::clone(&store);
        let worker_dir = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            save_in_with_hook(&store_worker, &worker_dir, || {
                at_rename_tx.send(()).unwrap();
                allow_rename_rx.recv().unwrap();
            });
        });

        at_rename_rx.recv().unwrap();

        let hash_b = [0xBB; 32];
        let val_b = Meta {
            used: sample_resources().into_iter().take(2).collect(),
            stores: false,
        };
        get_or_compute_in(&store, &hash_b, || Ok(val_b.clone())).unwrap();

        let (shutdown_done_tx, shutdown_done_rx) = mpsc::channel();
        let store_shutdown = Arc::clone(&store);
        let shutdown_dir = dir.path().to_path_buf();
        let shutdown = std::thread::spawn(move || {
            save_in(&store_shutdown, &shutdown_dir);
            shutdown_done_tx.send(()).unwrap();
        });

        assert!(shutdown_done_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());

        allow_rename_tx.send(()).unwrap();
        worker.join().unwrap();
        shutdown.join().unwrap();
        assert!(shutdown_done_rx.recv().is_ok());

        let disk = load_from(&dir.path().join(FILE_NAME), &FP).unwrap();
        assert_eq!(disk.get(&hash_a), Some(&val_a));
        assert_eq!(disk.get(&hash_b), Some(&val_b));
    }
}
