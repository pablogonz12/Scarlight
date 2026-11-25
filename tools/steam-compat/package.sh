#!/usr/bin/env bash
# package.sh: build the release tarball a ProtonUp-Qt-style installer can extract.
#
# Usage: tools/steam-compat/package.sh --vkd3d DIR [options]
#   --vkd3d DIR      folder with vkd3d-proton's d3d12.dll and d3d12core.dll (x64)
#   --include-dxc    also pack DXC's dxil.dll and dxcompiler.dll (needs --dxc DIR).
#                    Off by default: Microsoft's terms for those binaries limit them
#                    to use on Windows, so a Linux release does not pass them on; the
#                    user supplies their own copy (docs/steam.md).
#   --dxc DIR        folder with DXC's dxil.dll and dxcompiler.dll (x64)
#   --exe PATH       the built victory.exe
#                    (default: target/x86_64-pc-windows-msvc/release/victory.exe)
#   --build NAME     the build name the exe is installed as, files/victory-<NAME>.exe (default: default)
#   --version VER    the release version (default: <GE major>.<GE release>.<revision>, from
#                    BASE and the existing v<major>.<release>.* tags; see docs/RELEASES.md)
#   --vkd3d-version TAG   the vkd3d-proton version --vkd3d came from (recorded)
#   --vkd3d-source TEXT   where that version's corresponding source is (recorded)
#   --dxc-version TAG     the DXC release --dxc came from (recorded)
#   --out DIR        where to write the tarball (default: dist)
#   -h, --help       this text
#
# The result is dist/Scarlight-<version>.tar.gz whose single top folder is
# Scarlight-<version>/, holding compatibilitytool.vdf, toolmanifest.vdf, scarlight,
# scarlight.conf.example, VERSION, files/ and the licence texts of everything
# redistributed in it. Its display name is "Scarlight <version>" and its internal
# name is "Scarlight-<version>", so two releases install side by side, as two
# GE-Proton builds do.
#
# The version's major.release comes from tools/steam-compat/BASE (the GE-Proton
# this release runs on), and --version may not name a different one.
#
# What it packs is what may be redistributed: Scarlight itself (GPL-3.0) and
# vkd3d-proton (LGPL-2.1). Microsoft's d3dcompiler_47.dll and, by default, DXC's
# two DLLs are not packed - the entry script runs without either, and the user
# drops in their own copy where a title needs one. dxgi/d3d11/d3d10core come from
# the user's own Proton, and Wine comes from their Proton too.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
VKD3D=""
DXC=""
DXC_VERSION=""
INCLUDE_DXC=0
EXE="$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe"
BUILDNAME="default"
VERSION=""
VKD3D_VERSION=""
VKD3D_SOURCE=""
OUT="$ROOT/dist"

usage() { sed -n '2,36p' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --vkd3d) VKD3D="$2"; shift ;;
    --dxc) DXC="$2"; shift ;;
    --include-dxc) INCLUDE_DXC=1 ;;
    --exe) EXE="$2"; shift ;;
    --build) BUILDNAME="$2"; shift ;;
    --version) VERSION="$2"; shift ;;
    --vkd3d-version) VKD3D_VERSION="$2"; shift ;;
    --vkd3d-source) VKD3D_SOURCE="$2"; shift ;;
    --dxc-version) DXC_VERSION="$2"; shift ;;
    --out) OUT="$2"; shift ;;
    -h | --help) usage; exit 0 ;;
    *)
      echo "unknown option $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

case "$BUILDNAME" in
  '' | *[!A-Za-z0-9._-]*)
    echo "--build must be letters, digits, dot, dash or underscore: got '$BUILDNAME'" >&2
    exit 2
    ;;
esac
[ -n "$VKD3D" ] || { echo "pass --vkd3d DIR (d3d12.dll, d3d12core.dll)" >&2; exit 1; }
if [ "$INCLUDE_DXC" = 1 ]; then
  [ -n "$DXC" ] || { echo "--include-dxc needs --dxc DIR (dxil.dll, dxcompiler.dll)" >&2; exit 1; }
fi
[ -f "$EXE" ] || {
  echo "Scarlight build not found: $EXE" >&2
  echo "(cargo xwin build --release -p victory --target x86_64-pc-windows-msvc)" >&2
  exit 1
}
for f in d3d12.dll d3d12core.dll; do
  [ -f "$VKD3D/$f" ] || { echo "missing $VKD3D/$f" >&2; exit 1; }
done
if [ "$INCLUDE_DXC" = 1 ]; then
  for f in dxil.dll dxcompiler.dll; do
    [ -f "$DXC/$f" ] || { echo "missing $DXC/$f" >&2; exit 1; }
  done
