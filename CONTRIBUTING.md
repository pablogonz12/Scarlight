# Contributing to ScarLight

Thanks for helping. ScarLight is GPL-3.0; the project it is built on is named in [CREDITS.md](CREDITS.md), and its
history, authorship and commit dates are kept in this tree.
Fixes arrive as merged commits that keep their authors (see [CREDITS.md](CREDITS.md)), plus our own
work. Contributions of any size are welcome, and a test report is as useful as a patch.

## Picking something to do

* [`good first issue`](https://github.com/pablogonz12/Scarlight/labels/good%20first%20issue)
  is self-contained work with the file to touch and the evidence that finishes it written down.
* [`help wanted`](https://github.com/pablogonz12/Scarlight/labels/help%20wanted) is
  wanted but harder, or needs hardware we do not have.
* [`test report`](https://github.com/pablogonz12/Scarlight/labels/test%20report) marks
  reports on other Mesa GPUs. A run on your card is work we cannot do ourselves.
* A question that is not a bug belongs in
  [Discussions](https://github.com/pablogonz12/Scarlight/discussions), not in an issue.

Comment on the issue before you start so two people do not build the same thing. First-time
contributors are not expected to know Rust's Windows bindings or Mesa: say what you are unsure of in
the issue and somebody will narrow it down.

## No game files? Here is what you can do

Most of the repository needs nothing from the title, and none of the following reads a game file:

* `cargo test --workspace`, the two clippy runs and `cargo xtask policy` — the cheap ladder CI runs
  ([above](#test-and-lint-exactly-as-ci-does)).
* The translator's tests: every case is built from hand-written instruction words and inline HLSL
  (`crates/victory-shader/src/emit/`), and the HLSL cases skip themselves when `VICTORY_CONSOLE_DXC`
  is not set.
* Packaging and installing: `tools/steam-compat/package.sh` builds the release tarball,
  `tools/steam-compat/install.sh` installs from a source checkout, and a scratch
  `compatibilitytools.d` plus a Steam restart shows whether the entry appears.
* Documentation, the entry script, and reading Mesa or vkd3d-proton with us.

ScarLight is written with AI assistance (Claude, DeepSeek); every change goes through CI and review.

A dry run (`SCARLIGHT_DRY_RUN=1` in a title's Launch Options: imports resolved and bound, no GPU;
[docs/steam.md](docs/steam.md)) needs a title's `.exe`. A run, a frame rate and a test report need your
own copy of a title: the
[test report form](https://github.com/pablogonz12/Scarlight/issues/new?template=test-report.yml) says
what to send. The [`help wanted`](https://github.com/pablogonz12/Scarlight/labels/help%20wanted)
issues each say which part of their work can be done without one.

## Where pull requests go

* Open pull requests against **`main` only**. Nobody pushes to `main` directly.
* Keep a pull request to one verifiable outcome. Say in the description what it changes and the
  evidence it is right: a log line, an offline shader count, frame timings.
* Before review, CI must be green and **CodeRabbit** must have reviewed it. Answer CodeRabbit's
  real findings; ignore style nits you disagree with, but say why.
* Small fixes: a branch, a pull request, done. Large ones: open an issue first so nobody
  duplicates work.

## Naming rule

* **Docs, commit messages, issue and pull request text never name the title or its developer.**
  Say "the title", "the game's files", "the user's copy". Not the game's name, not its studio's name,
  not their abbreviations, not in a comment or a commit either. This is not modesty: it is what keeps
  the repository up.
* **Internal names stay `victory*`**: crates (`victory-core`, `victory-gpu`, …), the `victory`
  binary, `victory.toml`, `VICTORY_*` environment variables. Renaming them breaks every upstream
  patch we still want to merge, so do not.

## Build

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it (`rustup toolchain install`)
with the `x86_64-pc-windows-msvc` target and the `rustfmt` and `clippy` components.

**On Linux** (cross-build with [cargo-xwin](https://github.com/rust-cross/cargo-xwin), no MSVC needed):

```sh
# once: clang is required by the xwin linker setup, cargo-xwin alone is not enough
cargo install cargo-xwin
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
# add --features diagnostics for per-frame copy and write-watch statistics
```

Install your distribution's `clang` package as well (`clang` on Arch, `clang` on Debian/Ubuntu,
`clang` via `xcode-select` on macOS); `cargo xwin` drives it as the cross linker.

The Wine/Proton/vkd3d-proton setup that runs here is in the [quick start](README.md#quick-start) and
[docs/linux.md](docs/linux.md).

**On Windows** (MSVC):

```sh
cargo build --release -p victory
```

## Test and lint exactly as CI does

CI (`.github/workflows/ci.yml`) runs on `windows-latest`, calls `rustup toolchain install` so the
pinned version and components are used, then runs these three steps. Run the same commands locally:

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

Warnings are errors, so a clean tree is required. These commands build for the **host** target, so on
Linux they do not compile the Windows-only test modules that CI runs; only a run on Windows matches
CI exactly. Cross-target checks such as
`cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings` catch
compilation breaks in that code, but they are not a substitute for the Windows test run.

### The checks before a pull request

```sh
cargo xtask install-hooks                             # once: policy check on every commit
cargo xtask policy                                    # asset policy over tracked and untracked files
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

`cargo xtask policy` is the check that refuses files: it is an allowlist of
`.rs`, `.toml`, `.md`, `.txt`, `.lock`, `.yml`/`.yaml`, `.json`, `.hlsl`/`.hlsli`, `.ps1`, `.cmd`
and `.sh`, each under 512 KiB, text only, with no long hex or base64 runs. A new file type needs a
change to `xtask/src/policy.rs` with a reason, and that is a pull request of its own. Run it before
you push, not only on commit: an untracked `.png` in the tree fails the check even when you never
`git add` it. `.gitignore` hides the common data types; a file type it does not name is still
checked, and `--staged` skips untracked files entirely.

`cargo test` includes xtask's `repository_passes_policy`, the asset-policy check, and the shader
tests in `crates/victory-shader/src/emit/diff_tests.rs`. Those tests compile inline HLSL cases with
DXC, and they run only when `VICTORY_CONSOLE_DXC` points at a console DXC binary; they need no game
files. Keep the tree formatted (`cargo fmt`);
CI does not check formatting, so do not reformat unrelated files.

## Asset policy

**Nothing from the game is ever stored here**, and nothing derived from it: no assets, no
executable bytes, no shader dumps, no extracted tables, no screenshots of game content, no hashes
of game data. Build identities (BLAKE3 of a user's executable, CodeView GUID), our own
byte-pattern signatures and our own synthetic fixtures are fine. The full list, including the one
documented exception, is in [docs/asset-policy.md](docs/asset-policy.md); read it before adding
anything binary.

## Crediting upstream authors

We credit by **merging the real commits**, not by copying code:

* Merge the upstream branch or pull request so its commits and author are kept (`git merge`, or a
  merge commit after resolving conflicts keeping both sides). Do not squash somebody else's work
  into ours, and do not retype their patch as an unnamed commit.
* Add the author to [CREDITS.md](CREDITS.md) if the merge brings in a name that is not listed yet
  (the list is read from `git log main`).
* Fixes that also help Windows or NVIDIA go upstream to the project credited in [CREDITS.md](CREDITS.md). The point is to make upstream
  patches mergeable, not to fork forever.
* Disagreements about a merge go in the pull request, not in commit messages about people.

## Reporting without code

Use the issue forms: **Bug report** for a failure, **Test report** for a run on your hardware
(fps, crash or not, minutes run). Both remind you what never to attach. The last 50 lines of the
log around the failure are worth more than a screenshot.

A test report is the only way a hardware row in [docs/hardware.md](docs/hardware.md) moves from
*Expected* to *Tested*, and a card that fails is as useful as one that works: a failure on a gated
path is our bug. Fill in the form as it asks — commit, GPU, driver family (RADV, ANV or NVK) and
Mesa version, kernel, distribution and session, Proton and vkd3d-proton versions, how far the run got
with the frame lines, and what looked wrong. Counts, timings and log lines only: no screenshots of
game content, no game files, no hashes of game data and no links to builds
([asset policy](docs/asset-policy.md)). Maintainers read reports and open a narrowed issue for
anything that needs code, which is a good place to ask for help if you want to fix it yourself.

Crashes that look like a security problem go to [SECURITY.md](SECURITY.md), privately, not here.
Conduct problems go to [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
