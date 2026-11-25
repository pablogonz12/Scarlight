#!/usr/bin/env python3
"""Generate tools/progress/rdna2_isa_forms.txt: the RDNA2 reference form list.

usage: python3 -I tools/progress/gen_isa_forms.py [options]

  --source PATH   a local copy of Mesa's aco_opcodes.py (default: download)
  --out PATH      output, default tools/progress/rdna2_isa_forms.txt
  --check         exit 1 if --out differs from what it would write

The reference is one line per instruction form of the RDNA2 family, grouped by
encoding: the public table Mesa ships for its ACO compiler, `GFX10` column,
which is gfx1010..gfx1030 and is what the console's shaders use. It is the same
public source our decoder's opcode tables were generated from, so the two lists
can be compared form by form. Nothing here comes from a title: the names and
opcodes are AMD's published instruction set.

The table is not vendored: it is fetched at a pinned tag, and the file this
script writes is small, textual and committed, so the dashboard never needs a
network at check time.
"""

import argparse
import os
import sys
import urllib.request

TAG = "mesa-24.3.0"
URL = (
    "https://gitlab.freedesktop.org/mesa/mesa/-/raw/%s/src/amd/compiler/aco_opcodes.py"
    % TAG
)

# The order the dashboard shows: scalar, then vector, then memory.
ORDER = [
    "SOP1",
    "SOP2",
    "SOPK",
    "SOPC",
    "SOPP",
    "SMEM",
    "VOP1",
    "VOP2",
    "VOP3",
    "VOP3P",
    "VOPC",
    "DS",
    "MUBUF",
    "MTBUF",
    "MIMG",
    "FLAT",
    "GLOBAL",
    "SCRATCH",
    "EXP",
    "VINTRP",
]

# Pseudo instructions are compiler-internal (p_*), not encodings.
PSEUDO = {"PSEUDO", "PSEUDO_BRANCH", "PSEUDO_REDUCTION", "PSEUDO_BARRIER"}

# Modifier forms: a second dword or two that re-addresses a base instruction's
# operands. They are not distinct opcodes, so the dashboard counts them once
# per base encoding instead of per opcode. Base encodings are the ones Mesa's
# Format bitmask (DPP16, DPP8, SDWA) is documented to combine with.
MODIFIERS = [
    ("DPP16", ["VOP1", "VOP2", "VOPC"]),
    ("DPP8", ["VOP1", "VOP2", "VOPC"]),
    ("SDWA", ["VOP1", "VOP2", "VOPC"]),
]

HEADER = """\
# RDNA2 (gfx1030) reference instruction forms, one per line.
#
# PROVENANCE
#   Source:   Mesa src/amd/compiler/aco_opcodes.py, tag {tag} (MIT licence),
#             the GFX10 column. Mesa's GFX10 covers the RDNA1/RDNA2 family
#             (gfx1010..gfx1030); the platform this project targets is gfx1030.
#   URL:      {url}
#   Why this source: it is a public, machine-readable list of AMD's
#             instruction set with the encoding of every form, and it is the
#             same table our decoder's opcode lists were generated from, so a
#             form can be compared one-to-one instead of by eye.
#   Regenerate: python3 -I tools/progress/gen_isa_forms.py (fetches the tag
#             above; --source PATH uses a local copy instead).
#   Licence:  aco_opcodes.py carries "Copyright (c) 2018 Valve Corporation",
#             SPDX-License-Identifier: MIT. Only instruction names, opcodes
#             and encoding families are reproduced here.
#
# FORMAT
#   encoding<TAB>opcode<TAB>name, sorted by encoding (see ORDER in the
#   generator) and then by opcode. Blank lines and lines starting with '#' are
#   ignored. A trailing # [modifiers] section lists the modifier forms as
#   modifier<TAB>base encodings, space separated.
#
# This list is a reference, not a target: it is every form the family defines,
# including ones no shader of ours has ever contained. A dashboard count over
# it says which forms we name, never that a shader using them would run.

"""


def load(source):
    """Exec Mesa's opcode module in a namespace and return its forms."""
    with open(source, "r", encoding="utf-8") as f:
        text = f.read()
    namespace = {"__name__": "aco_opcodes"}
    exec(compile(text, source, "exec"), namespace)
    instructions = namespace["instructions"]
    forms = []
    for insn in instructions.values():
        fmt = insn.format.name
        if fmt in PSEUDO or fmt not in ORDER:
            continue
        if getattr(insn.op, "gfx10", -1) < 0:
            continue
        forms.append((fmt, insn.op.gfx10, insn.name))
    forms.sort(key=lambda row: (ORDER.index(row[0]), row[1], row[2]))
    return forms


def render(forms):
    out = [HEADER.format(tag=TAG, url=URL)]
    for encoding, opcode, name in forms:
        out.append("%s\t0x%03x\t%s\n" % (encoding, opcode, name))
    out.append("\n# [modifiers]\n")
    out.append("# modifier\tbase encodings that accept it\n")
    for modifier, bases in MODIFIERS:
        out.append("%s\t%s\n" % (modifier, " ".join(bases)))
    return "".join(out), len(forms)


def fetch(target):
    with urllib.request.urlopen(URL, timeout=60) as response:
        data = response.read()
    with open(target, "wb") as f:
        f.write(data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", default=None)
    ap.add_argument("--out", default=None)
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    out_path = args.out or os.path.join(root, "tools", "progress", "rdna2_isa_forms.txt")
    source = args.source
    if source is None:
        source = os.path.join(root, "target", "aco_opcodes-%s.py" % TAG)
        if not os.path.exists(source):
            os.makedirs(os.path.dirname(source), exist_ok=True)
            fetch(source)

    text, count = render(load(source))
    if args.check:
        old = ""
        if os.path.exists(out_path):
            with open(out_path, "r", encoding="utf-8") as f:
                old = f.read()
        if old != text:
            print(
                "%s is stale: run gen_isa_forms.py" % os.path.relpath(out_path, root),
                file=sys.stderr,
            )
            return 1
        return 0
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    print("wrote %s (%d forms)" % (os.path.relpath(out_path, root), count))
    return 0


if __name__ == "__main__":
    sys.exit(main())