fi

# The version is <GE major>.<GE release>.<revision>: Scarlight 11.14.1 runs on
# GE-Proton11-14, and the revision counts within that GE release, so rebasing on
# GE-Proton11-15 starts again at 11.15.1. BASE is the one place the GE release is
# named; docs/RELEASES.md explains the scheme.
BASE_FILE="$HERE/BASE"
[ -f "$BASE_FILE" ] || {
  echo "missing $BASE_FILE (the GE-Proton this release runs on, e.g. GE-Proton11-14)" >&2
  exit 1
}
BASE="$(sed -n '1{s/[[:space:]]*$//;p;}' "$BASE_FILE")"
case "$BASE" in
  GE-Proton[0-9]*-[0-9]*) ;;
  *)
    echo "$BASE_FILE must hold one line, GE-Proton<major>-<release>; got '$BASE'" >&2
    exit 2
    ;;
esac
BASE_SERIES="${BASE#GE-Proton}"
BASE_SERIES="${BASE_SERIES/-/.}" # 11-14 -> 11.14

if [ -z "$VERSION" ]; then
  # The highest revision this GE release has tagged, plus one - not how many
  # tags there are: with v11.14.1 and v11.14.3 the next is 11.14.4, never an
  # existing number again. No tag yet means .1, and a rebase on a newer
  # GE-Proton starts at .1 by itself.
  REV=1
  if git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    SERIES_RE="$(printf '%s' "$BASE_SERIES" | sed 's/\./\\./g')"
    LAST="$(git -C "$ROOT" tag --list "v$BASE_SERIES.*" |
      sed -n "s/^v$SERIES_RE\.\([0-9][0-9]*\)$/\1/p" | sort -n | tail -1)"
    if [ -n "$LAST" ]; then REV=$((10#$LAST + 1)); fi  # 10#: a revision like 08 is decimal, not octal
  fi
  VERSION="$BASE_SERIES.$REV"
fi
VERSION="${VERSION#v}"
case "$VERSION" in
  "$BASE_SERIES".[0-9]*) ;;
  *)
    echo "--version $VERSION does not belong to $BASE: this release's versions are $BASE_SERIES.<revision>" >&2
    exit 2
    ;;
esac
case "$VERSION" in
  '' | .* | *. | *[!0-9.]* | *..*)
    echo "--version must be <major>.<release>.<revision>; got '$VERSION'" >&2
    exit 2
    ;;
esac
if [ "$(printf '%s' "$VERSION" | awk -F. '{print NF}')" != 3 ]; then
  echo "--version must be three numbers, <major>.<release>.<revision>; got '$VERSION'" >&2
  exit 2
fi
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"

WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
STAGE="$WORK/Scarlight-$VERSION"
mkdir -p "$STAGE/files/licenses"

install -m 755 "$HERE/scarlight" "$STAGE/scarlight"
install -m 644 "$BASE_FILE" "$STAGE/BASE"
cat >"$STAGE/compatibilitytool.vdf" <<VDF
"compatibilitytools"
{
  "compat_tools"
  {
    "Scarlight-$VERSION"
    {
      "install_path" "."
      "display_name" "Scarlight $VERSION"
      "from_oslist" "windows"
      "to_oslist" "linux"
    }
  }
}
VDF
cat >"$STAGE/toolmanifest.vdf" <<'VDF'
"manifest"
{
  "version" "2"
  "commandline" "/scarlight %verb%"
  "require_tool_appid" "4183110"
  "use_sessions" "1"
}
VDF
# What a bug report needs: which build, from which commit, on which GE-Proton.
cat >"$STAGE/VERSION" <<VERS
version=$VERSION
commit=$COMMIT
base=$BASE
vkd3d-proton=${VKD3D_VERSION:-unknown}
dxc=$( [ "$INCLUDE_DXC" = 1 ] && echo "${DXC_VERSION:-unknown}" || echo none )
VERS

install -m 644 "$EXE" "$STAGE/files/victory-$BUILDNAME.exe"
install -m 644 "$VKD3D/d3d12.dll" "$VKD3D/d3d12core.dll" "$STAGE/files/"
if [ "$INCLUDE_DXC" = 1 ]; then
  install -m 644 "$DXC/dxil.dll" "$DXC/dxcompiler.dll" "$STAGE/files/"
fi

