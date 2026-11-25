# Changelog

All notable changes to Scarlight are recorded in this file.

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): before 1.0.0, a preview tag may change
behaviour without a major version bump.

## [Unreleased]

### Picture

### Speed

### Load time

### Stability

### Tools

## [11.14.1] - 2026-11-25

The first release, and the first versioned one. A Scarlight version is
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
