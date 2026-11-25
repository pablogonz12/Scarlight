#!/usr/bin/env bash
# release-notes.sh: draft the next CHANGELOG section from the merged PRs.
#
# Usage: tools/release-notes.sh [FROM-TAG]
#   FROM-TAG   the release the draft starts after (default: the newest v* tag)
#
# Reads tools/steam-compat/BASE for the GE series and counts that series' tags for
# the revision after this one, then lists what was merged since FROM-TAG grouped as
# Picture / Speed / Load time / Stability / Tools. The grouping is a guess from the
# titles: reword, move and cut before pasting, and fill in what a title cannot say
# (numbers, what is still broken). docs/RELEASES.md has the cadence.
#
# Needs gh on the PATH and the tags fetched (git fetch --tags). SCARLIGHT_REPO
# overrides the repository it asks.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
REPO="${SCARLIGHT_REPO:-pablogonz12/Scarlight}"

case "${1:-}" in
  -h | --help)
    sed -n '2,14p' "$0"
    exit 0
    ;;
esac

FROM="${1:-}"
if [ -z "$FROM" ]; then
  FROM="$(git -C "$ROOT" tag --list 'v*' --sort=-v:refname | head -1)"
fi
if [ -z "$FROM" ]; then
  echo "no v* tag to start from: pass FROM-TAG (run git fetch --tags first)" >&2
  exit 1
fi

BASE="$(sed -n '1{s/[[:space:]]*$//;p;}' "$HERE/steam-compat/BASE")"
SERIES="${BASE#GE-Proton}"
SERIES="${SERIES/-/.}"
SERIES_RE="$(printf '%s' "$SERIES" | sed 's/\./\\./g')"
LAST="$(git -C "$ROOT" tag --list "v$SERIES.*" |
  sed -n "s/^v$SERIES_RE\.\([0-9][0-9]*\)$/\1/p" | sort -n | tail -1)"
REV=1
if [ -n "$LAST" ]; then REV=$((10#$LAST + 1)); fi  # 10#: a revision like 08 is decimal, not octal
NEXT="$SERIES.$REV"
# The tag's own commit time, not its date: a PR merged the same morning, before
# the tag was cut, belongs to the release that already shipped it.
SINCE="$(git -C "$ROOT" log -1 --format=%ct "$FROM" 2>/dev/null || true)"
if [ -z "$SINCE" ]; then
  echo "no commit for $FROM: run git fetch --tags, or pass a tag that exists" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
gh pr list --repo "$REPO" --state merged --limit 400 \
  --json number,title,mergedAt \
  --jq '.[] | "\(.mergedAt)\t\(.number)\t\(.title)"' | sort > "$WORK/prs.tsv"
while IFS=$'\t' read -r merged num title; do
  [ -n "${num:-}" ] || continue
  when="$(date -u -d "$merged" +%s 2>/dev/null || echo 0)"
  if [ "$when" -ge "$((SINCE - 0))" ]; then
    printf '%s\t%s\t%s\n' "$merged" "$num" "$title"
  fi
done < "$WORK/prs.tsv" | sort -t$'\t' -k1,1 > "$WORK/since.tsv"
COUNT="$(grep -c . "$WORK/since.tsv" || true)"

# One bucket per PR, first match wins. Picture is the default because most work
# here is what the card draws; anything the keywords miss is listed last.
bucket() {
  local t="$1"
  case "$t" in
    *[Tt]ools* | *docs* | *README* | *CI:* | *ci:* | *workflow* | *[Rr]elease* | \
      *[Ll]auncher* | *[Cc]hangelog* | *CONTRIBUTING* | *[Bb]rand* | *[Tt]ests* | *[Tt]est\ *)
      echo "Tools"
      ;;
    *[Ss]peed* | *faster* | *perf* | *[Ww]orker* | *budget* | *[Ss]tarv* | *throughput* | \
      *[Cc]ache*)
      echo "Speed"
      ;;
    *[Ll]oad* | *boot* | *[Ss]tart* | *[Rr]esidency* | *stream* | *first\ frame* | \
      *[Cc]ompile\ order* | *[Rr]esident*)
      echo "Load time"
      ;;
    *crash* | *[Hh]ang* | *[Ll]eak* | *[Gg]uard* | *flaky* | *overflow* | *refus* | \
      *[Rr]etry* | *domin* | *[Ff]ix*)
      echo "Stability"
      ;;
    *) echo "Picture" ;;
  esac
}

{
  printf '## [%s] - %s\n\n' "$NEXT" "$(date +%F)"
  printf '<!-- Draft from the %s merged PRs after %s. Reclassify, reword and cut. -->\n' \
    "$COUNT" "$FROM"
  for section in Picture Speed "Load time" Stability Tools; do
    lines=""
    while IFS=$'\t' read -r _ num title; do
      [ -n "${title:-}" ] || continue
      if [ "$(bucket "$title")" = "$section" ]; then
        lines="$lines- $title (#$num)"$'\n'
      fi
    done < "$WORK/since.tsv"
    if [ -n "$lines" ]; then
      printf '\n### %s\n\n%s' "$section" "$lines"
    fi
  done
  printf '\n## Notes for the writer\n\n'
  printf -- '- Base: %s (change tools/steam-compat/BASE, not this file)\n' "$BASE"
  printf -- '- Version in the tarball and the tag: v%s\n' "$NEXT"
  printf -- '- Check docs/RELEASES.md: a revision only ships when the dry run passes.\n'
}
