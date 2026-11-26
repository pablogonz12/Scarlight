#!/bin/bash
# The frame's watch legs, quiet and with the title's address-space churn:
# b  the kernel's asking over the same spans
# e  our own guards with the kernel's asking every 8th frame as the verifier
# Run through xcl-bin/notes/build-slot.sh (it runs Wine).
cd "$(dirname "$0")" || exit 1
EXE=target/x86_64-pc-windows-msvc/release/copybench.exe
export WINEPREFIX=${WINEPREFIX:-$HOME/.cache/wwstall-prefix} WINEDEBUG=-all
W=$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-14-x86_64/files/bin/wine
for mode in quiet contend; do
  extra=""
  [ "$mode" = contend ] && extra=--contend
  for i in 1 2 3; do
    echo "== $mode run $i"
    "$W" "$EXE" be $extra 2>&1 | grep -E "^[be] "
  done
done
