# ScarLight launcher

One menu for a ScarLight install on Linux + Mesa: it looks at your machine first,
then starts the port under Wine/Proton. No game files are shipped, downloaded or
read: every path in it is one you gave it.

```bash
./scarlight            # the menu
./scarlight --check    # only the machine report (exit 1 if something is red)
./scarlight --dry-run  # print the setup summary and the exact command, start nothing
```

## Install

```bash
install -Dm755 scarlight ~/.local/bin/scarlight
install -Dm644 scarlight.desktop.in ~/.local/share/applications/scarlight.desktop
```

The desktop file expects `scarlight` on your `PATH`; if you keep it elsewhere, edit
the `Exec=` line to the full path. Nothing else is installed.

## What it asks for

| Step | What | Default |
|---|---|---|
| Proton | folder of a Proton/GE build (`files/bin/wine`) | found in the Steam `compatibilitytools.d` folders |
| Executable | a local `.exe` you have permission to run in this environment | none |
| DLL folder | where `dxgi`, `d3d12`, `d3d12core`, `dxil`, `dxcompiler` live (a folder other than the build's own is added to `WINEDLLPATH`, so Wine really searches it) | next to the ScarLight build |
| Prefix | `WINEPREFIX` | `${XDG_DATA_HOME:-~/.local/share}/scarlight/prefix` |

`docs/linux.md` in the repository explains where those DLLs come from.

## The screens

1. **System check** - GPU and Vulkan driver (RADV/ANV/NVK, Vulkan 1.3+), Mesa version,
   kernel, RAM, Proton/GE builds found, vkd3d-proton and DXC DLLs present, build and game
   exe set, and whether the build has a title profile. Every line is `ok`, `warn` or
   `FAIL`, and a `FAIL` carries the one command or step that fixes it. A missing title
   profile is a `warn`, not a failure: profiles are optional, the layer runs without one
   and applies no title patches ("No title profile for this build"). The check asks the
   build itself (`victory.exe title-id <exe>`) and reads the same folders the loader
   reads, so the line matches what Play will do; see `docs/title-profiles.md`.
2. **Main menu** - `Play` (the last setup), `Quick setup` (the three questions above),
   `Presets`, `Advanced`, `Logs`, `Help`.
3. **Presets** - `Play`, `Performance`, `Picture debug`, `Developer` (skips the intros,
   starts at the test scene).
4. **Advanced** - ray tracing (only when the build has it), backend (`d3d12` stable,
   `vulkan` native greyed out as "coming later"), Mesa options, log level, watchdog,
   where to start, Proton, build, game exe, DLL folder, prefix.
5. **Logs** - open the last launch log, or copy its last 50 lines for a bug report.
6. Before anything starts: a compact summary and the exact command line, then one
   question.

## Front ends

The same flow is drawn with whatever is installed, in this order: `gum` (nice TUI),
`fzf`, `whiptail`/`dialog`, plain numbered prompts. Force one with
`SCARLIGHT_UI=gum|fzf|whiptail|dialog|select`; a forced front end that is not installed
falls back with a warning.

## Mesa options are gated

Only switches the installed Mesa really has are offered: the launcher finds your Vulkan
ICD library (`/usr/share/vulkan/icd.d/*.json`), asks the library for the variable name,
and keeps the driver/version check (`RADV_*` only on RADV, Mesa 22+ for `RADV_PERFTEST`,
and so on). If the library cannot be read, the driver/version check alone decides. Each
option carries its one-line explanation, and every one is also written into the summary
before launch, so nothing is hidden.

## Flags

| Flag | Meaning |
|---|---|
| `--check` | machine report only; exit 1 when something is red |
| `--dry-run`, `--print` | print the check, the summary and the command; start nothing |
| `--profile NAME` | load `${XDG_CONFIG_HOME:-~/.config}/scarlight/NAME.env` |
| `--save-profile NAME` | write the current setup to that profile |
| `--list-profiles` | names of the profiles you have |
| `--ui NAME` | force the front end |
| `--yes` | skip the "start now?" question |
| `--help` | the header of the script |

## Files

```
${XDG_CONFIG_HOME:-~/.config}/scarlight/
  last.env            the last choices (sourced on start)
  default.env         written by Quick setup
  <name>.env          your profiles
${XDG_STATE_HOME:-~/.local/share}/scarlight/
  logs/<date>-<preset>.log   one log per launch
```

A profile is plain shell: `KEY='value'` lines, so you can keep them in git. Keys:
`PRESET TARGET RT MESA LOG WATCHDOG BACKEND PROTON WINE PORT_EXE GAME_EXE GAME_DIR
DLLDIR PREFIX`.

## Environment

| Variable | Meaning |
|---|---|
| `SCARLIGHT_UI` | force the front end |
| `SCARLIGHT_CONFIG_DIR` | where the profiles live (default `~/.config/scarlight`) |
| `SCARLIGHT_DEFAULT_PREFIX` | default `WINEPREFIX` |
| `SCARLIGHT_LAUNCH_CMD` | command template that starts the game instead of the built-in `wine` line; `{exe}`, `{port_exe}` and `{prefix}` are substituted (shell-quoted) and the profile is exported as `SCARLIGHT_*` |
| `SCARLIGHT_RT_SUPPORTED` | `1`/`0` to tell the launcher whether the build has the ray-tracing path (otherwise a `*-rt` build name or a `.rt` file next to it) |
| `SCARLIGHT_NO_COLOR` | plain text |

`SCARLIGHT_LAUNCH_CMD` is how a site keeps its own runner (a queue, a wrapper, a
benchmark harness) while still using this menu: the launcher does the checks, the
profiles and the summary, the hook does the start.

## Requirements

`bash` 4.4+, coreutils, `util-linux` (`fold`, `paste`), `vulkan-tools` for the check, and
`find`; optionally `gum`/`fzf`/`whiptail`/`dialog`, `wl-clipboard`/`xclip` for the
"copy the last 50 lines" action, and `less` for the log viewer. POSIX-friendly: only
`bash` builtins and coreutils, no Python, no hard-coded paths.

## License

GPL-3.0-or-later, the same as the repository.
