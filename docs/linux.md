# Linux setup (Mesa)

Tested on one machine: CachyOS (Arch family), RX 9070 XT, Mesa 26.3.1 RADV, GE-Proton11-14. Other
Mesa GPUs (other RADV generations, ANV) are untested. Read the [status](../README.md#status) first:
this is not playable.

## 1. Packages

```bash
sudo pacman -S --needed mesa vulkan-radeon vulkan-tools rustup git clang lld llvm   # vulkan-intel on Intel
```

Install **GE-Proton11-14** from [proton-ge-custom](https://github.com/GloriousEggroll/proton-ge-custom/releases),
for example into `~/.local/share/Steam/compatibilitytools.d/`.

## 2. Build

Follow [Quick start](../README.md#quick-start).

## 3. Files next to `victory.exe`

In `target/x86_64-pc-windows-msvc/release/`:

| File | Source |
|---|---|
| `dxgi.dll` | GE-Proton's DXVK copy, `files/lib/wine/dxvk/x86_64-windows/` |
| `d3d12.dll`, `d3d12core.dll` | [vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton) master, 64-bit. Without them, GE-Proton11-14's own are used |
| `dxcompiler.dll`, `dxil.dll` | DirectXShaderCompiler release, x64 (needed by the quick-start tag) |
| `d3dcompiler_47.dll` | Microsoft's, from software you are licensed to use (stop-gap, because Wine's built-in one rejects some runtime shaders) |

Never commit any of these, or any game file.

## 4. Play script

Save as `play.sh` **outside the repository** and edit the paths. Your game files must also be
outside the repository.

```bash
#!/usr/bin/env bash
set -eu
PROTON="$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-14-x86_64"   # your folder name; usually ends in -x86_64
EXE="$HOME/src/Scarlight/target/x86_64-pc-windows-msvc/release/victory.exe"
GAME_DIR="$HOME/victory-data/game"          # your own legally obtained files
GAME_EXE="<the title's exe>"

export WINEPREFIX="$HOME/.local/share/scarlight-prefix"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core,d3dcompiler_47=n,b"
export VKD3D_CONFIG=ignore_shared_fence
export VICTORY_HOOKS=1   # place the patches your build's profile lists; see title-profiles.md
export VICTORY_GAME_DIR="$GAME_DIR"
export VICTORY_LOG="${VICTORY_LOG:-info}"

exec "$PROTON/files/bin/wine" "$EXE" run "$GAME_DIR/$GAME_EXE" --execute
```

Make it executable once with `chmod +x play.sh`. Pass switches in front: `VICTORY_GPU_TIMING=1 ./play.sh`;
they are listed under [switches](#switches). If you use GE-Proton's own D3D12 DLLs, make sure your
prefix has them installed or copy them next to the exe.

## 5. Title profiles (optional)

A **title profile** tells the layer what *one build of one title* needs beyond the generic path: a
code patch to place, shaders to leave out, the name it prints. Nothing of this kind is built into
the code and none ships with the repository (see `docs/asset-policy.md`), so **without a profile the
layer runs with no title patches at all** — that is the normal state, and every generic path works.

A profile is a TOML file you write, selected at launch by the BLAKE3 of the executable. It is read
from the first of these that exists:

| Order | Directory | Notes |
|---|---|---|
| 1 | `VICTORY_PROFILE_DIR` | any directory; under Wine give a Windows path, e.g. `Z:\home\you\.config\victory\titles` |
| 2 | `%APPDATA%\victory\titles` | inside the prefix, e.g. `~/.local/share/scarlight-prefix/drive_c/users/<you>/AppData/Roaming/victory/titles` |
| 3 | `$XDG_CONFIG_HOME/victory/titles`, else `~/.config/victory/titles` | reached through Wine's `Z:` drive; some Proton wrappers drop `XDG_CONFIG_HOME`, so set `VICTORY_PROFILE_DIR` if it is not found |

Every `*.toml` in that directory is read; a file that does not parse is logged and skipped. Hooks
need `VICTORY_HOOKS=1` as well (patches are never automatic). The launch log says which file won:

```
title profile: Z:\home\you\.config\victory\titles\<file>.toml matched, 2 hooks
title profile: 2 hooks applied
```

or, with nothing found (the normal case):

```
title profile: none matched
```

The launch log names no image hash, because logs get pasted into reports and hashes of game files
do not belong there. To find the hash for a profile, run `victory title-id <exe>` locally; it also
says whether a profile already matches.

A profile to copy, with every value made up — replace it with your own build's:

```toml
name  = "my build"                       # what the launch line prints
build = ["0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"]
                                         # BLAKE3 of the executable; the layer prints it as
                                         # "build unknown (<hash>)" when no profile matches

[[hooks]]                                # optional: a code patch; skipped, with a log line, if
                                         # new is another length or rva+len leaves the image
what   = "example patch"                 # your words, logged
rva    = 0x12345                         # offset from the start of the mapped image
expect = "488b0500000000"                # bytes that must be there first (hex, spaces allowed)
new    = "eb189090909090"                # what to write instead, same length as expect

[skip]                                   # optional: work the GPU layer leaves out
shaders = ["0011223344556677"]           # 16-digit host shader hashes
stages  = ["mesh"]                       # or whole stages written for 64-lane waves
```

`VICTORY_NO_WATCHDOG=1`, the name `VICTORY_HOOKS` had before profiles existed, still turns the
hooks on, so older launch scripts and queue lines keep working.

## 6. Controls

A gamepad works. Otherwise the keyboard acts as a pad while the title's window has focus:

| Keys | Pad |
|---|---|
| Enter, Backspace, X, Y | A, B, X, Y |
| Esc, Tab | Menu, View |
| Arrows | D-pad |
| W A S D / I J K L | left / right stick |
| Q / E, Z / C, F / H | shoulders, triggers, stick clicks |

## Troubleshooting

| Problem | Try |
|---|---|
| Crash at world entry | Replace stock vkd3d-proton 3.1 DLLs with master's or GE-Proton11-14's |
| Very slow, swapping | Close apps that hold RAM or GPU memory (local LLM servers, browsers) |
| The title quits while waiting for the GPU | `VICTORY_HOOKS=1` with a profile for that build (see [title profiles](title-profiles.md)); without one, nothing is patched |
| Freezes | Unset all experimental switches |
| `victory` reports no paths | `victory config`; paths must be outside the repository |
| RenderDoc installed and the title crashes | `VK_LOADER_LAYERS_DISABLE='*renderdoc*'` |

Crashes and GPU failures are appended to `crashes.log` in the data folder.

For more diagnostics, build on Linux with:

```bash
cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc
```

Then point `EXE` in `play.sh` at `target/x86_64-pc-windows-msvc/diag/victory.exe`, with the same
[files next to it](#3-files-next-to-victoryexe).

## Switches

| Switch | Effect |
|---|---|
| `VICTORY_HOOKS=1` | Places the code patches the profile for this build lists (see [title profiles](title-profiles.md)). Nothing is patched without a profile, and a patch whose bytes do not match is skipped. `VICTORY_NO_WATCHDOG=1`, the old name, still works |
| `VICTORY_ASYNC_WRITEBACK=1` | **Experimental**, off by default; has hung the title in our tests |
| `VICTORY_LOG=info` | Normal per-frame log; `VICTORY_LOG=<target>=debug` for debug output from one area |
| `VICTORY_LOG_FILE=<path>` | Appends the log to that file instead of stderr. A title Steam starts has no terminal, so its stderr goes nowhere; with this, `play.sh` leaves the log where you can read it (`Z:\home\you\victory.log` is the Linux path as Wine sees it). Without it the log stays on stderr |
| `VICTORY_GPU_TIMING=1` | Diagnostics builds only: runs and waits for each command list on its own, and logs any list over 50 ms with its slowest commands. Serialising the submission changes what you measure; the per-frame copy statistics are logged without it |
| `VICTORY_SHOW_TARGET=<n>` / `busiest` | Diagnostics builds only: shows the nth texture the frame rendered to or wrote, in first-use order — the index a target dump uses — instead of the title's picture; `busiest` picks the displayable target the most draws wrote. A frame with fewer targets shows the title's own picture |

### Diagnostics switches

These need a build with `--features diagnostics` (see above). They are how a bug report gets the
detail to act on.

| Variable | Effect |
|---|---|
| `VICTORY_STACKS=20,60` | Prints every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | Logs a title `u32` when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | Presses keys at those seconds, focus or not |
| `VICTORY_CAPTURE=<dir>` | Saves presented frames (powers of two by default, or the list in `VICTORY_CAPTURE_FRAMES`) |
| `VICTORY_DUMP_TARGETS=1` | With capture on, also saves every render target of those frames, in first-use order |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | Saves title memory to the cache folder on a crash |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | Diagnostics build only. Omits draws and dispatches, indirect ones included, whose pipeline has a listed 16-digit shader hash; the profile's `skip.shaders` is used when this is unset |
| `VICTORY_SKIP_WAVE64=mesh,compute` | Diagnostics build only. Omits draws and dispatches whose pipeline has a 64-lane program in a listed stage (diagnostic on 32-lane hosts); the profile's `skip.stages` is used when this is unset |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | Stops translated shader loops after `n` iterations per lane instead of 2^20; a real loop longer than `n` is cut short too, so 32 is a diagnostic value and 256-1024 a safer probe |
| `VICTORY_D3D12_DEBUG=1` | Host D3D12 debug layer; `VICTORY_D3D12_GBV=1` adds GPU-based validation (slow) |
| `VICTORY_D3D12_DRED=1` | On a GPU failure, logs the allocation hit and the commands in flight |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | Records what the title plays, from our process only |

Crashes and GPU failures are also appended to `crashes.log` in the data folder. Attach the last ~50
lines of the log to a report; never attach game files, screenshots of game content, shader dumps or
hashes of game data ([asset policy](asset-policy.md)).