# The conf a new install starts from: every option documented, Proton left for
# the user (or the installer) to fill in. Shipped as scarlight.conf.example: the
# tool copies it to scarlight.conf on first launch only, so an upgrade of an
# installed tool never replaces the settings someone tuned.
cat >"$STAGE/scarlight.conf.example" <<CONF
# Scarlight compatibility tool settings. Edit, then press Play again.
# Full reference: docs/steam.md in the Scarlight repository.

# Which build to run: files/victory-<BUILD>.exe (the one this package shipped).
BUILD=__BUILD__

# The Proton whose Wine Scarlight runs on: a folder name in this
# compatibilitytools.d, or a full path to a Proton folder. Empty means the
# GE-Proton this release is built for ($BASE, in the BASE file next to this
# one); failing that, the newest GE-Proton11-* installed, with a warning.
PROTON=

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
# The packaged build name, so BUILD and files/victory-<BUILD>.exe always agree.
sed -i "s/__BUILD__/$BUILDNAME/" "$STAGE/scarlight.conf.example"

cp "$ROOT/LICENSE" "$STAGE/LICENSE"
cp "$HERE/licenses/LGPL-2.1.txt" "$HERE/licenses/vkd3d-proton-LGPL-2.1.txt" \
  "$STAGE/files/licenses/"
if [ "$INCLUDE_DXC" = 1 ]; then
  cp "$HERE/licenses/DXC-LICENSE-MS.txt" "$HERE/licenses/DXC-LICENCE-MIT.txt" \
    "$HERE/licenses/DXC-LICENSE-LLVM.txt" "$HERE/licenses/DXC-CLANG-LICENSE-Apache-2.0-LLVM.txt" \
    "$STAGE/files/licenses/"
fi
install -m 644 "$HERE/licenses/README.md" "$STAGE/files/licenses/README.md"

if [ "$INCLUDE_DXC" = 1 ]; then
  DXC_NOTE="DXC ${DXC_VERSION:-unknown}
  files/dxil.dll, files/dxcompiler.dll, unmodified release binaries from
  https://github.com/microsoft/DirectXShaderCompiler/releases/tag/${DXC_VERSION:-unknown},
  with their notices (DXC-LICENSE-MS.txt, DXC-LICENCE-MIT.txt, DXC-LICENSE-LLVM.txt,
  DXC-CLANG-LICENSE-Apache-2.0-LLVM.txt) in this folder. Not packed by a release:
  the packager asked for them."
else
  DXC_NOTE="DXC: not included.
  Scarlight loads dxil.dll and dxcompiler.dll at runtime when a title needs them.
  Microsoft's terms for those binaries limit them to use on Windows, so a Linux
  release does not pass them on: take them from
  https://github.com/microsoft/DirectXShaderCompiler/releases and put both files in
  Scarlight/files/ (see docs/steam.md). It is the same one-step arrangement as
  d3dcompiler_47.dll below."
fi

# Which binaries these are, where they came from, and where their source is: the
# LGPL asks for the corresponding source and this is the written offer that goes
# with it (the release job also attaches the source archive).
cat >"$STAGE/files/licenses/VERSIONS.txt" <<VERS
Scarlight $VERSION
  built from https://github.com/pablogonz12/Scarlight, GPL-3.0-or-later (see LICENSE).

vkd3d-proton ${VKD3D_VERSION:-unknown}
  files/d3d12.dll, files/d3d12core.dll: LGPL-2.1-or-later, built from that tree and
  nothing else. Corresponding source: ${VKD3D_SOURCE:-https://github.com/HansKristian-Work/vkd3d-proton/tree/${VKD3D_VERSION:-unknown}},
  cloned with its submodules (dxil-spirv, Vulkan-Headers, SPIRV-Headers), which the
  release that carries these DLLs also attaches as a source archive. If it is missing,
  write to the project's issue tracker and we will send it; the offer is good for
  three years after the release date.

${DXC_NOTE}

Microsoft d3dcompiler_47.dll is not included and not required.
VERS



mkdir -p "$OUT"
TARBALL="$OUT/Scarlight-$VERSION.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner \
  --mtime="@$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)" \
  -C "$WORK" -czf "$TARBALL" "Scarlight-$VERSION"
sha256sum "$TARBALL" > "$TARBALL.sha256"

echo "$TARBALL"
echo "  Scarlight-$VERSION/: compatibilitytool.vdf, toolmanifest.vdf, scarlight, VERSION,"
echo "                      scarlight.conf.example, BASE, LICENSE, files/"
echo "  files/: victory-$BUILDNAME.exe, d3d12.dll, d3d12core.dll, licenses/"
echo "Steam will list it as \"Scarlight $VERSION\" (runs on $BASE)."
echo "Extract it into Steam's compatibilitytools.d and restart Steam."
