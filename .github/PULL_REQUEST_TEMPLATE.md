## What and why

<!-- One paragraph: what changes, and the evidence it is right (log line, shader count, frame timing). -->

## Credit

- Upstream PR / commit / author, if this is not new work: <!-- e.g. upstream PR #704 by @r4tedx -->
- If it came from another fork or project, name it and its licence.

## How it was tested

- Offline shader count: <!-- e.g. 692 of 751 world shaders translate, all valid SPIR-V under vkd3d-proton master -->
- Game run numbers: <!-- menus / loading / world fps, minutes run, crash or not, with and without this PR -->
- Hardware: <!-- GPU, driver + version, OS, Windows or Linux + Wine/Proton version, vkd3d-proton version, Scarlight commit -->

## Checklist

- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` is clean, and so is the `--all-features` variant
- [ ] `cargo test --workspace --locked` passes (or the failure is pre-existing and named above)
- [ ] No game names, assets, executables, screenshots or hashes of game data ([docs/asset-policy.md](docs/asset-policy.md))
- [ ] Internal `victory*` crate, binary and environment-variable names are unchanged
- [ ] If this merges an upstream PR, it is merged (not squashed or copied) so the author's commits are kept, and every new author is added to [CREDITS.md](CREDITS.md)

## AI assistance

<!-- Say which parts were AI-assisted and with what, or "none". You are responsible for code you submit. -->
