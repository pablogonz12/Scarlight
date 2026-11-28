# Scarlight as a Steam compatibility tool

Scarlight ships as one thing: a custom compatibility tool for Steam on Linux. Install it once,
restart Steam, and pick **Scarlight <version>** under the title's *Properties → Compatibility*.
Play then runs the title through Scarlight on top of a Proton you already have.

```
Steam  ──▶  Scarlight (compatibility tool)  ──▶  victory.exe (the title's console APIs, GPU
                                                  command streams, RDNA2 shaders)
                                                 │
                                                 ├─ Wine .......... from your installed Proton
                                                 └─ D3D12 ──▶ vkd3d-proton ──▶ Vulkan ──▶ Mesa (RADV/ANV)
```

We do not ship or fork Proton. The tool uses the Wine in an installed GE-Proton or Valve Proton
for the operating-system side, and its own Windows DLLs for the GPU side.

Steam runs the tool inside the same kind of container it runs Proton in: `toolmanifest.vdf` asks for
the Steam Linux Runtime GE-Proton declares (`require_tool_appid`, `use_sessions`), so the tool sees
`compatibilitytools.d` — including the Proton folder it names — exactly as Proton does, on any
distribution. Distros whose container setup is unusual can drop those two lines and the tool runs on
the host instead; nothing else changes.

## 1. Install

### Option A: from a release tarball (no build)

1. Download `Scarlight-<version>.tar.gz` from the [releases page](https://github.com/pablogonz12/Scarlight/releases).
2. Extract the `Scarlight/` folder into Steam's compatibility tools folder:

   ```bash
   mkdir -p ~/.local/share/Steam/compatibilitytools.d/
   tar -xzf Scarlight-<version>.tar.gz -C ~/.local/share/Steam/compatibilitytools.d/
   ```

   Flatpak Steam: `~/.var/app/com.valvesoftware.Steam/data/Steam/compatibilitytools.d/`.
3. Create your settings file from the example if you have none yet, then set `PROTON` to a Proton you
   have installed (the folder name under `compatibilitytools.d/`, or a full path). That is the only
   setting you must touch:

   ```bash
   cd ~/.local/share/Steam/compatibilitytools.d/Scarlight-<version>
   [ -f scarlight.conf ] || cp ../Scarlight-<older version>/scarlight.conf .   # keep your settings, if you had a release
   [ -f scarlight.conf ] || cp scarlight.conf.example scarlight.conf
   ```
4. **If Scarlight is already installed**, each version has its own directory, so a new release starts
   from the example: copy your `scarlight.conf` across as in step 3. Inside one directory nothing is
   ever replaced: a release ships `scarlight.conf.example` and never a `scarlight.conf`, and the tool
   copies the example into place only when there is no conf yet, so your `PROTON=`, `D3DCOMPILER=`
   and other settings survive a re-extract of the same version.
   `tools/steam-compat/install.sh` behaves the same way.
5. Restart Steam.

A ProtonUp-Qt-style installer that extracts a tarball into `compatibilitytools.d/` produces the same
layout, so the release works there too.

### Option B: from a source checkout

```bash
tools/steam-compat/install.sh --dlls DIR        # see "The DLLs" below
```

`install.sh` builds nothing: it copies the tool, an already-built `victory.exe` and the DLLs into
Steam's `compatibilitytools.d/` (native Steam, or Flatpak Steam's if that is what you have),
detects your installed Proton - the GE-Proton named in `tools/steam-compat/BASE` first - and writes a
`scarlight.conf` for you. A source install goes to `compatibilitytools.d/Scarlight/` and Steam lists it
as **Scarlight (Xbox Series)**; a release tarball installs as `Scarlight-<version>/`, listed as
**Scarlight <version>**, so a release and a development build can sit side by side.

