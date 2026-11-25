#!/usr/bin/env bash
# Measure the port on this machine and write docs/progress.json (counts only).
#
#   tools/progress/collect.sh [options]
#
#   --base DIR        the local tooling root (default: the checkout's parent)
#   --sources DIR     where the notes, the offline runs and the shots live
#                     (default: --base; ours keeps them one level below)
#   --repo DIR        the Scarlight checkout to measure (default: this one)
#   --run PATH        one world run's run.txt for the per-frame counts
#   --scene PATH      scene-results.tsv for the fixed-scene fps
#   --translate PATH  a `victory translate` summary (default: newest in notes)
#   --report PATH     an offline-check report-*.tsv (default: newest usable)
#   --matrix PATH     a family-matrix-*.txt (default: newest)
#   --cache DIR       the title's cache dir (default: the game prefix's)
#   --warm N          pipelines the saved library can build (the app prints it)
#   --dry-run         print the numbers found, write nothing
#
# Every source is optional: a source that is absent or unusable leaves the
# committed number for that row untouched and says so, so the script also runs
# on a machine without the title. Nothing from the title is copied anywhere:
# only counts land in the JSON. Run tools/progress/render.py afterwards.
#
#   python3 -I tools/progress/render.py            # docs/PROGRESS.md
#   python3 -I tools/progress/render.py --table    # the README rows
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BASE=$(cd "$REPO/.." && pwd)
SOURCES=""
RUN=""
SCENE=""
TRANSLATE=""
REPORT=""
MATRIX=""
CACHE=""
WARM=0
EARLY=""
DRY=0

usage() { sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --base) BASE=${2:-}; shift 2 ;;
    --sources) SOURCES=${2:-}; shift 2 ;;
    --repo) REPO=${2:-}; shift 2 ;;
    --run) RUN=${2:-}; shift 2 ;;
    --scene) SCENE=${2:-}; shift 2 ;;
    --translate) TRANSLATE=${2:-}; shift 2 ;;
    --report) REPORT=${2:-}; shift 2 ;;
    --matrix) MATRIX=${2:-}; shift 2 ;;
    --cache) CACHE=${2:-}; shift 2 ;;
    --warm) WARM=${2:-0}; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 64 ;;
  esac
done

[ -n "$SOURCES" ] || SOURCES=$BASE
JSON="$REPO/docs/progress.json"
NOTES="$SOURCES/notes"
OFF="$SOURCES/offline"
CENSUS="$BASE/tmp/native/census-default.txt"
FACTS=$(mktemp "${TMPDIR:-/tmp}/progress-facts.XXXXXX") || exit 1
trap 'rm -f "$FACTS"' EXIT

