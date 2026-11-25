#!/usr/bin/env bash
# install.sh: install Scarlight as a Steam compatibility tool.
# Usage: tools/steam-compat/install.sh [options]
#   --exe PATH         the Scarlight build (default: target/x86_64-pc-windows-msvc/release/victory.exe)
#   --dlls DIR         folder with d3d12.dll + d3d12core.dll (vkd3d-proton) and dxil.dll + dxcompiler.dll (DXC)
#   --d3dcompiler PATH Microsoft's d3dcompiler_47.dll, if you have one you are licensed to use (optional)
#   --proton NAME|PATH the Proton whose Wine Scarlight runs on (default: newest GE-Proton in compatibilitytools.d)
#   --dest DIR         Steam's compatibilitytools.d (default: Steam's own, or Flatpak Steam's if that is what you have)
#   --uninstall        remove the installed tool from --dest
#   -h, --help         this text
# Then restart Steam, open the game's Properties > Compatibility, tick "Force the use of a specific Steam Play
# compatibility tool" and pick "Scarlight (Xbox Series)". The log is
# $STEAM_COMPAT_DATA_PATH/scarlight.log (normally ~/.local/share/Steam/steamapps/compatdata/<id>/scarlight.log).
#
# Microsoft's d3dcompiler_47.dll is never shipped: it is not redistributable.
# Scarlight works without it (Wine's built-in compiler is used); pass
# --d3dcompiler, or install it in the game's prefix with winetricks, to use it.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

EXE="$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe"
DLLS=""
D3DC=""
PROTON=""
DEST=""
UNINSTALL=0

usage() { sed -n '2,16p' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --exe) EXE="$2"; shift ;;
    --dlls) DLLS="$2"; shift ;;
    --d3dcompiler) D3DC="$2"; shift ;;
    --proton) PROTON="$2"; shift ;;
    --dest) DEST="$2"; shift ;;
    --uninstall) UNINSTALL=1 ;;
    -h | --help) usage; exit 0 ;;
    *)
      echo "unknown option $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# Native Steam first, then the Flatpak build (its data lives under ~/.var/app).
if [ -z "$DEST" ]; then
  for d in "$HOME/.local/share/Steam" "$HOME/.steam/root" \
    "$HOME/.var/app/com.valvesoftware.Steam/data/Steam"; do
    if [ -d "$d" ]; then DEST="$d/compatibilitytools.d"; break; fi
  done
fi
[ -n "$DEST" ] || {
  echo "Steam not found; pass --dest (the compatibilitytools.d folder to install into)" >&2
  exit 1
}
T="$DEST/Scarlight"

if [ "$UNINSTALL" = 1 ]; then
  if [ -d "$T" ]; then
    rm -rf -- "$T"
    echo "Removed $T."
  else
    echo "Nothing to remove at $T."
  fi
  echo "Your game's prefix (steamapps/compatdata/<id>/), its Mesa shader cache and the"
  echo "scarlight.log in it were left alone."
  echo "A game's saved games can live in that prefix: back them up (or check Steam Cloud)"
  echo "before you delete compatdata/<id>/ to reclaim the space."
  echo "In Steam, the game's Compatibility setting falls back to its previous tool once Steam restarts."
  exit 0
fi

mkdir -p "$DEST"
if [ -n "$PROTON" ] && [ "${PROTON#/}" != "$PROTON" ]; then
  [ -x "$PROTON/files/bin/wine" ] || { echo "no Wine at $PROTON/files/bin/wine" >&2; exit 1; }
else
  if [ -z "$PROTON" ]; then
    # The GE-Proton this checkout is built for (the BASE file next to this
    # script), then the newest Proton installed.
    BASE="$(sed -n '1{s/[[:space:]]*$//;p;}' "$HERE/BASE" 2>/dev/null || true)"
    if [ -n "$BASE" ]; then
      for name in "$BASE-x86_64" "$BASE"; do
        if [ -x "$DEST/$name/files/bin/wine" ]; then
          PROTON="$name"
          break
        fi
      done
    fi
  fi
  if [ -z "$PROTON" ]; then
    PROTON="$(cd "$DEST" && { ls -d GE-Proton* Proton* 2>/dev/null || true; } | sort -V | tail -1)"
  fi
  [ -n "$PROTON" ] && [ -x "$DEST/$PROTON/files/bin/wine" ] || {
    echo "no Proton with Wine in $DEST; install GE-Proton or pass --proton" >&2
    exit 1
  }
fi
[ -f "$EXE" ] || {
  echo "Scarlight build not found: $EXE" >&2
  echo "(build it with: cargo xwin build --release -p victory --target x86_64-pc-windows-msvc)" >&2
  exit 1
}
# The folder the Wine and the DXVK DLLs are copied from (a name is relative to
# this compatibilitytools.d, a full path is used as it is).
if [ "${PROTON#/}" != "$PROTON" ]; then PDIR="$PROTON"; else PDIR="$DEST/$PROTON"; fi

