#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'HELP'
Usage: bash install.sh [--with-proton] [--uninstall VERSION] [--help]
Set SCARLIGHT_VERSION to install a specific version, e.g. 11.14.8.
--with-proton also installs the required GE-Proton if missing.
HELP
}
with_proton=0
uninstall=''
while (($#)); do
  case "$1" in
    --with-proton) with_proton=1 ;;
    --uninstall) shift; (($#)) || { usage >&2; exit 2; }; uninstall=$1 ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done
valid_version() { [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; }
if [[ -n $uninstall ]] && ! valid_version "$uninstall"; then
  echo 'Invalid version.' >&2; exit 2
fi
steam_dirs=()
steam_roots=()
for dir in "$HOME/.local/share/Steam" "$HOME/.steam/root" "$HOME/.var/app/com.valvesoftware.Steam/data/Steam"; do
  [[ -d $dir ]] || continue
  root=$(cd -P -- "$dir" && pwd -P)
  duplicate=0
  for existing in "${steam_roots[@]}"; do
    [[ $root != "$existing" ]] || duplicate=1
  done
  ((duplicate)) && continue
  steam_roots+=("$root")
  steam_dirs+=("$dir/compatibilitytools.d")
done
((${#steam_dirs[@]})) || { echo 'Steam not found.' >&2; exit 1; }
if [[ -n $uninstall ]]; then
  for dir in "${steam_dirs[@]}"; do
    if [[ -d $dir/Scarlight-$uninstall ]]; then
      rm -rf -- "$dir/Scarlight-$uninstall"
      echo "Removed $dir/Scarlight-$uninstall"
    fi
  done
  exit 0
fi
version=${SCARLIGHT_VERSION:-}
if [[ -z $version ]]; then
  releases=$(curl -fsSL 'https://api.github.com/repos/pablogonz12/Scarlight/releases?per_page=100')
  version=$(printf '%s\n' "$releases" | sed -n 's/^[[:space:]]*"tag_name": "v\([0-9]*\.[0-9]*\.[0-9]*\)".*/\1/p' | sed -n '1p')
fi
valid_version "$version" || { echo 'Invalid or unavailable release version.' >&2; exit 1; }
# Each release version records its GE major and release; this also supports pinned older releases.
major=${version%%.*}; rest=${version#*.}; ge_release=${rest%%.*}
base="GE-Proton${major}-${ge_release}"
tmp=$(mktemp -d)
stage=''
cleanup() {
  [[ -z $stage ]] || rm -rf -- "$stage"
  rm -rf -- "$tmp"
}
trap cleanup EXIT
archive="Scarlight-$version.tar.gz"
url="https://github.com/pablogonz12/Scarlight/releases/download/v$version"
curl -fsSL "$url/$archive" -o "$tmp/$archive"
curl -fsSL "$url/SHA256SUMS.txt" -o "$tmp/SHA256SUMS.txt"
(cd "$tmp" && grep -F "  $archive" SHA256SUMS.txt | grep -E "  ${archive//./\\.}$" | sha256sum -c -)
# Stage once; never overwrite an existing version and its user files.
tar -xzf "$tmp/$archive" -C "$tmp"
[[ -d $tmp/Scarlight-$version ]] || { echo 'Unexpected archive layout.' >&2; exit 1; }
need_proton=0
for dir in "${steam_dirs[@]}"; do
  if [[ ! -x $dir/$base-x86_64/files/bin/wine && ! -x $dir/$base/files/bin/wine ]]; then need_proton=1; fi
done
if ((need_proton && with_proton)); then
  ge="$base-x86_64"
  geurl="https://github.com/GloriousEggroll/proton-ge-custom/releases/download/$base"
  curl -fsSL "$geurl/$ge.tar.gz" -o "$tmp/$ge.tar.gz"
  curl -fsSL "$geurl/$ge.sha512sum" -o "$tmp/$ge.sha512sum"
  (cd "$tmp" && sha512sum -c "$ge.sha512sum")
  tar -xzf "$tmp/$ge.tar.gz" -C "$tmp"
  [[ -x $tmp/$ge/files/bin/wine ]] || { echo 'Unexpected GE-Proton archive layout.' >&2; exit 1; }
fi
for dir in "${steam_dirs[@]}"; do
  mkdir -p "$dir"
  if ((need_proton && with_proton)) && [[ ! -x $dir/$base-x86_64/files/bin/wine && ! -x $dir/$base/files/bin/wine ]]; then
    [[ ! -e $dir/$ge ]] || { echo "Cannot overwrite $dir/$ge" >&2; exit 1; }
    stage=$(mktemp -d "$dir/.scarlight-stage.XXXXXXXX")
    cp -a "$tmp/$ge" "$stage/item"
    mv -T --no-clobber "$stage/item" "$dir/$ge"
    [[ ! -e $stage/item ]] || { echo "Cannot overwrite $dir/$ge" >&2; exit 1; }
    rmdir "$stage"
    stage=''
    echo "Installed $ge in $dir"
  fi
  if [[ -e $dir/Scarlight-$version ]]; then
    echo "Already installed: $dir/Scarlight-$version (left unchanged)"
    continue
  fi
  previous=''
  for conf in "$dir"/Scarlight-*/scarlight.conf; do
    [[ -f $conf ]] || continue
    candidate=${conf%/scarlight.conf}; candidate=${candidate##*/Scarlight-}
    valid_version "$candidate" || continue
    if [[ -z $previous ]] || [[ $(printf '%s\n%s\n' "$previous" "$candidate" | sort -V | tail -n 1) == "$candidate" ]]; then previous=$candidate; fi
  done
  stage=$(mktemp -d "$dir/.scarlight-stage.XXXXXXXX")
  cp -a "$tmp/Scarlight-$version" "$stage/item"
  if [[ -n $previous ]]; then
    cp -- "$dir/Scarlight-$previous/scarlight.conf" "$stage/item/scarlight.conf"
  else
    printf 'BUILD=default\nPROTON=\nEXTRA_ENV=VICTORY_LOG=info\n' > "$stage/item/scarlight.conf"
  fi
  mv -T --no-clobber "$stage/item" "$dir/Scarlight-$version"
  [[ ! -e $stage/item ]] || { echo "Cannot overwrite $dir/Scarlight-$version" >&2; exit 1; }
  rmdir "$stage"
  stage=''
  echo "Installed Scarlight $version in $dir"
done
if ((need_proton && !with_proton)); then
  echo "Install $base from https://github.com/GloriousEggroll/proton-ge-custom/releases/tag/$base into each Steam compatibilitytools.d, or rerun with --with-proton."
fi
echo "Restart Steam, then pick Scarlight $version under the game's Properties > Compatibility."
