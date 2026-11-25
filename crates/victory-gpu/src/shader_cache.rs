//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content and by the VIctory build
//! that translated it (the BLAKE3 of our executable), so a rebuilt
//! translator never serves an old translation. Entries go through
//! `victory-pipeline`'s content-addressed cache, which writes them
//! atomically; an entry that does not read back is translated again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too (entries of older builds
//! are ignored anyway).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// A name for this build, 16 hex digits of its BLAKE3: the same executable
/// the shader cache keys its translations on, shortened for a directory
/// name. A pipeline library is keyed on it, so a rebuilt VIctory never
/// reads pipelines another build made (its entries are named after
/// translated shaders, which a rebuild changes). `None` when our executable
/// cannot be read: nothing about this run may be trusted to a name, so
/// nothing is pruned either.
pub(crate) fn build_name() -> Option<String> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        // The executable's bytes alone: this is asked before the host GPU
        // exists (the pipeline library is opened first), so nothing here
        // may probe it as `build()` does.
        match std::env::current_exe().and_then(std::fs::read) {
            Ok(bytes) => Some(blake3::hash(&bytes).to_hex()[..16].to_owned()),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read our executable; no other run's pipeline library is read");
                None
            }
        }
    })
    .clone()
}

/// What identifies this build: the BLAKE3 of our executable file and of
/// the translation settings it runs with (every switch `victory-shader`'s
/// [`translate`] reads, its loop limits, its barrier mode, its clean-up
/// passes: `victory_shader::emit::settings_bytes`). One place lists them, so
/// a switch that picks the form of a program cannot be left out of the key
/// again: a run with `VICTORY_WAVE_LOCAL=0` or `VICTORY_F16_SUM_WIDE=0` was
/// served the translations an earlier run of the same executable had cached
/// with them unset, and the water it drew was flat.
///
/// The inputs the host GPU provides (the null slot view indices are clamped
/// to, the widest wave it runs) are *not* here: they are known only once the
/// device exists, which is after this is first asked for (the pipeline
/// library and `shader-meta.bin` are opened before it). [`entry_key`] adds
/// them when a translation is looked up.
pub(crate) fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        // The compute programs' own limit is part of their translation too.
        let compute = victory_shader::emit::compute_loop_limit();
        if compute != limit {
            tracing::warn!(
                compute,
                "VICTORY_SHADER_LOOP_LIMIT_CS: translated compute loops stop sooner"
            );
        }
        let settings = victory_shader::emit::settings_bytes();
        match std::env::current_exe().and_then(std::fs::read) {
            Ok(exe) => build_key(&exe, &settings),
            Err(e) => {
                // Without it no entry can be trusted: a fresh key per run.
                tracing::warn!(error = %e, "cannot read our executable; shader cache unused");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                *blake3::hash(&now.as_nanos().to_le_bytes()).as_bytes()
            }
        }
    })
}

/// The key of one translation: the build (executable and switches) and the
/// inputs the host GPU supplies ([`victory_shader::emit::device_bytes`]).
/// Read at translation time, never cached: the device's limits are probed
/// after the caches are opened, and a translation made on one host must
/// never be served to a run of another (its clamp and its wave size are in
/// the DXIL).
fn entry_key() -> [u8; 32] {
    let mut bytes = build().to_vec();
    bytes.extend_from_slice(&victory_shader::emit::device_bytes());
    *blake3::hash(&bytes).as_bytes()
}

/// The key for `exe`'s translation with `settings` (see [`build`]): pure, so
/// a test can ask what a switch set would key without touching the process's
/// executable or environment.
fn build_key(exe: &[u8], settings: &[u8]) -> [u8; 32] {
    let mut bytes = exe.to_vec();
    bytes.extend_from_slice(settings);
    *blake3::hash(&bytes).as_bytes()
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, String> {
    let translate = |c: &[u8]| victory_shader::emit::translate(c).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => cached(cache, &entry_key(), container, translate),
        None => translate(container),
    }
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let input = [&build[..], container].concat();
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

const MAGIC: &[u8; 4] = b"VSH1";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        root_indexed_views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH1 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    /// What this fix was for. The key before it named four switches
    /// (`VICTORY_WAVE_WIDE`, `VICTORY_WAVE_DEEP_SKIPS`, `VICTORY_WAVE_OWN_BITS`,
    /// `VICTORY_F16_NEAREST`) and nothing else, so a run with any other one
    /// changed was served the translation the earlier run had cached: 106 of
    /// the 751 dumped programs translate to different DXIL with
    /// `VICTORY_FIRST_HALF=0`, `VICTORY_F16_SUM_WIDE=0` and
    /// `VICTORY_STRICT_JOINS=1` change or refuse one each. A key that ignores
    /// the switch is shown here serving that stale entry; the key built from
    /// `settings_bytes` misses it.
    #[test]
    fn a_switch_outside_the_old_key_served_a_stale_translation() {
        use std::ffi::OsString;

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let container = b"a console container";
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        // The old key's settings: the four switches it named, all unset, so
        // the same bytes whatever any other switch says.
        let old_settings = victory_shader::emit::settings_bytes_of(|name| {
            let four = [
                "VICTORY_WAVE_WIDE",
                "VICTORY_WAVE_DEEP_SKIPS",
                "VICTORY_WAVE_OWN_BITS",
                "VICTORY_F16_NEAREST",
            ];
            four.contains(&name).then(|| OsString::from("0"))
        });
        let old = build_key(b"our executable", &old_settings);
        assert_eq!(cached(&cache, &old, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1);

        let half = victory_shader::emit::settings_bytes_of(|name| match name {
            "VICTORY_FIRST_HALF" => Some(OsString::from("0")),
            _ => None,
        });
        assert_ne!(
            victory_shader::emit::settings_bytes_of(|_| None),
            half,
            "VICTORY_FIRST_HALF=0 does not change the settings"
        );
        // The old key cannot tell the two runs apart: the stale entry is served.
        assert_eq!(cached(&cache, &old, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the old key served it");
        // The new one does: it translates again.
        let new = build_key(b"our executable", &half);
        assert_ne!(old, new);
        assert_eq!(cached(&cache, &new, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2, "the new key translated");
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    /// Toggling any switch the translation reads must translate again: the
    /// entry an earlier run cached with the switch unset is not served.
    #[test]
    fn a_switch_toggle_misses_the_cache() {
        use std::ffi::OsString;

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let container = b"a console container";
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        fn keyed(switches: impl Fn(&str) -> Option<OsString>) -> [u8; 32] {
            build_key(b"our executable", &victory_shader::emit::settings_bytes_of(switches))
        }
        let unset = keyed(|_| None);
        assert_eq!(cached(&cache, &unset, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        // The same switches again is the entry just written.
        assert_eq!(cached(&cache, &unset, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "a warm key is a hit");

        for name in victory_shader::emit::SWITCHES {
            for value in ["0", "1"] {
                let toggled = keyed(|asked| match asked == *name {
                    true => Some(OsString::from(value)),
                    false => None,
                });
                assert_ne!(unset, toggled, "{name}={value} keys like unset");
                let before = runs.load(Ordering::Relaxed);
                assert_eq!(cached(&cache, &toggled, container, translate), Ok(shader()));
                assert_eq!(
                    runs.load(Ordering::Relaxed),
                    before + 1,
                    "{name}={value} was served a translation cached without it"
                );
            }
        }
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
    }
}
