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
- **vitutu1** by **ZigZagBoi1** ([ZigZagBoi1/ScarlettPort](https://github.com/ZigZagBoi1/ScarlettPort),
  GPL-3.0-or-later): the resolution and upscaler levers, which here are profile hooks and the
  switches `VICTORY_RENDER_SCALE` / `VICTORY_OWN_UPSCALER` rather than code that names a title.

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
- **fxceless/victory-fork [#1068](https://github.com/fxceless/victory-fork/pull/1068)** (UwUCammie), GPL-3.0: the
  idea that the title buffers' table is read most of the time and so wants a read lock, and that one submission's
  buffer refreshes go into the list in one batch (`crates/victory-gpu/src/shadow.rs`, `VICTORY_REFRESH_BATCH=0` puts
  one step in per span again). Its `defer_refreshes` came over as written; our `lookup` re-checks under the write
  lock, so a copy is made once.
- **miatson/VIctory [#2801](https://github.com/miatson/VIctory/pull/2801)** (miatson), GPL-3.0: the idea that
  the console blends MIN/MAX into integer render targets, which the host refuses, so such a target is made to
  write unblended when the pipeline is built (`crates/victory-gpu/src/pipeline/host.rs`). Its twin pipeline
  states did not come over; the format list came across as it stands, now checked against the `DXGI_FORMAT`
  constants by a test.

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

- **DexNom/VIctory [4775e8aaf](https://github.com/DexNom/VIctory/commit/4775e8aaf0f45c316807d26202efb369f40ceea0)**,
  GPL-3.0: one lock serializing the tests that set the global frame clock or read it across a wait
  (`TEST_FRAME_LOCK`, `crates/victory-gpu/src/frame.rs`), so a test that retimes the clock cannot move
  another's ticks under it when `cargo test` runs the crate's tests in parallel.
- **AnyPS5** ([boykopovar/AnyPS5](https://github.com/boykopovar/AnyPS5)): the format of the
  source-counted progress page (reference instruction forms, history chart). No code taken.
- **HUMAN** ([HumanlyDev/HUMAN](https://github.com/HumanlyDev/HUMAN)) by HumanlyDev and bdycounter-sudo,
  GPL-3.0-or-later: the pass that draws a depth plane's twin into the plane runs inline, in the list that
  wrote the twin (and that list replays the title's state after it), so a depth test in the same list sees
  what the shaders wrote (`crates/victory-gpu/src/host.rs` `twin_use`/`pass_plane`, `draw.rs` `replay`);
  `VICTORY_PLANE_INLINE=0` waits for the list's close, as before.

- **Mesa** ([aco_opcodes.py](https://gitlab.freedesktop.org/mesa/mesa)), MIT: the reference list of
  RDNA2 instruction forms in `tools/progress/rdna2_isa_forms.txt` is generated from it.
- **Marvisak/VIctory [#2513](https://github.com/Marvisak/VIctory/pull/2513)** (Calvix2k), GPL-3.0: the idea
  that a copy-back brings a declared range's snapshot in step with the bytes it takes from title memory, so that a
  later overlapping range does not put the copy's older bytes back over a title write (`merge_back` in
  `crates/victory-gpu/src/shadow.rs`). Reimplemented: theirs changes `copy_back_marked`, which this tree has no
  equivalent of (its `copy_back` merges into the copy and keeps one snapshot per submission).

- **fxceless/victory-fork [PR #1130](https://github.com/fxceless/victory-fork/pull/1130)** (Lkxex), GPL-3.0:
  detiling only the 64 KiB pages a texture's write watch says the title wrote, instead of the whole texture at
  every refresh (`crates/victory-formats/src/texture.rs`'s `Layout::detile_window`,
  `crates/victory-gpu/src/texture.rs`'s `Staging::refresh_pages`, `VICTORY_DETILE_DIRTY=0`). Its own page table
  and its per-page reads are not taken: our subresource ranges come from the layout (`Layout::mip_extents`) and
  the written pages are read in runs, with the rows outside the window skipped whole.

- **Marvisak/VIctory [#2163](https://github.com/Marvisak/VIctory/pull/2163)** (0xkn0wh3r3), GPL-3.0: the
  built-in BCPack table's first BC7 dictionary entry has its 32 indices right after its width bytes, where
  every other entry repeats them first and holds 31 (`crates/victory-formats/src/bcpack.rs`).

- **fxceless/victory-fork** (Geckomacabre, Codex), GPL-3.0: textures a list's shaders write that the title's own
  code then reads on the CPU are copied into a read-back buffer when the list closes and tiled into the texture's
  title memory in the console's layout, by kind (`VICTORY_TEXTURE_BACK`, off by default), with each read-back's
  allocation and fence tracked so a remapped or released allocation is not landed
  (`crates/victory-gpu/src/texback.rs`).

- **fxceless/victory-fork [PR #976](https://github.com/fxceless/victory-fork/pull/976)** (MoscaLosca), GPL-3.0: a
  compute or mesh dispatch asking for more than the host's 65,535 groups in `x` runs as pieces, each told where
  its groups start in a host-computed user-data dword the translated program adds to its group id
  (`USER_GROUP_BASE`, `crates/victory-shader/src/emit/shader.rs`, `Lower::compute_id`;
  `State::dispatch`/`dispatch_mesh`, `crates/victory-gpu/src/draw.rs`, `VICTORY_GROUP_SPLIT=0`), and the clamp's
  budget words now count the largest group count a compute record asked for as well as the mesh one.
- **fxceless/victory-fork [PR #981](https://github.com/fxceless/victory-fork/pull/981)** (avt0m4t), GPL-3.0: an
  embedded shader keys and translates on its declared DXBC bytes, so a packed record whose payload runs on past
  the container gets the same BLAKE3 as its standalone copy (`Container::as_bytes()`,
  `crates/victory-shader/src/dxbc.rs`; `Source::bytes()` for both the raw and the `SGD5` paths,
  `apps/victory/src/shader.rs`). Reimplemented on this tree's `victory translate`, which has no `warm` command.

- **Marvisak/VIctory [`8ff791351`](https://github.com/Marvisak/VIctory/commit/8ff791351)** (Marvis),
  GPL-3.0: a rectangle list's fourth corner is completed opposite the right angle, wherever the three
  corners put it. This tree's geometry stage (`emit::rect_list_gs`) already finds the corner in any order; the
  rule is now in the CPU expansion too (`right_angle`, `right_corners` in `crates/victory-gpu/src/primitive.rs`).

- **codec-d/CGBJ (codec-d/cool_fox [`ab75ac3e0`](https://github.com/codec-d/cool_fox/commit/ab75ac3e0),
  [`18bdf7932`](https://github.com/codec-d/cool_fox/commit/18bdf7932),
  [`8cc31ab37`](https://github.com/codec-d/cool_fox/commit/8cc31ab37))**, GPL-3.0-or-later: a host-memory guard for
  speculative uploads — a free-RAM and commit sample with hysteresis, a floor left free after an upload and a larger
  one before a new block of upload storage is committed, a backoff after a refused allocation instead of one attempt
  per small upload, and idle upload storage given back at once under pressure. Reimplemented for our 16 GiB PC and
  16 GiB RADV card, where a refused import falls back silently, so only pool growth waits and no texture an
  on-demand upload needs is ever deferred (`crates/victory-gpu/src/residency.rs`, `VICTORY_RESIDENCY=0`).

## Tools

Some commits were made with AI coding tools (Claude, Codex, Devin, Gemini, GPT). A contributor is
responsible for the code they submit, whoever or whatever helped write it.

Missing or wrong name? Open an issue.

- **miatson/VIctory [28b7278f](https://github.com/miatson/VIctory/commit/28b7278f)** (miatson), GPL-3.0: the
  finding that the console blends (MIN, MAX) into integer render targets while D3D12 refuses a pipeline that
  enables blending on one, so those targets are made to write unblended, per target and with independent
  blending turned on only where it was needed (`crates/victory-gpu/src/pipeline/host.rs`,
  `VICTORY_HOST_BLEND=0`). Its host-pipeline device test used `D3DCompile`; ours checks the state itself.
- **avatarmanz/VIctory-7900XTX [2be0515c](https://github.com/avatarmanz/VIctory-7900XTX/commit/2be0515cb)**, GPL-3.0:
  a page the title maps at more than one address, where a guard at one address can be written through another: the
  guarded blocks of the views beside it are given back to write together, under the one look
  (`crates/victory-gpu/src/watch.rs`, `VICTORY_GUARD_ALIASES=0`).
- **avatarmanz/VIctory-7900XTX [200a9cc5](https://github.com/avatarmanz/VIctory-7900XTX/commit/200a9cc5e)**
  (after fxceless/victory-fork [c36971b2](https://github.com/fxceless/victory-fork/commit/c36971b2)), GPL-3.0: the
  dwords of one console buffer load read by one `rawBufferLoad` for every four of them instead of a call a dword,
  behind `VICTORY_SHADER_OPT=64` (off by default, worth nothing on the card that measured it)
  (`crates/victory-shader/src/emit/lower/wide.rs`).
- **avatarmanz/VIctory-7900XTX [17c4717b](https://github.com/avatarmanz/VIctory-7900XTX/commit/17c4717b4)**
  (after fxceless/victory-fork [c36971b2](https://github.com/fxceless/victory-fork/commit/c36971b2)), GPL-3.0:
  `VICTORY_CARD_BUFFERS_MIN_KB`, so that title buffers' copies under the MiB floor can go into the card's memory
  (unset: the floor of before) (`crates/victory-gpu/src/shadow.rs`).
