# Changelog

All notable changes to ScarLight are recorded in this file.

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): before 1.0.0, a preview tag may change
behaviour without a major version bump.

## [Unreleased]

### Picture

### Speed

### Load time

### Stability

### Tools

- A one-command installer: it verifies the release checksum, installs versions side by side and keeps your settings (#329).

## [11.14.11] - 2026-11-28

Aimed at the copy-volume slowdown in 11.14.9 and at repeated waits for unfinished pipelines.

- **Performance:** eligible private pools are guarded again, not copied whole; aligned parts of larger copies stay write-watched (#339, #342).
- **Performance:** a pipeline that is still compiling gets one wait budget in total, not a new one every frame; draws are skipped until it is ready (#343).
- **Performance:** small savings in shadow-buffer lookup and shader instruction tracking (#345, #349).
- **Diagnostics:** a count of physical pages mapped more than once, and an opt-in per-frame CPU timeline (#340, #344).

Measured:

- One run on an RX 9070 XT (RADV, Proton) with the pipeline-wait change: pipeline waits fell to 0.0 ms and fps rose from 1.3 to 2.0, with a complete picture and no device removal. Eleven overdue pipelines finished in 631–722 ms (#343).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.11).

## [11.14.10] - 2026-11-27

Further shader correctness fixes and two diagnostic tools.

- **Picture:** early depth testing is kept for pixel shaders that ask for it, so rejected pixels no longer write through unordered views (#322).
- **Picture and shaders:** more accurate normalised buffer values; scalar values survive wave-skip joins (#315, #334).
- **Shaders:** scalar quad-mask and vector sum-of-absolute-differences instructions are lowered (#321, #324).
- **Diagnostics:** copy-back thread counters, and the render-target preview is back in diagnostics builds (#326, #328).
- **Command line:** a worker count of zero is reported as an error (#335).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.10).

## [11.14.9] - 2026-11-27

A large batch of ported shader and runtime fixes, plus hang fixes. It has a known copy-volume slowdown.

- **Performance:** less lock contention and faster mapped-view lookup; storage worker count follows the physical cores available (#281, #283, #285, #318).
- **Picture and shaders:** several ported lowering, decoding, texture-view and capability fixes (#143, #253, #306, #307, #309, #311, #313, #316, #319, #323).
- **Picture:** a fallback sampler heap, wide indirect dispatches split instead of cut off, and extra render-format handling, and two-plane presentation under an opt-in switch (#207, #308, #287, #289).
- **Stability:** a self-deadlock in memory guards is fixed and private pools are copied whole again; failed asynchronous cleanup no longer runs under the completion lock (#301, #302, #304).
- **Install/launcher:** shader caches survive rebuilds that do not change translation (#312).
- **Performance (off by default):** comparing only written pages, and resetting finished command lists on a background thread (#239, #320).
- **Diagnostics:** intentional drops are counted separately, shader coverage includes all discovered programs, and the early-pipeline cap is reported (#275, #310, #117).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.9).

## [11.14.8] - 2026-11-27

A loading-screen fix, bounded write-back waits and logs that name stalls.

- **Loading:** held DirectStorage reads are now opt-in. With the old default, releases 11.14.4–11.14.6 could not leave the loading screen (#291).
- **Stability:** write-back records a page hold before protecting the page, and its waits have a deadline and a name in the log (#296).
- **Stability:** guard handling for reserved placeholder memory was moved to ask the mapped views (#284).
- **Diagnostics:** long console-API calls and lock waits are named; the frame log shows where copied bytes came from and the five largest copies (#293, #297).
- **Tests:** an intermittent write-watch test failure is fixed (#300).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.8).

## [11.14.7] - 2026-11-27

Host-memory pressure, fence waits and copy tracking.

- **Performance:** an indirect record over a list's budget no longer uses up the budget of later records; private memory pools are tracked with page guards instead of repeated system queries (#233, #277).
- **Performance (off by default):** read-only title memory can stay resident for the GPU (#273).
- **Picture:** a placed texture view can alias the first mip levels of a texture the GPU writes (#218).
- **Stability:** the layer backs off after refused memory allocations and limits uploads according to system memory (#260).
- **Stability:** a blocking fence wait sleeps until signalled, with a time limit (#263).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.7).

## [11.14.6] - 2026-11-26

Shader and picture coverage, format handling and memory fixes.

- **Performance:** a wider GPU-side compare (on by default; `VICTORY_CARD_HASH_WIDE=0` turns it off), per-heap buffer-view passes, and an off-by-default smaller texture copy-back (#157, #256, #265).
- **Picture:** console upscaler programs now translate; a restored 64-lane mask keeps its lanes; more shader instructions are lowered (#162, #216, #230).
- **Picture:** the two-plane presentation of the lower-power console profile is composed; textures keep only the uses their format supports (#177, #254, #189).
- **Picture (opt-in):** a minimum indirect-light pass and another sampler-selection path, both off by default (#217).
- **Stability:** freed physical page numbers are reused; mesh pipelines are refused where the driver has no mesh shaders; raw page views grow when copies need it (#160, #186, #255).
- **Diagnostics/docs:** corrected explanation of a shader-converter crash (#274).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.6).

## [11.14.5] - 2026-11-26

A small release: streaming, audio and shared-lock fixes.

- **Performance:** view-memory guards are split into shards, upload buffers are reused, and the descriptor-heap rebuild interval is configurable (#258, #264, #178).
- **Loading:** a texture can be staged when only its middle mip levels have arrived (#257).
- **Picture:** a sampler chosen by a uniform branch is handled; a widened 16-bit wave sum no longer reads uninitialised memory (#225, #228).
- **Audio:** a refilled XMA voice decodes again and publishes whole blocks (#262).
- **Stability:** batched memory mappings are clipped to the reservation they start in (#266).

Measured:

- One GPU correctness test on an RX 9070 XT (Wine, RADV, Mesa 26.3.1): before #228, 3 of 6 runs gave the expected value; after it, 8 of 8 runs of that test group passed (#228). This is not a game run.

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.5).

## [11.14.4] - 2026-11-26

Texture upload and per-frame CPU changes, with further shader and format work.

- **Performance:** repeated render-target binds and root-constant-buffer fills are skipped (#226, #229).
- **Loading:** first texture uploads can start ahead of use on a copy queue (#235).
- **Performance (opt-in):** AVX2 byte comparison and lower priority for the title's idle threads (#238, #259).
- **Picture:** more tiled-texture layouts are addressed; a sampler used both for comparison and plain sampling gets two slots (#195, #210).
- **Shaders:** identical branch arms, integer clamp and a 16-bit divide fix-up are handled (#167, #219).
- **Stability:** command lists are split at texture read-backs; DirectStorage staging buffers accept only console sizes (#118, #215).

Measured:

- Opt-in AVX2 comparison, CPU microbenchmark on a Ryzen 9 9900X (release build, 4,096 pages of 64 KiB, not a game run): finding the first and last changed byte went from 2.26 ms to 0.74 ms against the 8-byte path; a second walk went from 16.0 ms to 12.8 ms (#238). The switch is off by default.

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.4).

## [11.14.3] - 2026-11-26

Work on per-frame CPU cost, picture fixes from several ports, and copy safety.

- **Performance:** textures re-detile only pages written since the last refresh; shared-lock buffer lookups and grouped refreshes; fewer repeated pipeline and declaration steps; the title's own buffer copies can run on the GPU (`VICTORY_GPU_COPIES=0` turns this off) (#86, #211, #212, #227, #231).
- **Performance (opt-in or off by default):** wider shader buffer loads, a lower size floor for GPU-side copies, and title memory the GPU can use directly (#232, #234, #245).
- **Loading:** startup reads go through the host file cache and small shared buffers are pooled, with overlapped reads opt-in; queued DirectStorage reads keep their file open (#139, #161).
- **Picture:** rectangle-list corners, integer-target blending, sampler selection, 16-bit compares, depth-plane copies and reads below a shader's constant table fixed (#182, #208, #223, #224, #221, #197).
- **Picture and shaders:** pixel-ordered (POPS) views, retries for refused textures, further ported shader fixes and decoder additions (#140, #171, #173, #250).
- **Stability:** safer copies into remapped or partly missing title memory, waits on unreadable memory, and oversized dispatches (#115, #89, #123, #206, #237).
- **Diagnostics:** adapter and feature logging, an on-picture frame-rate counter and an early-pipeline cap message (#124, #201, #180).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.3).

## [11.14.2] - 2026-11-25

A batch of picture, shader and memory-safety fixes.

- **Performance:** less repeated work when refreshing guarded views, comparing changed bytes and decoding shaders (#181, #184, #205).
- **Picture:** chosen kinds of shader-written textures can be copied back for the title's CPU; unordered views in formats the host cannot store to stay empty; stencil upload and a BCPack/BC7 decoding bug fixed (#183, #127, #134, #193).
- **Picture and shaders:** integer render targets are written unblended; 24-bit high multiplies are lowered; `ds_wrap_rtn_b32` now decodes both operands (the instruction itself is still not lowered) (#200, #192, #185).
- **Stability:** copy-back keeps its snapshot current; failed write-back jobs release their pages; memory imports the GPU refused are rejected; plain copies check the destination is writable (#214, #203, #188, #190).
- **Install/launcher:** the shader cache now notices every translation setting and host limit; Mesa shader-cache setup is documented (#194, #113).
- **Diagnostics:** a source-counted instruction coverage page, and an opt-in switch that clears the unfinished reflection image (#90, #191).

Known issues and install steps: [release notes](https://github.com/pablogonz12/Scarlight/releases/tag/v11.14.2).

## [11.14.1] - 2026-11-25

The first release, and the first versioned one. A ScarLight version is
`<GE major>.<GE release>.<revision>`: **11.14.1** is the first revision on GE-Proton11-14, and the
revision counts within that GE release, so rebasing on GE-Proton11-15 starts again at 11.15.1. The
Steam tool shows **Scarlight 11.14.1**, the tag is `v11.14.1`, and two releases install side by side.
[docs/RELEASES.md](docs/RELEASES.md) has the scheme, the install and the cadence.

### Picture

- Zero-copy on the unified core: the title's memory is used in place by default for the class the
  probe proved, and a startup self-check turns vkd3d-proton's silent fallback into one log line.
- Shaders: a table descriptor read only as numbers takes its raw view's values, which completes the
  runtime set at **2,912 of 2,912** programs; u16 MADs and `v_lerp_u8` are lowered.
- Shaders: a joined lane mask ballots at its own edge, and the ray-tracing node test now misses a
  child that is not there and empty or NaN boxes.
- Formats: tiled copies go as runs, and BCPack slots through a window table.
- Descriptor heaps: views are refreshed and transitioned before the draws that use them, and a
  root signature whose heap-directly-indexed flags the driver refuses is retried without them.
- Ray tracing: the structure builder's by-address writes stay in host copies, which removes the
  crash that kept the builder off by default; the builder is still off by default.

### Speed

- Screen passes get workers and a budget of their own.
- Pipelines that the title asks for again and again compile first, with anti-starvation, and the
  compile order puts live pipelines ahead of the rest; structurisation itself is faster.
- Copy-back runs on a per-queue worker behind `VICTORY_ASYNC_WRITEBACK=1` (default off), upload
  scratch is reused across command lists, and shader resource metadata is cached across runs in
  `shader-meta.bin`.
- A dropped draw now says whether its pipeline was refused or is still being made.

### Load time

- A heap that outgrows the host's is reported once, not per slot: first frame **21 s → 33 ms** on the
  measured run.
- Narrowed streamed SRVs are remade when residency widens, behind one residency store per view's mip
  window.
- The empty pipeline library asks for a null blob instead of a pointer at nothing, and the crash
  class seen at boot is gone.

### Stability

- Guards for the crashes that ended runs: overflowing buffer views, the XG tiling count query with a
  null out pointer, and a host copy nothing wrote no longer asked for at submission.
- HLE: a page scatter is mapped in runs instead of a page at a time, page destinations keep their
  view and tell the write watch first, and the scatter diagnostics count a call once.
- Copy-back reaches pages the write watch covers under Wine.
- The GPU tests run on the real adapter under vkd3d-proton in CI, not only on the software one.

### Tools

- A Steam compatibility tool (`tools/steam-compat/`) with versioned releases: install it into Steam's
  `compatibilitytools.d`, pick **Scarlight 11.14.1** under a title's Properties > Compatibility, and
  Play. It runs on the Wine of an installed Proton, in Steam's own prefix. `scarlight.conf` holds the
  options, the log is `$STEAM_COMPAT_DATA_PATH/scarlight.log`, and `SCARLIGHT_DRY_RUN=1` binds
  imports without running. Microsoft's `d3dcompiler_47.dll` and DXC's two DLLs are never shipped;
  `tools/steam-compat/package.sh` builds the release tarball and a tag starts the release job.
  See [docs/RELEASES.md](docs/RELEASES.md) and [docs/steam.md](docs/steam.md).
- Switches for logging, GPU timing and frame capture, plus experimental async write-back.
- Documentation: Linux setup, architecture, testing method, hardware status and roadmap in `docs/`,
  and a progress page whose numbers are generated from counted sets by `tools/progress/`.
- The world shader set is compiled into real pipelines, not just translated: 749 of 751 on RADV here,
  and through Mesa's noop shims for RDNA1-3, Vega and three Intel ANV families (see
  [docs/hardware.md](docs/hardware.md#compile-check-per-family)).

### Known limitations

- **Not playable**: 1.45-3 fps in the world, with multi-second stalls.
- Faces of some characters still fail to render; close-up character detail and walls are missing in
  some builds.
- Shadowed areas are dark: the title's ray-traced lighting is incomplete.
- Intro frames can show out of order.
- Tested on one GPU only: RX 9070 XT on RADV. Other Mesa GPUs are untested.
