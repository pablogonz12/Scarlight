# Credits

Scarlight is a port of **VIctory** ([Marvisak/VIctory](https://github.com/Marvisak/VIctory)),
GPL-3.0. Its history, authorship and commit dates are kept.

The lists below are every author with a commit in this tree on `main`, plus the contributors named
in `Co-authored-by:` trailers, read from `git log main`.

## Project

- **VIctory** — Marvis and the VIctory contributors, GPL-3.0.

## Authors

9hvn9yt2vk-netizen, A6thGraderFr, Abdellah K., acleofe, adamfx, Advait Palve, AlanQiang0418,
alessiodeia, Andrea Casale, artor410, BaggyALPHA, bartikele, BastienRdz, Brazlucas, CaravelaLabs,
Carlos Sant'Anna, dadou000, Dathinel, desp, Duartt - Ryu Garage, Duff, EatCodeSleep2,
Enzo Souza Duarte, ESAT, Exorion, fever308, FIocker, franticfrostin, fuzeofficial, Gabriel Cruz,
gingertonik, Halfect, Henrique Bueno, hophaver, ifBars, ismailchaka1, itsRevela, Jeremy Goldberg,
JMAN730, julio, KaiserBloo, KoLolX3, lameloball2813-ui, LaunchDoesntCode, LF2000 Builder,
localshotgun, lordon1a, Louis Faust, LUIS LARIOS, Luiz Neves, Lyrnc, Manuel Pérez, Maraco91,
marquezini, Marvis, Matheus, meower, nathanbzrr, Nikita Romanovich, Noname0202, Nostyxx, Ortomanic,
pablogonz12, Pablo González, Pedro Fortunato, Peter Bonanni, QB, quaternion, r4., raffclar,
ruvenesh, Saitken91, Sakura-Byte, SkippySteve, swarm, tedin7, TG0000, undeadhuman37, uwutaube, Vanthys,
wrothgaril, Yaekai.

## Co-authors

andrewpozdnakov7-jpg, Bebrik229 (the `shader-meta.bin` shader metadata cache, VIctory #2614), BlueKaiman, kais7654, pushkarreddyy, shh.

## Design notes

- **Marvisak/VIctory [#2478](https://github.com/Marvisak/VIctory/pull/2478)** (HoLa-Lee), GPL-3.0: the idea
  of reading a texture once every N frames at most, round-robin by the texture's address, its measurements
  (a frame of world entry at 6,814 ms of texture work, 145-522 ms at 8), and its name for the switch,
  `VICTORY_TEXTURE_REFRESH_EVERY`. Reimplemented against this tree's refresh path
  (`crates/victory-gpu/src/texture.rs`), not copied: only textures already uploaded are skipped, and the
  switch is live (`crates/victory-gpu/src/switches.rs`).
- **xcl** ([ivall/xcl](https://github.com/ivall/xcl) and its fork Bathyal), GPL-2.0-only: the
  wave-pairing approach and the NVIDIA wave lane-assignment report used in
  [docs/wave64-on-wave32.md](docs/wave64-on-wave32.md). No code taken.
- **JMAN730/victory** ([project-flamingo#672](https://github.com/JMAN730/project-flamingo/pull/672)),
  GPL-3.0: narrowing a host shader resource view of a streamed texture to the mips whose memory is
  there, and remaking it when more levels arrive or a host copy fills the chain
  (`crates/victory-gpu/src/texture.rs`, `crates/victory-gpu/src/descriptor/host.rs`).
- **Marvisak/VIctory [#2736](https://github.com/Marvisak/VIctory/pull/2736)** (brainbudt), GPL-3.0: copying
  tiled textures as runs of whole micro-tile columns (one call per row and depth slice, 4-32 bytes at a
  time) instead of one closure call per span, its `detile_throughput` benchmark, and its idea of keeping
  the span walk as the reference (`crates/victory-formats/src/texture.rs`, `VICTORY_TEXTURE_RUNS=0`).
  Reimplemented on this tree's pattern tables: the depth-pitch overlap check its fast path dropped is
  kept, and a column-by-column fallback replaces its `unreachable!` for any pattern whose run width is
  not 4-32 bytes.
- **Marvisak/VIctory [#2735](https://github.com/Marvisak/VIctory/pull/2735)** (brainbudt), GPL-3.0: resolving a
  BCPack slot through a 256-slot window table (one bit per transition, ranked inside its 64-bit word) instead
  of a bucket search and an exact-division reciprocal, its `bcpack_decode_throughput` benchmark, and keeping
  the range scan as the reference (`crates/victory-formats/src/bcpack.rs`, `VICTORY_BCPACK_WINDOWS=0`). Ported
  whole, including its BC7 endpoint and index packing in `bc7_pack`.

- **fxceless/victory-fork [PR #905](https://github.com/fxceless/victory-fork/pull/905)** (staronthehorizon,
  from [PR #814](https://github.com/fxceless/victory-fork/pull/814)), GPL-3.0: the node test's rules that a
  child that is not there is a miss whatever its box holds, and that an empty or inverted box and any NaN
  coordinate miss in both node widths, with the reference's own tests
  (`crates/victory-shader/src/bvh.rs`, `crates/victory-shader/src/emit/intersect.rs`).
- **Marvisak/VIctory [#2706](https://github.com/Marvisak/VIctory/pull/2706)** (pes7), GPL-3.0: keeping a
  pipeline library per build in `pipelines/<hash>`, so a rebuild never writes its few new pipelines over a
  library the build before it warmed, with the newest two kept and only directories that hold a library
  pruned. Reimplemented against this tree's library, which has no `victory_shader::SOURCE_HASH`: the key is
  the executable's BLAKE3, the same bytes the shader cache keys its translations on
  (`crates/victory-gpu/src/shader_cache.rs`), and `VICTORY_LIBRARY_PRUNE=0` keeps every library.

- **fxceless/victory-fork [PR #1117](https://github.com/fxceless/victory-fork/pull/1117)** (Azemed),
  GPL-3.0: the title's views behind an `RwLock` (`views_read`/`views_write`, `crates/victory-gpu/src/inplace.rs`)
  and the lock-free hit path of `HostHeap::mirror` (`VICTORY_MIRROR_FAST=0` off,
  `crates/victory-gpu/src/descriptor/host.rs`, with `widen_due` kept on the hit path). Its section-heap windows
  did not come over: measured under Wine + vkd3d-proton + RADV, a committed section opens and the import under it
  is refused, silently substituting the driver's own memory (`xcl-bin/notes/zero-copy.md` 6).
- **Marvisak/VIctory [#2721](https://github.com/Marvisak/VIctory/pull/2721)** (0xkn0wh3r3), GPL-3.0: the idea
  that a frame past a budget says what bound it and why, split into our work around submissions, the GPU,
  the copy-back and the rest, the largest named (`crates/victory-gpu/src/frame_budget.rs`,
  `VICTORY_FRAME_BUDGET=0`). Reimplemented on this tree's counters and its own 50 ms budget: its
  per-command timestamps, memory-trait breakdown and slow-pipeline guard are not taken.
- **JMAN730/project-flamingo [#747](https://github.com/JMAN730/project-flamingo/pull/747)** (Rareuncommon),
  GPL-3.0: lower `v_mul_hi_u32_u24`/`v_mul_hi_i32_i24` as the high dword of the 48-bit product of the low 24
  bits of each operand (`crates/victory-shader/src/emit/lower/valu_int.rs`). Reimplemented on this tree's
  `mul_hi`, which moved to `lower/valu_int.rs` after that fork; its tests came across in spirit.

- **Chezzuss/VIctory batch 7 (unpublished), GPL-3.0**: clearing the ray-traced reflection image while the
  ray-tracing structures are unbuilt, as `VICTORY_REFLECTION=miss`
  (`crates/victory-gpu/src/reflection.rs`). Its `alpha`, `water` and `history` values and the sky-probe fallback
  tracking that tells the passes apart are not taken: this tree clears one image, the opaque pass's.
- **fxceless/victory-fork [PR #979](https://github.com/fxceless/victory-fork/pull/979)** (gmanka), GPL-3.0: the
  guarded views' clean blocks kept as contiguous runs (`CleanBlocks`, `crates/victory-gpu/src/watch.rs`), so a
  refresh of a mostly clean view heap walks only its dirty gaps instead of collecting one entry per 64 KiB
  block under the guard mutex; the per-block set it replaces is the test's reference.

- **AnyPS5** ([boykopovar/AnyPS5](https://github.com/boykopovar/AnyPS5)): the format of the
  source-counted progress page (reference instruction forms, history chart). No code taken.
- **Mesa** ([aco_opcodes.py](https://gitlab.freedesktop.org/mesa/mesa)), MIT: the reference list of
  RDNA2 instruction forms in `tools/progress/rdna2_isa_forms.txt` is generated from it.

- **Marvisak/VIctory [#2163](https://github.com/Marvisak/VIctory/pull/2163)** (0xkn0wh3r3), GPL-3.0: the
  built-in BCPack table's first BC7 dictionary entry has its 32 indices right after its width bytes, where
  every other entry repeats them first and holds 31 (`crates/victory-formats/src/bcpack.rs`).

- **fxceless/victory-fork** (Geckomacabre, Codex), GPL-3.0: textures a list's shaders write that the title's own
  code then reads on the CPU are copied into a read-back buffer when the list closes and tiled into the texture's
  title memory in the console's layout, by kind (`VICTORY_TEXTURE_BACK`, off by default), with each read-back's
  allocation and fence tracked so a remapped or released allocation is not landed
  (`crates/victory-gpu/src/texback.rs`).
- **fxceless/victory-fork [PR #1024](https://github.com/fxceless/victory-fork/pull/1024),
  [#1030](https://github.com/fxceless/victory-fork/pull/1030),
  [#1036](https://github.com/fxceless/victory-fork/pull/1036),
  [#1040](https://github.com/fxceless/victory-fork/pull/1040),
  [#1042](https://github.com/fxceless/victory-fork/pull/1042)** (Bartosz), GPL-3.0: four lowerings and a
  hardware rule from the console line, ported: a cube texture's level query in a pixel program
  (`image_get_lod`), an inactive compute lane's swizzle read as zero (`VICTORY_SHADER_SWIZZLE_ZERO=0` turns it
  off), 16-bit compares that ignore `op_sel` as RDNA2 does, `v_div_fixup_f16`, and saturating `v.clamp` on the
  packed 16-bit integer add/sub/mad (`crates/victory-shader/src/emit/`).

## Tools

Some commits were made with AI coding tools (Claude, Codex, Devin, Gemini, GPT). A contributor is
responsible for the code they submit, whoever or whatever helped write it.

Missing or wrong name? Open an issue.

- **miatson/VIctory [28b7278f](https://github.com/miatson/VIctory/commit/28b7278f)** (miatson), GPL-3.0: the
  finding that the console blends (MIN, MAX) into integer render targets while D3D12 refuses a pipeline that
  enables blending on one, so those targets are made to write unblended, per target and with independent
  blending turned on only where it was needed (`crates/victory-gpu/src/pipeline/host.rs`,
  `VICTORY_HOST_BLEND=0`). Its host-pipeline device test used `D3DCompile`; ours checks the state itself.
