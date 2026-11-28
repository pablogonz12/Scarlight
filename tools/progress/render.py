#!/usr/bin/env python3
"""Render docs/progress.json into docs/PROGRESS.md and a compact table.

usage: python3 -I tools/progress/render.py [options]

  --json PATH     input, default docs/progress.json
  --out PATH      full page, default docs/PROGRESS.md
  --table         print the compact README table to stdout (6-8 rows) and stop
  --check         exit 1 if --out differs from what it would write

The JSON holds counts only: what was counted, how much of it we do, and how
much of that a run has shown working. No game data, no program names.
"""

import argparse
import json
import os
import sys

CELLS = 12
FULL = "\u2588"  # █
EMPTY = "\u2591"  # ░
ARROW = " \u2192 "


def bar(done, total):
    """Text progress bar: 12 cells, so 83 % reads ██████████░░ 83 %."""
    if total <= 0:
        return "n/a"
    done = max(0, min(total, done))
    # nearest, then held below 100 %: a set that is not complete must not read
    # 100 %, and 88/587 must not read 14 %
    cells = int(round(done * CELLS / total))
    if done > 0:
        cells = max(1, cells)
    if done >= total:
        cells, pct = CELLS, 100
    else:
        cells = min(CELLS - 1, cells)
        pct = min(99, int(round(done * 100.0 / total)))
    return FULL * cells + EMPTY * (CELLS - cells) + " %d %%" % pct


def cell(row, key="impl"):
    """'done / total' for one column, or an em dash when it was not counted."""
    pair = row.get(key)
    if not pair or len(pair) != 2 or pair[0] is None or pair[1] is None:
        return "\u2014"
    return "%s / %s" % (num(pair[0]), num(pair[1]))


def num(value):
    if isinstance(value, int) or (isinstance(value, float) and value.is_integer()):
        return "{:,}".format(int(value))
    return str(value)


def row_table(area):
    """One markdown table per area."""
    out = []
    head = "| %s | Implemented | Verified | Progress | What is counted |"
    out.append(head % area.get("column", "Area"))
    out.append("|---|---|---|---|---|")
    for row in area["rows"]:
        label = row["label"]
        if "status" in row:
            impl = row.get("impl_text", "\u2014")
            ver = row.get("ver_text", "\u2014")
            prog = row["status"]
        else:
            impl = cell(row, "impl")
            ver = cell(row, "verified")
            prog = bar(row["impl"][0], row["impl"][1])
        note = row.get("note", "")
        out.append("| %s | %s | %s | %s | %s |" % (label, impl, ver, prog, note))
    return "\n".join(out)


def isa_section():
    """The source-counted ISA section, written by shader_scan.py.

    It lives in its own file so this renderer stays the only writer of
    docs/PROGRESS.md and the per-title numbers stay the only thing
    collect.sh touches.
    """
    path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        "docs",
        "progress",
        "isa.md",
    )
    if not os.path.exists(path):
        return ""
    with open(path, "r", encoding="utf-8") as f:
        return f.read().strip()


def page(doc):
    head = doc["header"]
    lines = ["# Progress", ""]
    lines.append("**" + head["meaning"] + "**")
    lines.append("")
    lines.append(head["where"])
    lines.append("")
    for line in head.get("notes", []):
        lines.append("- " + line)
    lines.append("")
    lines.append(
        "`tools/progress/render.py` regenerates the page from existing counts without an "
        "executable. Collect new runtime measurements only with test inputs authorized "
        "for this environment."
    )
    lines.append("")
    for area in doc["areas"]:
        lines.append("## %s" % area["title"])
        lines.append("")
        lines.append(area["summary"])
        lines.append("")
        lines.append(row_table(area))
        lines.append("")
        if area.get("foot"):
            for line in area["foot"]:
                lines.append("- " + line)
            lines.append("")
    lines.append("## How each number is measured")
    lines.append("")
    for row in doc["method"]:
        lines.append("- **%s**: %s" % (row["area"], row["how"]))
    lines.append("")
    isa = isa_section()
    if isa:
        lines.append(isa)
        lines.append("")
    lines.append(doc["footer"])
    lines.append("")
    return "\n".join(lines)


def compact(doc, limit=8):
    """The 6-8 row table the README carries.

    Every cell is derived from the same row of docs/PROGRESS.md that the detail
    page shows, so the compact table can never quote a different counted set.
    """
    out = ["| Area | Progress | Detail | Where |", "|---|---|---|---|"]
    for area in doc["areas"]:
        if area.get("readme") is False:
            continue
        where = area.get("readme_where", "[detail](docs/PROGRESS.md)")
        src = area.get("readme_from") or {}
        labels = area.get("readme_labels") or ["", ""]
        impl_row = area["rows"][src["impl"]] if src.get("impl") is not None else None
        ver_row = area["rows"][src["verified"]] if src.get("verified") is not None else None
        impl_col = src.get("impl_column", "impl")
        if impl_row is None or impl_col not in impl_row or impl_row[impl_col] is None:
            left = area.get("readme_text", "\u2014")
        else:
            pair = impl_row[impl_col]
            label = labels[0]
            if area.get("readme_resolved"):
                label = "resolved (%d missing)" % (pair[1] - pair[0])
            left = (bar(*pair) + " " + label).strip()
        if ver_row is not None and "impl" in ver_row:
            right = (bar(*ver_row["impl"]) + " " + labels[1]).strip()
        else:
            right = area.get("readme_verified_text", "\u2014")
        out.append("| %s | %s | %s | %s |" % (area["title"], left, right, where))
        if len(out) - 2 >= limit:
            break
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--json", default="docs/progress.json")
    ap.add_argument("--out", default="docs/PROGRESS.md")
    ap.add_argument("--table", action="store_true")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    jpath = args.json if os.path.isabs(args.json) else os.path.join(root, args.json)
    with open(jpath, "r", encoding="utf-8") as f:
        doc = json.load(f)

    if args.table:
        print(compact(doc))
        return 0

    text = page(doc)
    opath = args.out if os.path.isabs(args.out) else os.path.join(root, args.out)
    if args.check:
        old = ""
        if os.path.exists(opath):
            with open(opath, "r", encoding="utf-8") as f:
                old = f.read()
        if old != text:
            print("%s is stale: run collect.sh and render.py" % args.out, file=sys.stderr)
            return 1
        return 0
    with open(opath, "w", encoding="utf-8") as f:
        f.write(text)
    print("wrote %s (%d bytes)" % (args.out, len(text)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
