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
#   --version VER    version in the file name (default: latest git tag, else 0.0.0)
#   --vkd3d-version TAG   the vkd3d-proton release --vkd3d came from (recorded, with its source URL)
#   --dxc-version TAG     the DXC release --dxc came from (recorded)
#   --out DIR        where to write the tarball (default: dist)
#   -h, --help       this text
#
# The result is dist/Scarlight-<version>.tar.gz whose single top folder is
# Scarlight/, holding compatibilitytool.vdf, toolmanifest.vdf, scarlight,
# scarlight.conf.example, files/ and the licence texts of everything redistributed
# in it.
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
OUT="$ROOT/dist"

usage() { sed -n '2,28p' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --vkd3d) VKD3D="$2"; shift ;;
    --dxc) DXC="$2"; shift ;;
    --include-dxc) INCLUDE_DXC=1 ;;
    --exe) EXE="$2"; shift ;;
    --build) BUILDNAME="$2"; shift ;;
    --version) VERSION="$2"; shift ;;
    --vkd3d-version) VKD3D_VERSION="$2"; shift ;;
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

if [ -z "$VERSION" ]; then
  # Release tags only: an unreleased checkout packs as 0.0.0, not as some
  # other line's tag.
  # Only a tag on HEAD names a release; `--abbrev=0` would take an older one.
  VERSION="$(git -C "$ROOT" describe --tags --match 'v*' --exact-match 2>/dev/null || echo 0.0.0)"
fi
VERSION="${VERSION#v}"

WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
STAGE="$WORK/Scarlight"
mkdir -p "$STAGE/files/licenses"

install -m 755 "$HERE/scarlight" "$STAGE/scarlight"
cat >"$STAGE/compatibilitytool.vdf" <<'VDF'
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
cat >"$STAGE/toolmanifest.vdf" <<'VDF'
"manifest"
{
  "version" "2"
  "commandline" "/scarlight %verb%"
  "require_tool_appid" "4183110"
  "use_sessions" "1"
}
VDF
install -m 644 "$EXE" "$STAGE/files/victory-$BUILDNAME.exe"
install -m 644 "$VKD3D/d3d12.dll" "$VKD3D/d3d12core.dll" "$STAGE/files/"
if [ "$INCLUDE_DXC" = 1 ]; then
  install -m 644 "$DXC/dxil.dll" "$DXC/dxcompiler.dll" "$STAGE/files/"
fi

# The conf a new install starts from: every option documented, Proton left for
# the user (or the installer) to fill in. Shipped as scarlight.conf.example: the
# tool copies it to scarlight.conf on first launch only, so an upgrade of an
# installed tool never replaces the settings someone tuned.
cat >"$STAGE/scarlight.conf.example" <<'CONF'
# Scarlight compatibility tool settings. Edit, then press Play again.
# Full reference: docs/steam.md in the Scarlight repository.

# Which build to run: files/victory-<BUILD>.exe (the one this package shipped).
BUILD=__BUILD__

# The Proton whose Wine Scarlight runs on: a folder name in this
# compatibilitytools.d, or a full path to a Proton folder.
PROTON=GE-Proton11-14-x86_64

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
  files/d3d12.dll, files/d3d12core.dll, unmodified release binaries, LGPL-2.1-or-later.
  Corresponding source for that exact version: the tag's source tree, cloned with
  its submodules (dxil-spirv, Vulkan-Headers, SPIRV-Headers),
  https://github.com/HansKristian-Work/vkd3d-proton/tree/${VKD3D_VERSION:-unknown},
  which this release also attaches as vkd3d-proton-<tag>-source.tar.gz. If it is
  missing, write to the project's issue tracker and we will send it; the offer is
  good for three years after the release date.

${DXC_NOTE}

Microsoft d3dcompiler_47.dll is not included and not required.
VERS



mkdir -p "$OUT"
TARBALL="$OUT/Scarlight-$VERSION.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner \
  --mtime="@$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)" \
  -C "$WORK" -czf "$TARBALL" Scarlight

echo "$TARBALL"
echo "  Scarlight/: compatibilitytool.vdf, toolmanifest.vdf, scarlight, scarlight.conf.example, LICENSE, files/"
echo "  files/: victory-$BUILDNAME.exe, d3d12.dll, d3d12core.dll, dxil.dll, dxcompiler.dll, licenses/"
echo "Extract it into Steam's compatibilitytools.d and restart Steam."
