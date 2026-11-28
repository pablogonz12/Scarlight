#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
export HOME="$tmp/home" FIXTURES="$tmp/fixtures"
export PATH="$tmp/bin:$PATH" SCARLIGHT_VERSION=11.14.8
mkdir -p "$HOME/.local/share/Steam" "$FIXTURES" "$tmp/bin" \
  "$tmp/archive/Scarlight-11.14.8" "$tmp/archive/GE-Proton11-14-x86_64/files/bin"
printf 'payload\n' > "$tmp/archive/Scarlight-11.14.8/payload"
printf '#!/bin/sh\n' > "$tmp/archive/GE-Proton11-14-x86_64/files/bin/wine"
chmod +x "$tmp/archive/GE-Proton11-14-x86_64/files/bin/wine"
tar -czf "$FIXTURES/Scarlight-11.14.8.tar.gz" -C "$tmp/archive" Scarlight-11.14.8
tar -czf "$FIXTURES/GE-Proton11-14-x86_64.tar.gz" -C "$tmp/archive" GE-Proton11-14-x86_64
(cd "$FIXTURES" && sha256sum Scarlight-11.14.8.tar.gz > SHA256SUMS.txt &&
  sha512sum GE-Proton11-14-x86_64.tar.gz > GE-Proton11-14-x86_64.sha512sum)
cat > "$tmp/bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
url=$2
[[ $3 == -o ]]
/usr/bin/cp -- "$FIXTURES/${url##*/}" "$4"
SH
cat > "$tmp/bin/cp" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${FAIL_COPY:-} == proton && $* == *GE-Proton11-14-x86_64* && $* == *stage* ]] ||
   [[ ${FAIL_COPY:-} == scarlight && $* == *Scarlight-11.14.8* && $* == *stage* ]]; then
  mkdir -p "${@: -1}"
  exit 1
fi
exec /usr/bin/cp "$@"
SH
chmod +x "$tmp/bin/curl" "$tmp/bin/cp"
dir="$HOME/.local/share/Steam/compatibilitytools.d"
run() { bash "$root/install.sh" "$@" > "$tmp/output" 2>&1; }
clean_stage() { ! compgen -G "$dir/.scarlight-stage.*" > /dev/null; }

# A corrupt checksum must fail before any installation is published.
printf 'bad\n' > "$FIXTURES/SHA256SUMS.txt"
if run --with-proton; then exit 1; fi
[[ ! -e $dir/Scarlight-11.14.8 && ! -e $dir/GE-Proton11-14-x86_64 ]]
(cd "$FIXTURES" && sha256sum Scarlight-11.14.8.tar.gz > SHA256SUMS.txt)

# Failed copies leave neither partial destinations nor staging directories.
if FAIL_COPY=proton run --with-proton; then exit 1; fi
[[ ! -e $dir/GE-Proton11-14-x86_64 ]]
clean_stage
if FAIL_COPY=scarlight run --with-proton; then exit 1; fi
[[ -x $dir/GE-Proton11-14-x86_64/files/bin/wine && ! -e $dir/Scarlight-11.14.8 ]]
clean_stage
run --with-proton
grep -Fx 'PROTON=' "$dir/Scarlight-11.14.8/scarlight.conf"
[[ -f $dir/Scarlight-11.14.8/payload ]]
clean_stage

# Existing installations remain unchanged, including their configuration.
printf 'PROTON=custom\n' > "$dir/Scarlight-11.14.8/scarlight.conf"
run
grep -Fx 'PROTON=custom' "$dir/Scarlight-11.14.8/scarlight.conf"

# Automatic selection also works when only the unsuffixed Proton is present.
export HOME="$tmp/second-home"
dir="$HOME/.local/share/Steam/compatibilitytools.d"
mkdir -p "$dir/GE-Proton11-14/files/bin"
printf '#!/bin/sh\n' > "$dir/GE-Proton11-14/files/bin/wine"
chmod +x "$dir/GE-Proton11-14/files/bin/wine"
run --with-proton
[[ ! -e $dir/GE-Proton11-14-x86_64 ]]
grep -Fx 'PROTON=' "$dir/Scarlight-11.14.8/scarlight.conf"

# A new version inherits an earlier configuration unchanged.
export HOME="$tmp/third-home"
dir="$HOME/.local/share/Steam/compatibilitytools.d"
mkdir -p "$dir/Scarlight-11.14.7"
printf 'PROTON=my-choice\n' > "$dir/Scarlight-11.14.7/scarlight.conf"
run
grep -Fx 'PROTON=my-choice' "$dir/Scarlight-11.14.8/scarlight.conf"
printf 'installer tests passed\n'
