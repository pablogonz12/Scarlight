#!/usr/bin/env bash
# Which Proton and which path the Steam tool picks, against fake
# compatibilitytools.d folders. Runs `scarlight --print`: no Wine, no game.
#   bash tests/steam-compat-pick.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fails=0

# A Proton folder with a Wine; $2=paired gives its d3d12core.dll the view-heap marker.
proton() {
  mkdir -p "$1/files/bin" "$1/files/lib/wine/vkd3d-proton/x86_64-windows"
  printf '#!/bin/sh\n' > "$1/files/bin/wine"
  chmod +x "$1/files/bin/wine"
  if [ "${2:-}" = paired ]; then
    printf 'xx VKD3D_VIEW_HEAP_DESCRIPTORS xx' > "$1/files/lib/wine/vkd3d-proton/x86_64-windows/d3d12core.dll"
  else
    printf 'xx stock xx' > "$1/files/lib/wine/vkd3d-proton/x86_64-windows/d3d12core.dll"
  fi
}

# setup NAME: a fresh HOME whose compatibilitytools.d holds this tool.
setup() {
  H="$T/$1"
  CT="$H/.local/share/Steam/compatibilitytools.d"
  mkdir -p "$CT/Scarlight"
  cp "$ROOT/tools/steam-compat/scarlight" "$ROOT/tools/steam-compat/BASE" "$CT/Scarlight/"
  DEV="$H/udmabuf"
}

# expect NAME WANT_PROTON WANT_FAST WANT_TEXT [env...]
expect() {
  local name="$1" want_p="$2" want_f="$3" want_t="$4" out
  shift 4
  out="$(env -i PATH="$PATH" HOME="$H" STEAM_COMPAT_DATA_PATH="$H/data" SCARLIGHT_UDMABUF="$DEV" "$@" \
    bash "$CT/Scarlight/scarlight" --print 2>&1)"
  if printf '%s\n' "$out" | grep -q "^proton=.*/$want_p\$" &&
    printf '%s\n' "$out" | grep -q "^fast=$want_f\$" &&
    printf '%s\n' "$out" | grep -qF "$want_t"; then
    echo "ok   $name"
  else
    echo "FAIL $name"
    fails=$((fails + 1))
  fi
  printf '%s\n' "$out" | sed 's/^/     /'
}

setup fast
proton "$CT/GE-Proton11-14"
proton "$CT/GE-Proton11-14-victory5" paired
proton "$CT/GE-Proton11-14-victory10" paired
mkdir "$CT/GE-Proton11-14-victory11" # no Wine: must not win
: > "$DEV"
chmod 600 "$DEV"
expect "newest paired build, udmabuf usable" GE-Proton11-14-victory10 1 "fast path on GE-Proton11-14-victory10"

rm -f "$DEV"
expect "no udmabuf device" GE-Proton11-14 0 "does not exist (load it: sudo modprobe udmabuf)"

: > "$DEV"
chmod 400 "$DEV"
expect "udmabuf not writable" GE-Proton11-14 0 "is not readable and writable"
chmod 600 "$DEV"

printf 'FAST=0\n' > "$CT/Scarlight/scarlight.conf"
expect "FAST=0 in the conf" GE-Proton11-14 0 "FAST=0 in scarlight.conf"
rm "$CT/Scarlight/scarlight.conf"

setup nomarker
proton "$CT/GE-Proton11-14"
proton "$CT/GE-Proton11-14-victory5"
: > "$DEV"
expect "paired name without the paired vkd3d" GE-Proton11-14 0 "has no paired vkd3d-proton"

setup stockonly
proton "$CT/GE-Proton11-14"
proton "$CT/GE-Proton11-15"
: > "$DEV"
expect "stock only" GE-Proton11-14 0 "no GE-Proton11-14-victory<N> in compatibilitytools.d"

setup pairedonly
proton "$CT/GE-Proton11-14-victory5" paired
expect "paired only, no udmabuf: paired on the safe settings" GE-Proton11-14-victory5 0 "safe path on GE-Proton11-14-victory5"

setup explicit
proton "$CT/GE-Proton11-14"
proton "$CT/GE-Proton11-14-victory5" paired
: > "$DEV"
printf 'PROTON=GE-Proton11-14\n' > "$CT/Scarlight/scarlight.conf"
expect "explicit stock PROTON stays safe" GE-Proton11-14 0 "has no paired vkd3d-proton"

echo "$fails failure(s)"
[ "$fails" = 0 ]