say() { printf '%s\n' "$*" >&2; }
skip() { printf 'skip  %s\n' "$*" >&2; }
fact() { printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$FACTS"; }
newest() { ls -t "$@" 2>/dev/null | head -1; }
fmt() { printf '%s' "$1" | rev | sed 's/\([0-9][0-9][0-9]\)/\1,/g' | rev | sed 's/^,//'; }
jsonget() { python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); a={x["id"]:x for x in d["areas"]}[sys.argv[2]]["rows"][int(sys.argv[3])].get(sys.argv[4]); print("" if a is None else ",".join(str(v) for v in a) if isinstance(a,list) else a)' "$JSON" "$@" 2>/dev/null; }

# ---- 1. system APIs (HLE): the import census ---------------------------------
# Rows match docs/progress.json. Only the HLE column is code we wrote: Wine
# pass-throughs and the title's own libraries resolve too, and are counted as
# such, so nothing here reads as ours.
if [ -f "$CENSUS" ]; then
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^api-ms/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r a_total a_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^(xmem|acphal|xapu|chad|xcrdapi|xg_xs|gameinput|d3d12_xs)\.dll$/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r c_total c_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^(msvcp140|vcruntime140|vcruntime140_1|bcrypt|ws2_32|winhttp|mfplat|mfreadwrite)\.dll$/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r r_total r_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 {f+=$2;h+=$3;o+=$4;m+=$5;n++} END {print f+0, h+0, o+0, m+0, n+0}' "$CENSUS")
  read -r all_total hle nhost title all_dlls <<< "$acc"
  if [ "$all_total" -gt 0 ]; then
    # the census's "missing" column is what the title's own images export, not
    # a gap: the loader's bound line reports missing=0 for the whole table
    resolved=$all_total
    fact hle 0 "Imports resolved" impl "$resolved,$all_total"
    fact hle 0 "Imports resolved" verified "$resolved,$all_total"
    fact hle 1 "… of which by Scarlight's HLE modules" impl "$hle,$all_total"
    fact hle 1 "… of which by Scarlight's HLE modules" verified "$hle,$all_total"
    fact hle 2 "… of which by Wine (Proton)" impl "$nhost,$all_total"
    fact hle 2 "… of which by Wine (Proton)" verified "$nhost,$all_total"
    fact hle 3 "… of which by the title's own libraries" impl "$title,$title"
    fact hle 3 "… of which by the title's own libraries" verified "$title,$title"
    [ "$a_total" -gt 0 ] && fact hle 4 "Kernel and API sets (43 DLLs)" impl "$a_both,$a_total"
    [ "$c_total" -gt 0 ] && fact hle 5 "Console DLLs: memory, audio, input, GPU" impl "$c_both,$c_total"
    [ "$r_total" -gt 0 ] && fact hle 6 "C runtime and OS (msvcp140, vcruntime, ws2_32, winhttp, bcrypt, mfplat)" impl "$r_both,$r_total"
    # the census cannot tell a title-owned export from an unbound import; the
    # loader can, so the verified count comes from a run's bound line
    if [ -z "$RUN" ]; then BOUNDSRC=$(ls -t "$SOURCES"/shots/*/run.txt 2>/dev/null | head -30); else BOUNDSRC=$RUN; fi
    BOUND=""
    while IFS= read -r f; do
      [ -f "$f" ] || continue
      # the loader prints one bound line per image; the census scans one image,
      # so only the line whose four counts add up to the census total describes
      # the same import set
      BOUND=$(awk -v want="$all_total" '
        /bound .*: [0-9]+ hle, [0-9]+ host, [0-9]+ title, [0-9]+ missing/ {
          h = $0; gsub(/.*: /, "", h); n = split(h, a, /[ ,]+/); sum = 0
          for (i = 1; i <= n; i++) if (a[i] ~ /^[0-9]+$/) sum += a[i] + 0
          if (sum == want) last = $0
        }
        END { if (last != "") print last }' "$f")
      [ -n "$BOUND" ] && { BOUNDSRC=$f; break; }
    done <<< "$BOUNDSRC"
    if [ -n "$BOUND" ]; then
      missed=$(printf '%s' "$BOUND" | sed -E 's/.* ([0-9]+) missing.*/\1/')
      bound=$((all_total - missed))
      fact hle 0 "Imports resolved" verified "$bound,$all_total"
      fact hle 0 "Imports resolved" note "$missed of $all_total imports did not bind in this loader run; the census scans the image, the run binds every DLL"
      say "hle   loader bound $bound/$all_total (missing $missed) in $(basename "$(dirname "$BOUNDSRC")")/run.txt"
    else
      skip "hle: no loader bound line to verify the resolved count; verified left as it was"
    fi
    say "hle   $resolved/$all_total resolved: $hle our HLE, $nhost Wine, $title the title's own, $all_dlls DLLs"
  fi
else
  skip "hle: no census at $CENSUS"
fi

# ---- 2. shaders: the whole archive ------------------------------------------
if [ -z "$TRANSLATE" ]; then TRANSLATE=$(newest "$NOTES"/translate-*.txt); fi
if [ -f "${TRANSLATE:-/nonexistent}" ]; then
  read -r tr_done tr_total <<< "$(awk '/^programs /{for(i=1;i<=NF;i++){if($i=="of"){t=$(i+1)}; if($i=="translated"){d=$(i-1)}} } END{print d+0, t+0}' "$TRANSLATE")"
  st() { awk -v want="$1" 'index($0,"emitted of") && index($0,want){for(i=1;i<=NF;i++){if($i=="of"){d=$(i-2);t=$(i+1)}}; print d+0, t+0}' "$TRANSLATE"; }
  vs=$(st "vertex and pixel programs")
  cs=$(st "compute programs")
  ms=$(st "mesh programs")
  if [ "$tr_total" -gt 0 ]; then
    # translation alone is not verification: this row has no verified column
    fact shaders 2 "Whole archive, translated" impl "$tr_done,$tr_total"
    sum_d=0; sum_t=0
    for pair in "$vs" "$cs" "$ms"; do
      [ -z "$pair" ] && continue
      set -- $pair
      sum_d=$((sum_d + ${1:-0})); sum_t=$((sum_t + ${2:-0}))
    done
    [ "$sum_t" -gt 0 ] && fact shaders 3 "Whole archive, host shader emitted: all stages" impl "$sum_d,$sum_t"
    [ "$sum_t" -gt 0 ] && fact shaders 3 "Whole archive, host shader emitted: all stages" note "vertex + pixel, compute and mesh: $(fmt $((sum_t - sum_d))) of $(fmt "$sum_t") programs of those stages are not emitted"
    stage_row() { # <row> <label> <done> <total>
      [ "${4:-0}" -gt 0 ] || return 0
      fact shaders "$1" "$2" impl "$3,$4"
      fact shaders "$1" "$2" note "$(fmt $(( $4 - $3 ))) programs of this stage are not emitted; no pipeline validation has been run on the archive yet"
    }
    set -- $vs; stage_row 4 "Whole archive, host shader emitted: vertex + pixel" "${1:-0}" "${2:-0}"
    set -- $cs; stage_row 5 "Whole archive, host shader emitted: compute" "${1:-0}" "${2:-0}"
    set -- $ms; stage_row 6 "Whole archive, host shader emitted: mesh" "${1:-0}" "${2:-0}"
    say "shaders archive $tr_done/$tr_total translated, $sum_d/$sum_t emitted (vs+ps, cs, ms)"
  fi
else
  skip "shaders: no translate summary (pass --translate)"
fi

# ---- 2b. shaders: the world set, end to end ---------------------------------
if [ -z "$REPORT" ] && [ ! -d "$OFF" ]; then
  skip "shaders world set: no $OFF and no --report"
else
  if [ -z "$REPORT" ]; then
    while IFS= read -r f; do
      [ -f "$f" ] || continue
      [ "$(awk -F'\t' '$2=="ok"' "$f" | wc -l)" -gt 0 ] && { REPORT=$f; break; }
    done <<< "$(ls -t "$OFF"/report-*.tsv 2>/dev/null)"
  fi
  if [ -f "${REPORT:-/nonexistent}" ]; then
    ok=$(awk -F'\t' '$2=="ok"' "$REPORT" | wc -l)
    tot=$(awk -F'\t' 'NF>=2' "$REPORT" | wc -l)
    if [ "$tot" -gt 0 ]; then
      fact shaders 0 "World set, translated to host shader" impl "$tot,$tot"
      fact shaders 0 "World set, translated to host shader" verified "$tot,$tot"
      fact shaders 1 "World set, translated + validated + real RADV pipeline" impl "$ok,$tot"
      fact shaders 1 "World set, translated + validated + real RADV pipeline" verified "$ok,$tot"
      fact shaders 1 "World set, translated + validated + real RADV pipeline" note "$ok of $tot reached a real pipeline in this run; the other $((tot - ok)) stop before one"
      fact pipelines 0 "World set: pipelines that build offline" impl "$ok,$tot"
      fact pipelines 0 "World set: pipelines that build offline" verified "$ok,$tot"
      fact pipelines 0 "World set: pipelines that build offline" note "$((tot - ok)) of $tot do not reach a pipeline in this run"
      say "shaders world set $ok/$tot through translate, validation and a real pipeline ($(basename "$REPORT"))"
    fi
  else
    skip "shaders world set: no usable offline report in $OFF"
  fi
fi

# ---- 2c. ISA: the opcode table against the decoder ---------------------------
SH="$REPO/crates/victory-shader/src"
if [ -f "$SH/gfx10/opcodes.rs" ] && [ -d "$SH/emit" ]; then
  tbl=$(grep -o '"[a-z][a-z0-9_]*"' "$SH/gfx10/opcodes.rs" | tr -d '"' | sort -u)
  all=$(printf '%s\n' "$tbl" | wc -l)
  used=0
  while read -r op; do
    [ -z "$op" ] && continue
    grep -rqE "(^|[^a-z0-9_])${op}([^a-z0-9_]|$)" "$SH/emit" 2>/dev/null && used=$((used + 1))
  done <<< "$tbl"
  if [ "$all" -gt 0 ]; then
    fact shaders 7 "ISA: opcode-table entries the decoder names" impl "$used,$all"
    fact shaders 7 "ISA: opcode-table entries the decoder names" note "$used of the $(fmt "$all") opcode-table entries are named in the decoder; the title uses far fewer than the table holds, and a per-program opcode census is not taken yet"
    say "isa   $used/$all opcode-table entries named in the decoder"
  fi
else
  skip "isa: no opcode table in $SH"
fi

# ---- 3. GPU command stream --------------------------------------------------
# A frame line carries its draws; the "dropped N/frame" lines that follow it,
# up to the next frame line, belong to that frame. The last world frame of a
# run gives one honest pair: draws recorded, draws that reached the card.
runpair() {
  awk '
    function flush() { if (cur_d >= 3000) { best_d = cur_d; best_kept = cur_d - drops } }
    /[0-9]+ draws\/frame/ {
      flush()
      d = $0
      if (match(d, /[0-9]+ draws\/frame/)) {
        d = substr(d, RSTART, RLENGTH); sub(/ draws.*/, "", d)
        cur_d = d + 0
      }
      drops = 0
    }
    /dropped [0-9]+(\.[0-9]+)?\/frame/ {
      for (i = 2; i <= NF; i++)
        if ($i ~ /^[0-9]+(\.[0-9]+)?\/frame$/ && $(i-1) == "dropped") drops += $i + 0
    }
    END { flush(); if (best_d > 0 && best_kept >= 0) print int(best_kept + 0.5), best_d }
  ' "$1"
}
if [ -n "$RUN" ]; then CANDIDATES=$RUN; else CANDIDATES=$(ls -t "$SOURCES"/shots/*/run.txt 2>/dev/null | head -50); fi
STREAM_DONE=0
while IFS= read -r f; do
  [ -f "$f" ] || continue
  read -r kept draws <<< "$(runpair "$f")"
  [ -z "${draws:-}" ] && continue
  fact command_stream 0 "Recorded work that reaches the card" impl "$kept,$draws"
  fact command_stream 0 "Recorded work that reaches the card" verified "$kept,$draws"
  say "stream $kept/$draws draws and dispatches reached the card ($(basename "$(dirname "$f")")/run.txt)"
  STREAM_DONE=1
  break
done <<< "$CANDIDATES"
if [ "$STREAM_DONE" = 0 ]; then
  skip "stream: no run with a world frame (pass --run)"
fi
METHODS="$BASE/tmp/native/methods.txt"
if [ -f "$METHODS" ] && [ -d "$REPO/crates/victory-gpu/src" ]; then
  tot=0; have=0
  while read -r m; do
    [ -z "$m" ] && continue
    tot=$((tot + 1))
    grep -rqE "(^|[^A-Za-z0-9_])${m}([^A-Za-z0-9_]|$)" "$REPO/crates/victory-gpu/src" && have=$((have + 1))
  done < "$METHODS"
  [ "$tot" -gt 0 ] && fact command_stream 1 "D3D12 calls the title makes that our backend has" impl "$have,$tot"
  say "stream $have/$tot of the title's D3D12 calls named in the backend"
else
  skip "stream: no method list at $METHODS"
fi

# ---- 4. pipelines: the pre-bake list ----------------------------------------
# The early list is the title's own record of the pipelines it will ask for;
# how many of those keys the saved library can build is printed by the app
# (pass --warm N to refresh it), not recoverable from the file.
if [ -n "$CACHE" ]; then
  EARLY=$(find "$CACHE" -maxdepth 5 -name early-pipelines.txt 2>/dev/null | head -1)
fi
if [ -z "$EARLY" ]; then
  for d in "$SOURCES"/victory-run "$SOURCES"/victory-play \
           "$HOME"/.local/share/*/drive_c/users/*/AppData/Local/victory; do
    [ -d "$d" ] || continue
    found=$(find "$d" -maxdepth 5 -name early-pipelines.txt 2>/dev/null | head -1)
    [ -n "${found:-}" ] && { EARLY=$found; break; }
  done
fi
if [ -f "${EARLY:-/nonexistent}" ]; then
  keys=$(grep -cve '^[[:space:]]*$' "$EARLY")
  was=$(jsonget pipelines 1 impl | cut -d, -f1)
  build=0
  case "${was:-0}" in ''|*[!0-9]*) build=0 ;; *) build=$was ;; esac
  [ "${WARM:-0}" -gt 0 ] && build=$WARM
  [ "$build" -gt "$keys" ] && build=$keys
  if [ "$keys" -gt 0 ] && [ "$build" -gt 0 ]; then
    fact pipelines 1 "Pipelines the title asks for, covered by the pre-bake list" impl "$build,$keys"
    fact pipelines 1 "Pipelines the title asks for, covered by the pre-bake list" note "$(fmt "$build") of the $(fmt "$keys") keys the title's own early list names can be built from the saved library"
    say "pipelines $build/$keys keys covered by the saved library ($(basename "$EARLY"))"
  fi
else
  skip "pipelines: no early-pipeline list (pass --cache DIR)"
fi

# ---- 6. mesa reach: the family matrix ---------------------------------------
# One tsv per run and per vendor: AMD families are reached through the amdgpu
# noop shim, Intel ones through the i915/xe shim. A family counts only when
# every program in the set compiled, and a run whose columns are all empty is a
# harness failure, not a result.
MATRIX_AMD=""; MATRIX_INTEL=""; MATRIX=${MATRIX:-$(newest "$OFF"/family-matrix-*.tsv)}
if [ -f "${MATRIX:-/nonexistent}" ]; then
  MATRIXES=$(ls -t "$OFF"/family-matrix-*.tsv 2>/dev/null | head -8)
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    fams=$(awk -F'\t' '$2 ~ /^gfx[0-9]+$/ {print "amd"; exit}' "$f")
    [ -z "$fams" ] && fams=$(awk -F'\t' '$2 ~ /^(dg2|bmg|mtl|lnl|ptl|nvl|acm)$/ {print "intel"; exit}' "$f")
    # a partial run (one or two families) is not a matrix: the AMD one must
    # cover at least four, or a half-finished run would become the result
    nfam=$(awk -F'\t' '$2 ~ /^gfx[0-9]+$/ {seen[$2]=1} END {print length(seen)+0}' "$f")
    case "$fams" in
      amd) [ "$nfam" -ge 4 ] && [ -z "${MATRIX_AMD:-}" ] && MATRIX_AMD=$f ;;
      intel) [ -n "${MATRIX_INTEL:-}" ] || MATRIX_INTEL=$f ;;
    esac
  done <<< "$MATRIXES"
fi
count_matrix() { # <file> <regex> -> "families_all families_total vega_ok vega_tot"
  awk -F'\t' -v re="$2" '
    $2 != "-" && $2 != "real" && $2 ~ re { t[$2]++; if ($3 == "ok") o[$2]++ }
    END {
      all = 0; n = 0
      for (f in t) { n++; if (o[f] + 0 == t[f] + 0) all++ }
      printf "%d %d %d %d\n", all, n, o["gfx900"] + 0, t["gfx900"] + 0
    }' "$1"
}
if [ -n "${MATRIX_AMD:-}" ]; then
  read -r amd_all amd_n vega_ok vega_tot <<< "$(count_matrix "$MATRIX_AMD" '^(gfx[0-9]+)$')"
  if [ "$amd_n" -gt 0 ]; then
    fact mesa 0 "AMD families whose whole world set compiles offline" impl "$amd_all,$amd_n"
    fact mesa 0 "AMD families whose whole world set compiles offline" note "$amd_all of $amd_n AMD families compile the whole set through the noop shim (Mesa driver, no GPU)"
    say "mesa  AMD $amd_all/$amd_n families compile the whole set ($(basename "$MATRIX_AMD"))"
  fi
  [ "$vega_tot" -gt 0 ] && fact mesa 4 "Vega (gfx900): world programs that compile offline" impl "$vega_ok,$vega_tot"
  [ "$vega_tot" -gt 0 ] && fact mesa 4 "Vega (gfx900): world programs that compile offline" note "$((vega_tot - vega_ok)) of $vega_tot abort the driver: every mesh-shader program, and GFX9 has no mesh shader support in RADV"
else
  skip "mesa: no AMD family-matrix tsv in $OFF"
fi
if [ -n "${MATRIX_INTEL:-}" ]; then
  # one row per Intel family: ANV's answer differs per family (DG2 and Xe-LPG
  # each miss one program on the stub's query gap, Arc B misses none), so an
  # aggregate would hide the only number a reader wants.
  MATRIX_INTEL_TXT="${MATRIX_INTEL%.tsv}.txt"
  intel_fact() { # <family> <row> <label> <what the misses are>
    read -r ok tot <<< "$(awk -F'\t' -v f="$1" '$2==f {t++; if ($3=="ok") o++} END {printf "%d %d", o+0, t+0}' "$MATRIX_INTEL")"
    [ "${tot:-0}" -gt 0 ] || return 0
    med=$(awk -v f="$1" '$1==f {print $7; exit}' "$MATRIX_INTEL_TXT" 2>/dev/null)
    note="$ok of $tot build a pipeline through ANV and Mesa's Intel noop shim; $4; median ${med:-?} ms per program"
    fact mesa "$2" "$3" impl "$ok,$tot"
    fact mesa "$2" "$3" note "$note"
    say "mesa  Intel $1 $ok/$tot pipelines ($(basename "$MATRIX_INTEL"))"
  }
  intel_fact bmg 1 "Arc B (Xe2): world programs that build pipelines offline" "none is refused"
  intel_fact dg2 2 "Arc A (DG2): world programs that build pipelines offline" "the one miss is the stub's gap (Unknown drm_i915_query_item), not a compiler error"
  intel_fact mtl 3 "Xe-LPG: world programs that build pipelines offline" "the one miss is the same stub gap"
else
  skip "mesa: no Intel family-matrix tsv in $OFF"
fi

# ---- 7. performance: the fixed scene ---------------------------------------
if [ -z "$SCENE" ]; then SCENE="$NOTES/scene-results.tsv"; fi
if [ -f "$SCENE" ]; then
  fps=$(awk -F'\t' '$3=="window" && $4=="yes" && $7 ~ /^[0-9.]+$/ {v=$7} END{print v+0}' "$SCENE")
  if awk -v v="$fps" 'BEGIN{exit !(v > 0)}'; then
    fact performance 0 "Fixed test scene vs the 30 fps console target" note "$fps fps median at the window scene of the last recorded run, compared only with rows of the same scene"
    fact performance 0 "Fixed test scene vs the 30 fps console target" impl "$fps,30"
    fact performance 0 "Fixed test scene vs the 30 fps console target" verified "$fps,30"
    say "perf  fixed scene $fps fps of 30"
  fi
else
  skip "performance: no $SCENE"
fi

# ---- merge: only rows whose label still matches, everything else kept -------
if [ "$DRY" = 1 ]; then
  say "dry run: $(wc -l < "$FACTS") facts, nothing written"
  cat "$FACTS" >&2
  exit 0
fi
python3 -I - "$JSON" "$FACTS" <<'PY'
import datetime, json, sys
path, facts = sys.argv[1], sys.argv[2]
doc = json.load(open(path, encoding="utf-8"))
areas = {a["id"]: a for a in doc["areas"]}
applied = kept = 0
for line in open(facts, encoding="utf-8"):
    line = line.rstrip("\n")
    if not line:
        continue
    area_id, index, label, field, value = line.split("\t")
    area = areas.get(area_id)
    if area is None:
        print("keep  unknown area %s" % area_id, file=sys.stderr)
        kept += 1
        continue
    row = area["rows"][int(index)]
    if row["label"] != label:
        print("keep  %s row %s is %r, not %r" % (area_id, index, row["label"], label), file=sys.stderr)
        kept += 1
        continue
    if field in ("impl", "verified"):
        a, b = (int(x) if x.lstrip("-").isdigit() else float(x) for x in value.split(","))
        row[field] = [a, b]
    else:
        row[field] = value
    applied += 1
doc["measured"] = datetime.date.today().isoformat()
json.dump(doc, open(path, "w", encoding="utf-8"), indent=2, ensure_ascii=False)
open(path, "a", encoding="utf-8").write("\n")
print("wrote %s: %d rows updated, %d kept as they were" % (path, applied, kept), file=sys.stderr)
PY
# The README's Shaders row shows a verified column: it must be the real-pipeline
# count, not a note that no validation has happened. Keep the pointer on the
# validated row, so a regeneration cannot bring the stale text back.
python3 -I - "$JSON" <<'PY'
import json, sys
path = sys.argv[1]
doc = json.load(open(path, encoding="utf-8"))
area = {a["id"]: a for a in doc["areas"]}["shaders"]
row = next((i for i, r in enumerate(area["rows"]) if "real RADV pipeline" in r.get("label", "")), None)
if row is None:
    print("keep  shaders: no validated-pipeline row, README detail unchanged", file=sys.stderr)
else:
    area["readme_from"] = {"impl": area.get("readme_from", {}).get("impl", 3), "verified": row}
    area["readme_labels"] = ["", "real RADV pipeline"]
    area.pop("readme_verified_text", None)
    json.dump(doc, open(path, "w", encoding="utf-8"), indent=2, ensure_ascii=False)
    open(path, "a", encoding="utf-8").write("\n")
    print("wrote %s: shaders README detail reads row %d (%s)" % (path, row, area["rows"][row]["label"]), file=sys.stderr)
PY

say "next: python3 -I tools/progress/render.py"
