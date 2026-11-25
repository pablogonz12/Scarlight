# Changelog

All notable changes to Scarlight are recorded in this file.

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): before 1.0.0, a preview tag may change
behaviour without a major version bump.

## [Unreleased]

### Added

- A Steam compatibility tool (`tools/steam-compat/`): install it into Steam's
  `compatibilitytools.d`, pick **Scarlight (Xbox Series)** under a title's Properties >
  Compatibility, and Play. It runs on the Wine of an installed Proton, in Steam's own prefix.
  `scarlight.conf` holds the options, the log is `$STEAM_COMPAT_DATA_PATH/scarlight.log`, and
  `SCARLIGHT_DRY_RUN=1` binds imports without running. Microsoft's `d3dcompiler_47.dll` is optional
  and is never shipped; `tools/steam-compat/package.sh` builds the release tarball, which a GitHub
  Actions job attaches on a tag. See [docs/steam.md](docs/steam.md).
- Linux build of the Xbox Series compatibility layer, running through Proton (Wine + vkd3d-proton)
  on Mesa's open Vulkan drivers, RADV first.
- Boot, menus, audio and gamepad input work; menus run at 30 fps.
- Built and run with one title so far; its world renders lit and textured on an RX 9070 XT with
  Mesa 26.3.1 RADV: **1.45-1.7 fps** on the
  fixed test scene in current builds, **up to 3.0 fps** in one earlier build.
- Offline shader translation: **751 of 751** world programs translate.
- Diagnostics build with per-frame statistics on copies, read-back, memory watches and pipelines.
- Title profiles: what one build needs (code hooks, shaders to skip) is read at launch from the
  user's config directory, selected by the image's own hash, so no title is known to the code. See
  `docs/title-profiles.md`.
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