[ -n "$DLLS" ] || {
  echo "pass --dlls DIR: the folder holding d3d12.dll, d3d12core.dll (vkd3d-proton) and dxil.dll, dxcompiler.dll (DXC)" >&2
  echo "(docs/steam.md says where each one comes from; they are redistributable, d3dcompiler_47.dll is not)" >&2
  exit 1
}
for f in d3d12.dll d3d12core.dll dxil.dll dxcompiler.dll; do
  [ -f "$DLLS/$f" ] || { echo "missing $DLLS/$f" >&2; exit 1; }
done

# d3dcompiler_47.dll is optional and never shipped. Take it from --d3dcompiler,
# else from a copy already in the prefix (winetricks' cache, or a game prefix
# that has one); otherwise Scarlight runs on Wine's built-in compiler.
if [ -z "$D3DC" ]; then
  for c in "$HOME/.cache/winetricks/d3dcompiler_47/d3dcompiler_47.dll" \
    "$HOME"/.local/share/Steam/steamapps/compatdata/*/pfx/drive_c/windows/system32/d3dcompiler_47.dll \
    "$HOME"/.var/app/com.valvesoftware.Steam/data/Steam/steamapps/compatdata/*/pfx/drive_c/windows/system32/d3dcompiler_47.dll; do
    if [ -f "$c" ]; then D3DC="$c"; break; fi
  done
fi
if [ -n "$D3DC" ] && [ ! -f "$D3DC" ]; then
  echo "--d3dcompiler: no file at $D3DC" >&2
  exit 1
fi

mkdir -p "$T/files"
install -m 755 "$HERE/scarlight" "$T/scarlight"
# The GE-Proton this checkout is built for: the entry script reads it from here.
install -m 644 "$HERE/BASE" "$T/BASE"
install -m 644 "$EXE" "$T/files/victory-default.exe"
for f in d3d12.dll d3d12core.dll dxil.dll dxcompiler.dll; do
  install -m 644 "$DLLS/$f" "$T/files/"
done
if [ -n "$D3DC" ]; then install -m 644 "$D3DC" "$T/files/d3dcompiler_47.dll"; fi
# dxgi/d3d11/d3d10core come from the Proton you already have: copy, do not ship.
for f in dxgi.dll d3d11.dll d3d10core.dll; do
  src="$PDIR/files/lib/wine/dxvk/x86_64-windows/$f"
  if [ -f "$src" ]; then install -m 644 "$src" "$T/files/"; fi
done

cat >"$T/compatibilitytool.vdf" <<'VDF'
"compatibilitytools"
{
  "compat_tools"
  {
    "Scarlight"
    {
      "install_path" "."
      "display_name" "Scarlight (Xbox Series)"
      "from_oslist" "windows"
      "to_oslist" "linux"
    }
  }
}
VDF
# Steam runs the tool inside a Steam Linux Runtime container, the way it runs
# Proton, so compatibilitytools.d and the concat'ed Proton folder are visible
# there. 4183110 is Steam Linux Runtime 4.0, the one GE-Proton11-14 declares in
# its own toolmanifest; use_sessions is the matching session API. See
# docs/steam.md if a container breaks on your distro: the two lines can go.
cat >"$T/toolmanifest.vdf" <<'VDF'
"manifest"
{
  "version" "2"
  "commandline" "/scarlight %verb%"
  "require_tool_appid" "4183110"
  "use_sessions" "1"
}
VDF

if [ ! -f "$T/scarlight.conf" ]; then
  cat >"$T/scarlight.conf" <<CONF
# Scarlight compatibility tool settings. Edit, then press Play again.
# Full reference: docs/steam.md in the Scarlight repository.

# Which build to run: files/victory-<BUILD>.exe (default: the shipped one).
BUILD=default

# The Proton whose Wine Scarlight runs on: a folder name in this
# compatibilitytools.d, or a full path to a Proton folder (shell-quoted here,
# because it is sourced as shell).
PROTON=$(printf '%q' "$PROTON")

# Microsoft's d3dcompiler_47.dll is optional and not shipped. If you have one
# you are licensed to use, put its path here; empty means use Wine's built-in.
D3DCOMPILER=

# Where the title profiles are read from (default: ~/.config/victory/titles).
PROFILE_DIR=

# Mesa shader cache for this game (in the game's prefix) and its size cap.
MESA_CACHE_MAX_SIZE=16G

# vkd3d-proton options.
VKD3D_CONFIG=ignore_shared_fence

# Extra environment for the run, space separated. Keep the quotes: the value is
# read as one string. Example: EXTRA_ENV="VICTORY_LOG=debug RUST_LOG=info"
EXTRA_ENV=
CONF
  # The release layout: an upgrade never replaces an existing scarlight.conf.
  cp "$T/scarlight.conf" "$T/scarlight.conf.example"
fi

echo "Installed to $T (on $PROTON${D3DC:+; d3dcompiler_47 from $D3DC})."
echo "Restart Steam, then pick \"Scarlight (Xbox Series)\" in the game's Properties > Compatibility."
echo "Log: <game's prefix>/scarlight.log (STEAM_COMPAT_DATA_PATH/scarlight.log)."
