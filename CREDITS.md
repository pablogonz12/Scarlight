# Credits

- **Duff [c430b1de](https://github.com/Marvisak/VIctory/commit/c430b1de)**, GPL-3.0:
  pack typed buffer d16 loads two channels per register, retaining integer bits.
- **Marvisak/VIctory [13091f6e](https://github.com/Marvisak/VIctory/commit/13091f6e)**, GPL-3.0:
  array level queries use spatial coordinates only.

Scarlight is a port of **VIctory** ([Marvisak/VIctory](https://github.com/Marvisak/VIctory)),
GPL-3.0. Its history, authorship and commit dates are kept.

The lists below are every author with a commit in this tree on `main`, plus the contributors named
in `Co-authored-by:` trailers, read from `git log main`.

## Project

- **VIctory** — Marvis and the VIctory contributors, GPL-3.0.

## Authors

9hvn9yt2vk-netizen, A6thGraderFr, Abdellah K., acleofe, adamfx, Advait Palve, AlanQiang0418,
alessiodeia, Andrea Casale, artor410, AscilCH, BaggyALPHA, bartikele, BastienRdz, Brazlucas, CaravelaLabs,
Carlos Sant'Anna, dadou000, Dathinel, desp, Duartt - Ryu Garage, Duff, EatCodeSleep2,
Enzo Souza Duarte, ESAT, Exorion, fever308, FIocker, franticfrostin, fuzeofficial, Gabriel Cruz,
gingertonik, GrapeJuice429, Halfect, Henrique Bueno, hophaver, ifBars, ismailchaka1, itsRevela, Jeremy Goldberg,
JMAN730, julio, KaiserBloo, KoLolX3, lameloball2813-ui, LaunchDoesntCode, LF2000 Builder,
localshotgun, lordon1a, Louis Faust, LUIS LARIOS, Luiz Neves, Lyrnc, Manuel Pérez, Maraco91,
marquezini, Marvis, Matheus, meower, nathanbzrr, Nikita Romanovich, Noname0202, Nostyxx, Ortomanic,
pablogonz12, Pablo González, Pedro Fortunato, Peter Bonanni, QB, quaternion, r4., raffclar,
ruvenesh, Saitken91, Sakura-Byte, SkippySteve, swarm, tedin7, TG0000, undeadhuman37, uwutaube, Vanthys,
wrothgaril, Yaekai.

## Co-authors

andrewpozdnakov7-jpg, Bebrik229 (the `shader-meta.bin` shader metadata cache, VIctory #2614), BlueKaiman, kais7654, pushkarreddyy, shh.

## Ports of fork pull requests

Code reimplemented here from a fork's pull request, because the fork's tree had diverged from ours;
the design and the author are credited in the commit's `Co-authored-by:` and here.

- **Marvisak/VIctory #2401** (Mumogg) — the Wine `ExitProcess` hook that makes a host driver
  compiler crash cost one pipeline instead of the run ([`pipeline/wine.rs`](crates/victory-gpu/src/pipeline/wine.rs)),
  adapted to our pipeline workers and library.
- **fever308/victory-corgi `38e367763`** (fever308) — the bounded, size-bucketed pool of mapped `UPLOAD`
  buffers that a list copies title memory through: a buffer a list let go of is kept by its size class and reused by
  the next copy, and only after every submission in the air has finished
  ([`upload_pool.rs`](crates/victory-gpu/src/upload_pool.rs), `VICTORY_UPLOAD_POOL=0` off).
- **fxceless/victory-fork [#1097](https://github.com/fxceless/victory-fork/pull/1097)** (Azemed), GPL-3.0 — a
  runtime placeholder reservation's mapped runs come off the views the GPU side already keeps, so the guards note its
  holes absent instead of a failing `VirtualProtect` and a `VirtualQuery` a block, and the copies' committed-byte
  counts skip the kernel too (`crates/victory-gpu/src/inplace.rs`, `guards.rs`, `VICTORY_PLACEHOLDER_FAST=0` off).

## Design notes

- **Belobug/VIctory-Belobug-AMD `84eae352a`** (local) — identified an indirect
  dispatch requesting 67,440 groups that lost its tail under the 65,535-group clamp.
- **fuzeofficial** ([Marvisak/VIctory #2608](https://github.com/Marvisak/VIctory/pull/2608)), GPL-3.0: the
  idea of cutting a command list at its texture read-backs and running the part after one in a submission
  of its own, so a copy or draw recorded behind a read-back is prepared after its bytes are in title
  memory (`crate::list::cut_read_back`). Reimplemented on the parts a queue already runs.
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
- **fxceless/victory-fork [#1014](https://github.com/fxceless/victory-fork/pull/1014)** (Martin Hartfelder),
  GPL-3.0: a group of 64 threads to a 64 KiB piece in the card compare's hash, thread `t` hashing steps `t`,
  `t + 64`... of the piece and the first thread folding the lanes in order, instead of one thread walking the
  piece 16 bytes at a time (`crates/victory-gpu/src/host.rs`, `VICTORY_CARD_HASH_WIDE`).
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
- **JMAN730/victory** ([project-flamingo#685](https://github.com/JMAN730/project-flamingo/pull/685)),
  GPL-3.0: a `LaneExit::Branch` whose two arms name one block is lowered as the jump it is, so the
  edge map's `(from, to)` key is not written twice and that successor's PHIs keep their predecessor
  (`crates/victory-shader/src/emit/lower.rs`, `crates/victory-shader/src/emit/branch_tests.rs`).
- **JMAN730/victory** ([project-flamingo#675](https://github.com/JMAN730/project-flamingo/pull/675)),
  GPL-3.0: VOP3 integer `clamp` on add/sub is unsigned saturation (carry → all ones, borrow → zero),
  applied in the lowering instead of refused as an integer output modifier, and a clamped add is no
  wrapping sum for address analysis (`crates/victory-shader/src/emit/valu_clamp.rs`,
  `crates/victory-shader/src/resource.rs`).
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
- **jmehru/victory-amd-rx9070 [3298b5dad](https://github.com/jmehru/victory-amd-rx9070/commit/3298b5dad)**
  (jmehru), GPL-3.0, a port of PastiiJay/klbf-lab-beta `ee7081b` (itself MiamiGame/XTP `00c60fb` and
  `188d895`), with the block patterns generated from Mesa's addrlib (MIT): addressing the pipe and bank
  XOR swizzle modes (`_T`, `_X`) for the console's pipe configuration, so a texture the title's CPU reads
  or writes in one has an address. Reimplemented against this tree's copies (`Buffers`, `detile_spans`,
  `walk_runs`): the XOR modes copy element by element, the patterns are regenerated by
  `crates/victory-formats/src/texture/generate.rs`, and `VICTORY_TEXTURE_XOR=0` turns it off.
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
  GPL-3.0-or-later: a placed view aliased to a texture the host writes may cover a leading prefix of that
  texture's mip chain (and its format may be block-compressed as well as texel-wise as wide), found by address
  in the host-written table as well as by the placed table (`crates/victory-gpu/src/resource.rs` `alias_of`,
  `texture.rs` `alias`/`with_host_written_at`); `VICTORY_ALIAS_WIDE=0` restores the narrower match.
  GPL-3.0-or-later: the pass that draws a depth plane's twin into the plane runs inline, in the list that
  wrote the twin (and that list replays the title's state after it), so a depth test in the same list sees
  what the shaders wrote (`crates/victory-gpu/src/host.rs` `twin_use`/`pass_plane`, `draw.rs` `replay`);
  `VICTORY_PLANE_INLINE=0` waits for the list's close, as before.

- **Mesa** ([aco_opcodes.py](https://gitlab.freedesktop.org/mesa/mesa)), MIT: the reference list of
  RDNA2 instruction forms in `tools/progress/rdna2_isa_forms.txt` is generated from it.
- **uwutaube/VIctory [217229460](https://github.com/uwutaube/VIctory/commit/217229460)** (uwutaube), GPL-3.0,
  a port of Marvisak/VIctory [#1778](https://github.com/Marvisak/VIctory/pull/1778) by fiestorra: a sampler heap of
  our own bound when the title binds none, because every root signature we make indexes the sampler heap directly
  and D3D12 requires a heap of each kind before such a signature is set (the debug layer's id 1419; NVIDIA removes
  the device). `VICTORY_SAMPLER_HEAP=0` turns it off.

- **JMAN730/project-flamingo** ([#666](https://github.com/JMAN730/project-flamingo/pull/666)), GPL-3.0: page
  numbers of freed physical pages are handed out again instead of only ever growing, and allocation stops at the
  title's 21-bit `number >> 4` field (`crates/victory-hle/src/modules/xmem.rs`, `VICTORY_PAGE_RECYCLE=0`).
- **Marvisak/VIctory [#2507](https://github.com/Marvisak/VIctory/pull/2507)** (TheGrayOrange), GPL-3.0: the
  DirectStorage staging buffer takes the capacities the console documents (0, 16, 20, 24, 28, 32 MiB) and refuses
  any other size, keeping the one the scheduler has (`crates/victory-hle/src/modules/dstorage.rs`).
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
- **DexNom/VIctory [76749ce5](https://github.com/DexNom/VIctory/commit/76749ce58da23a43a3500454a8b69c53f05aa477)** (DexNom), GPL-3.0: a
  wave64 lane mask parked in two SGPRs and restored one `s_mov_b32` at a time is recognised as the mask it was
  copied from when both words still trace back to the same known pair through scalar moves
  (`crates/victory-shader/src/structure.rs`); `VICTORY_DX_LANE_MASK_MOVES=0` keeps the split pair opaque.

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

- **Belobug/VIctory-Belobug-AMD `ambient.rs`**, via **fxceless/victory-fork [PR #1093](https://github.com/fxceless/victory-fork/pull/1093)**
  (m335671), GPL-3.0: the least indirect light, a compute pass after the program that resolves it, shaded from the
  scene's depth alone and packed as R9G9B9E5 words (`crates/victory-gpu/src/ambient_floor.rs`,
  `VICTORY_AMBIENT_FLOOR`). Its program name and both table offsets are not taken: the runner names the program and
  the views, so no title's own hold in this tree.

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
- **DexNom/VIctory [ddb73326](https://github.com/DexNom/VIctory/commit/ddb73326b09e600bd2405ee5c987fa611098aaa1)** (DexNom), GPL-3.0: a
  sample whose sampler comes from a uniform branch between two immediate samplers binds both as static
  samplers, samples twice with the original derivatives and selects the texel on the edge that supplied the
  console's sampler (`crates/victory-shader/src/emit/lower.rs`); this is what recovered the archive's refused
  windshield and wiper pixel and mesh variants.

- **avatarmanz/VIctory-7900XTX** (GPL-3.0), commit [b6202840a](https://github.com/avatarmanz/VIctory-7900XTX/commit/b6202840a):
  the idea of comparing title bytes under shader writes with AVX2 rather than eight bytes at a time: the copy-back's
  piece walk, its first and last difference and the differing runs take thirty-two bytes a compare where the CPU has
  AVX2, checked at run time, with the eight-byte walk kept as the fallback (`crates/victory-gpu/src/compare.rs`,
  `VICTORY_AVX2_COMPARE=1`; on his machine 14 -> 4-7 ms of compares a frame). His compares run on threads of their
  own; ours already do (`wide` in `shadow.rs`).
- **Marvisak/VIctory [#2773](https://github.com/Marvisak/VIctory/pull/2773)** (kais7654), GPL-3.0: the idea
  that a format's uses are the host's `CheckFeatureSupport` answer, and a use it does not take left out of the
  resource it makes -- a typeless format, which the device reports no use for, keeping the uses of its
  description (`crates/victory-gpu/src/resource.rs` `uses_taken`/`taken_flags`, `VICTORY_FORMAT_USES=0`). The
  views side is in from the round before (`descriptor::host::fits`, `VICTORY_TYPED_UAV`).
- **HUMAN** ([HumanlyDev/HUMAN](https://github.com/HumanlyDev/HUMAN)) by HumanlyDev and bdycounter-sudo,
  GPL-3.0-or-later: one immediate sampler constant used both for normal and for comparison sampling takes two
  static sampler slots (`crates/victory-shader/src/emit/lower.rs`, `samplers`/`cmp_samplers`), where our bit
  per slot made the normal use sample with a comparison sampler.
- **Marvisak/VIctory [`8ff791351`](https://github.com/Marvisak/VIctory/commit/8ff791351)** (Marvis),
  GPL-3.0: a rectangle list's fourth corner is completed opposite the right angle, wherever the three
  corners put it. This tree's geometry stage (`emit::rect_list_gs`) already finds the corner in any order; the
  rule is now in the CPU expansion too (`right_angle`, `right_corners` in `crates/victory-gpu/src/primitive.rs`).
D
- **JMAN730/project-flamingo [#734](https://github.com/JMAN730/project-flamingo/pull/734)** (Yaekai, with swarm,
  Blitz Studios and axknxla), GPL-3.0: the two-plane present compositor for the Series S menu -- the fullscreen
  triangle, its premultiplied blend, the explicit root signature for Wine, the retry after an initialization that
  failed, and the pair check that leaves every other present on the base-only path -- reimplemented for this
  tree's presenter (`crates/victory-gpu/src/present_compositor.rs`, `Host::present_overlay`), whose present keeps
  its own late-free lists and present fill. It supersedes the same idea in fxceless/victory-fork
  [#188](https://github.com/fxceless/victory-fork/pull/188) (sagouindupm), GPL-3.0, which bundled it with an
  `ffxDispatch` stand-in that our #162 now makes unnecessary.
- **codec-d/CGBJ (cool_fox) `1ad6e67aa`**, GPL-3.0: the retail two-plane present's plane stride is 72 bytes
  (`RetailPlane`, asserted at compile time) and not the common parameter prefix's size; and no barrier is
  recorded into the state a presented texture is already in (`COPY_SOURCE -> COPY_SOURCE` is illegal, and
  that is where a title leaves one). The second rule is in `host::moved` (`crates/victory-gpu/src/host.rs`);
  the stride was already ours (`queue::second_plane_resource`, 0x48).
- **avatarmanz/VIctory-7900XTX [350c68648](https://github.com/avatarmanz/VIctory-7900XTX/commit/350c68648)** (avatarmanz), GPL-3.0: a texture's first upload started ahead on a copy queue of its own (`texture/ahead.rs`) and a slot shown only once that upload ran (`descriptor/host.rs::show_after`, 3bab23c06), which there ended a device loss every ~16 minutes. Reimplemented on this tree's texture path: `crates/victory-gpu/src/texture/ahead.rs`, the copy queue made at device creation and a submission's wait on its fence in `crates/victory-gpu/src/host.rs`, `VICTORY_UPLOAD_AHEAD=1` and `VICTORY_SHOW_AFTER`.

- **DexNom/VIctory [921d8d3b4](https://github.com/DexNom/VIctory/commit/921d8d3b43b44f3a6ca4b44095edd3c4baf10a87)**,
  GPL-3.0: a pixel shader's quad permutations (`ds_swizzle_b32` quad_perm and DPP) read through quad operations,
  whose values include a helper lane's, as the console's do at a triangle's edge; common broadcasts and exchanges
  take one read, and a DPP8 that selects across quads falls back to a wave-lane read (`VICTORY_DX_PIXEL_QUAD=0`
  turns it off). Its other two parts: each view heap's run of raw page views grows with the copies that need one
  (the 1,023-copy cap is gone, `crates/victory-gpu/src/memory.rs`), and a texture whose interior mips alone have
  arrived is staged instead of treated as away (`crates/victory-gpu/src/texture.rs`).
- **codec-d/cool_fox [`18bdf7932`](https://github.com/codec-d/cool_fox/commit/18bdf7932)**, GPL-3.0: the
  buffer-view pass keeps its slot table, its once-a-frame gate and the refresh interval of unwatched host
  copies on the heap that owns them rather than in process-wide tables keyed by the heap's address, and an
  unwatched copy keeps its interval only while its views read inside the span the last refresh copied.
  Reimplemented in `crates/victory-gpu/src/buffersweep.rs` (this tree has no deferred list refresh to scope,
  so its `strides_gpu` filter has nothing to guard here).
- **fxceless/victory-fork [PR #980](https://github.com/fxceless/victory-fork/pull/980)** (fxceless), GPL-3.0:
  the arena, the layer a resident stands on: a title region opened as a D3D12 heap and a buffer placed over it, so
  the GPU reads and writes the title's own memory (`crates/victory-gpu/src/arena.rs`, `VICTORY_ARENA=1`, off by
  default). The part grid, the region registry, the budget and the canary are reimplemented on this tree's
  `inplace`; the canary here proves each part **both ways** with our own `Rig` (#188), which the fork's tests do not.
- **fever308/victory-corgi [f4c1175a1](https://github.com/fever308/victory-corgi/commit/f4c1175a1)**, GPL-3.0: the
  blocks view memory is guarded by, sharded across cache-line-aligned locks with contiguous block runs a shard and
  the shards of a range taken in ascending order, so a copy does not stop the title's write faults elsewhere
  (`crates/victory-gpu/src/guards.rs`, `VICTORY_GUARD_SHARDS=1` for the one lock of before).

- **Marvisak/VIctory [#1264](https://github.com/Marvisak/VIctory/pull/1264)** (Marvisak), GPL-3.0: a batched
  run of page mappings asks the host where the placeholder it starts in ends (`placeholder_room`, one
  `VirtualQueryEx`) instead of trusting the pages to end at the same boundary, so a run across two placeholders
  is clipped rather than refused after its range was unmapped (`crates/victory-hle/src/modules/xmem.rs`,
  `VICTORY_MAP_CLIP=0` turns it off).

- **codec-d/CGBJ (codec-d/cool_fox [`ab75ac3e0`](https://github.com/codec-d/cool_fox/commit/ab75ac3e0),
  [`18bdf7932`](https://github.com/codec-d/cool_fox/commit/18bdf7932),
  [`8cc31ab37`](https://github.com/codec-d/cool_fox/commit/8cc31ab37))**, GPL-3.0-or-later: a host-memory guard for
  speculative uploads — a free-RAM and commit sample with hysteresis, a floor left free after an upload and a larger
  one before a new block of upload storage is committed, a backoff after a refused allocation instead of one attempt
  per small upload, and idle upload storage given back at once under pressure. Reimplemented for our 16 GiB PC and
  16 GiB RADV card, where a refused import falls back silently, so only pool growth waits and no texture an
  on-demand upload needs is ever deferred (`crates/victory-gpu/src/residency.rs`, `VICTORY_RESIDENCY=0`).
- **codec-d/cool_fox [`e05d4633a`](https://github.com/codec-d/cool_fox/commit/e05d4633a)**, GPL-3.0: a
  `SetEventOnCompletion` with no event blocks the guest thread on the fence's value with `WaitOnAddress`, and
  `Signal`/`Set` wakes it with `WakeByAddressAll` (`crates/victory-gpu/src/fence.rs`; the wait here is bounded
  by `VICTORY_FENCE_BLOCK_MS` and lets the queue run what it holds between slices).
- **codec-d/cool_fox [`8cc31ab37`](https://github.com/codec-d/cool_fox/commit/8cc31ab37)**, GPL-3.0: an XMA
  voice whose input the title publishes again while the context stays enabled decodes on instead of staying
  ended, and the decode pass publishes only whole blocks (a short one only as the flushed tail), trying a few
  frames for a block and no more so a voice that produces nothing cannot hold the lock
  (`crates/victory-hle/src/modules/audio.rs`, `VICTORY_XMA_REFILL`).

## Tools

Some commits were made with AI coding tools (Claude, Codex, Devin, Gemini, GPT). A contributor is
responsible for the code they submit, whoever or whatever helped write it.

Missing or wrong name? Open an issue.

- **fxceless/victory-fork [PR #1040](https://github.com/fxceless/victory-fork/pull/1040)** (Moon0755), GPL-3.0:
  `v_div_fixup_f16` lowered through the f32 `v_div_fixup`'s special cases (an f16 is exact in f32), its table of
  the ISA's checks and its generated case grid (`crates/victory-shader/src/emit/lower.rs`,
  `VICTORY_SHADER_DIV_FIXUP_F16=0`). Its `v_div_fixup_f32` rewrite (a `|q|`-all-ones check and a different
  default NaN) is not taken: the f32 arm stays as it was, only moved into the shared helper.
- **miatson/VIctory [28b7278f](https://github.com/miatson/VIctory/commit/28b7278f)** (miatson), GPL-3.0: the
  finding that the console blends (MIN, MAX) into integer render targets while D3D12 refuses a pipeline that
  enables blending on one, so those targets are made to write unblended, per target and with independent
  blending turned on only where it was needed (`crates/victory-gpu/src/pipeline/host.rs`,
  `VICTORY_HOST_BLEND=0`). Its host-pipeline device test used `D3DCompile`; ours checks the state itself.

- **avatarmanz/VIctory-7900XTX** ([ca1d519f1](https://github.com/avatarmanz/VIctory-7900XTX/commit/ca1d519f1),
  [44629de2e](https://github.com/avatarmanz/VIctory-7900XTX/commit/44629de2e), and the four commits beside them),
  GPL-3.0, after ZJOU acf478b8 and Belobug 92d353f8, 84ea9c12, dd799aad and 76387c2d (romainhedouin): per-frame CPU
  work taken off the submitting thread — render targets compared by their views and not set again
  (`crates/victory-gpu/src/bindcache.rs`, `VICTORY_BIND_CACHE=0`), a pipeline a list binds again neither kept nor
  named again (`HostList::bind_pipeline`), a root constant buffer not 256-byte aligned filled into scratch memory
  once per list (`HostList::fill_scratch`), a range shaders write declared once until the list next writes title
  memory (`VICTORY_DECLARE_ONCE=0`), an indirect record that does not fit its list's budget taking nothing from the
  ones after it and the frame line counting those refused (`VICTORY_INDIRECT_REST=0`), and the heaps' records copied
  as bytes read, compared and sorted out on several threads.
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
D
- **fever308/victory-corgi [95641affc](https://github.com/fever308/victory-corgi/commit/95641affc)**, GPL-3.0:
  the title's "idle" and "lowest" threads (the streaming pager's among them) run "below normal", and the process
  can run "above normal", so a build or a scan beside the title no longer starves its loading, behind
  `VICTORY_TITLE_PRIORITIES=1` and `VICTORY_PROCESS_PRIORITY=above` (both off by default)
  (`crates/victory-hle/src/modules/kernel.rs`, `crates/victory-hle/src/runtime.rs`).
- **Belobug/VIctory-Belobug-AMD [4d291d4](https://github.com/Belobug/VIctory-Belobug-AMD/commit/4d291d4)**
  (davutsimsekk) via **Yoshi-233**, GPL-3.0: operandless VOP1 (`v_nop`, `v_pipeflush`, `v_clrexcp`) ignores its
  source field, so a marker byte there is not an extra dword and no longer swallows the next instruction, and the
  VOP3 encoding of `v_cmpx*` writes EXEC rather than its VDST, for both wave sizes
  (`crates/victory-shader/src/gfx10/{mod.rs,fields.rs}`).
- **jaschinski97-ops**, Belobug/VIctory-Belobug-AMD [15cc76c39](https://github.com/Belobug/VIctory-Belobug-AMD/commit/15cc76c39):
  regression tests for interpolation first halves read through a phi and standalone half-precision first halves
  (`crates/victory-shader/src/io.rs`).
- **Yoshi-233 [coolfox/main PR #7](https://github.com/Yoshi-233), from fxceless/victory-fork [9bf4a0f](https://github.com/fxceless/victory-fork/commit/9bf4a0f)**,
  GPL-3.0: the RGB dump decodes the packed 16-bit BGR targets (B5G6R5, B5G5R5A1, B4G4R4A4, host formats 85, 86,
  115) and keeps padded rows aligned (diagnostics only) (`crates/victory-gpu/src/capture.rs`).
- **fever308/victory-corgi [0aa32360e](https://github.com/fever308/victory-corgi/commit/0aa32360e)**, after
  **fxceless/victory-fork [#1152](https://github.com/fxceless/victory-fork/pull/1152)** and
  **[#1170](https://github.com/fxceless/victory-fork/pull/1170)**, GPL-3.0: a texture copied back for the title's
  CPU takes only the mip and array slices the list's texture UAVs expose (the union of the views, instead of every
  mip and slice), leaves the texels it did not take as they were, and the texture's recorded state is put back
  after the copy, behind `VICTORY_TEXTURE_BACK_UNION=1` (off by default) (`crates/victory-gpu/src/texback.rs`,
  `crates/victory-gpu/src/host.rs`, `crates/victory-gpu/src/draw.rs`, `crates/victory-gpu/src/descriptor/host.rs`).

- **Belobug**, [e70e031c](https://github.com/Belobug/VIctory-Belobug-AMD/commit/e70e031cf): accept native shader code stored without its wrapper.
- **Manuel Pérez**, [VIctory #114](https://github.com/Marvisak/VIctory/pull/114): run failed asynchronous call cleanup outside the completion lock.
- **Senchsus**, [VIctory #682](https://github.com/Marvisak/VIctory/pull/682): legacy multiply treats denormal operands as zero.
- **CaravelaLabs**, [shader decoder change](https://github.com/Belobug/VIctory-Belobug-AMD/commit/3d71ad1a6547836ac1dbd19bfab2d759c965d307): decode the ATC probe immediate, resource width, and cache flags.
- **beamdotdev**, [fxceless/victory-fork #1146](https://github.com/fxceless/victory-fork/pull/1146), GPL-3.0: reset settled submission lists on an optional background thread.