| Option | Meaning |
|---|---|
| `--exe PATH` | the built `victory.exe` (default `target/x86_64-pc-windows-msvc/release/victory.exe`) |
| `--dlls DIR` | folder with `d3d12.dll`, `d3d12core.dll`, `dxil.dll`, `dxcompiler.dll` (64-bit) |
| `--d3dcompiler PATH` | optional: Microsoft's `d3dcompiler_47.dll`, if you have one you are licensed to use |
| `--proton NAME\|PATH` | the Proton whose Wine to run on (default: newest GE-Proton in `compatibilitytools.d`) |
| `--dest DIR` | the `compatibilitytools.d` to install into (default: Steam's own) |
| `--uninstall` | remove the installed tool |

Build the exe first — [CONTRIBUTING.md](../CONTRIBUTING.md#build) has the commands — or use a release
tarball instead.

### The DLLs

vkd3d-proton's two DLLs are redistributable and are packed into a release tarball; DXC's and
Microsoft's are not, so a release leaves them to you (see "DXC, which a release does not carry" above).
From a source checkout, get all four yourself:

| File | Where it comes from |
|---|---|
| `d3d12.dll`, `d3d12core.dll` | [vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton) release or build, 64-bit |
| `dxil.dll`, `dxcompiler.dll` | [DirectXShaderCompiler](https://github.com/microsoft/DirectXShaderCompiler) release, `bin/x64` |
| `d3dcompiler_47.dll` | **optional.** Microsoft's, not redistributable. From software you are licensed to use |
| `dxgi.dll`, `d3d11.dll`, `d3d10core.dll` | copied out of your own Proton by `install.sh` |

`d3dcompiler_47.dll` is **not** required: without it Wine's built-in compiler is used and the title
still starts. If you have a licensed copy, `install.sh --d3dcompiler PATH` picks it up (it also looks
in `~/.cache/winetricks/d3dcompiler_47/` and in existing game prefixes), or point `D3DCOMPILER` in
`scarlight.conf` at it later.

### DXC, which a release does not carry

Scarlight loads `dxil.dll` and `dxcompiler.dll` (the shader compiler) at runtime. Microsoft's terms for
those binaries limit them to use on Windows, so a release does not pass them on. It is one step: get
them from [DirectXShaderCompiler's releases](https://github.com/microsoft/DirectXShaderCompiler/releases)
(the zip's `bin/x64` folder) and put both files in `compatibilitytools.d/Scarlight-<version>/files/`. Without
them a title that needs shaders compiled at runtime stops early, and `scarlight.log` says which file
it could not load.

## 2. Pick it in Steam

If the title is not in your library yet, add its executable first: *Games → Add a Non-Steam Game to My
Library → Browse…*, and pick it. Either way, then: *Library → right-click the title → Properties →
Compatibility →* tick **Force the use of a specific
Steam Play compatibility tool**, then choose **Scarlight <version>** (for example *Scarlight 11.14.1*). Press Play.

Each game gets its own Wine prefix and its own Mesa shader cache, both under Steam's
`steamapps/compatdata/<appid>/`, so titles do not share state. Untick the box to go back to your
previous tool.

## 3. `scarlight.conf`

One `KEY=value` per line, next to the tool (`compatibilitytools.d/Scarlight-<version>/scarlight.conf`). Edit it,
then press Play again; changes apply on the next launch. An empty value means "the default".

| Key | Default | What it does |
|---|---|---|
| `BUILD` | `default` | which build to run: `files/victory-<BUILD>.exe` |
| `PROTON` | empty: the newest paired `GE-Proton11-14-victory<N>` when the fast path can run (see docs/linux-proton.md), else the GE-Proton the release is built for (`GE-Proton11-14`, from the `BASE` file), else the newest `GE-Proton11-*` found | the Proton whose Wine to run on: a folder name in the same `compatibilitytools.d`, or a full path to a Proton folder |
| `FAST` | `auto` | `0` always runs the safe path, even on the paired Proton |
| `D3DCOMPILER` | empty | path to Microsoft's `d3dcompiler_47.dll` you are licensed to use; empty uses Wine's built-in compiler |
| `PROFILE_DIR` | `~/.config/victory/titles` | where the title profiles are read from (`~` is expanded) |
| `MESA_CACHE_MAX_SIZE` | `16G` | cap for the per-game Mesa shader cache |
| `VKD3D_CONFIG` | `ignore_shared_fence` | options handed to vkd3d-proton |
| `EXTRA_ENV` | empty | extra environment for the run, space separated; keep it quoted, e.g. `EXTRA_ENV="VICTORY_LOG=debug RUST_LOG=info"` (the conf is read as shell) |

Environment variables set on the Steam launch (`VAR=1 %command%` in Launch Options) also reach the
tool and win over the conf for `VICTORY_LOG`, `WINEDEBUG` and `SCARLIGHT_DRY_RUN`.

## 4. Logs

Everything goes to one file, in the game's own prefix:

```
$STEAM_COMPAT_DATA_PATH/scarlight.log      # e.g. ~/.local/share/Steam/steamapps/compatdata/<appid>/scarlight.log
```

The tool's own lines (verb, build, Proton, game, exit code) and the whole run's output are appended
there; the Mesa shader cache sits beside it in `mesa_cache/`. For a bug report, the last ~50 lines of
that file plus `vulkaninfo --summary` are what we need.

`SCARLIGHT_DRY_RUN=1` runs the tool without `--execute`: it resolves and binds the title's imports and
stops, without touching the GPU. That is the quickest way to check that a prefix, a Proton and the
DLLs are wired up:

```bash
SCARLIGHT_DRY_RUN=1 %command%        # in the game's Launch Options, after clearing them again afterwards
```

## 5. Uninstall

```bash
tools/steam-compat/install.sh --uninstall      # a source install (Scarlight/)
rm -rf ~/.local/share/Steam/compatibilitytools.d/Scarlight-<version>   # a release
```

Nothing else is needed: Scarlight keeps no service, no system package and no file outside the tool
folder and the game's `compatdata/<appid>/`.

That folder is the game's Wine prefix, and **a game's saved games can live inside it**: back up
whatever saves matter to you, or check that Steam Cloud has them, before deleting it. With that done,
delete `compatdata/<appid>/` to reclaim the prefix and the shader cache. In Steam, the game's
Compatibility setting falls back to its previous tool after a restart.

## 6. Troubleshooting

| Symptom | What to do |
|---|---|
| Scarlight is not in the Compatibility list | the folder must be `compatibilitytools.d/Scarlight-<version>/` with `compatibilitytool.vdf` directly inside it, and Steam needs a restart |
| `notify-send: Proton not found` / log says "no Wine at …" | `PROTON` names a folder your Steam does not have, or a path without `files/bin/wine` |
| Nothing happens, no log | Steam never got as far as starting the tool: check the game is set to *Force the use of a specific Steam Play compatibility tool* |
| The log says `game exe not found` | the title was launched with an argument the tool cannot resolve; clear Launch Options and try again |
| Crash or black window at world entry | check `vulkaninfo --summary` shows a Mesa driver (`radv`, `Intel … Mesa`, `nvk`) with Vulkan 1.3+; a proprietary driver is not a supported target |
| Dark picture, missing detail, single-digit fps | expected today: see the status in the [README](../README.md#status) |
| A run left a stuck prefix | the tool stops a leftover wineserver itself at the start of `waitforexitandrun`; if the game still hangs, stop that game's server only: `WINEPREFIX=<the game's compatdata/<appid>/pfx> <your Proton>/files/bin/wineserver -k`. As a last resort delete `compatdata/<appid>/pfx` — after backing up any saved games that live in it |
| Title builds its shaders again every launch | the Mesa cache is per game under `compatdata/<appid>/mesa_cache`; delete it if it is stale, and keep `MESA_CACHE_MAX_SIZE` large enough |
| Flatpak Steam | all paths move to `~/.var/app/com.valvesoftware.Steam/data/Steam/`; `install.sh` detects that folder by itself |
| The tool starts in a container and your Proton is not found | Scarlight runs inside a Steam Linux Runtime container, like Proton, so `compatibilitytools.d` is exposed there and a `PROTON` name resolves against it (and against the usual Steam folders, Flatpak included). If a distro's container still hides it, set `PROTON` to a full path in `scarlight.conf` |
| The runtime container breaks the launch on your distro | the two lines that ask for it are `require_tool_appid` and `use_sessions` in `Scarlight/toolmanifest.vdf`; delete them and the tool runs on the host, on the same Wine |
