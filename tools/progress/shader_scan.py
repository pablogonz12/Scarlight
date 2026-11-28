#!/usr/bin/env python3
"""Source-only ISA coverage dashboard: what the decoder and each backend cover.

usage: python3 -I tools/progress/shader_scan.py [options]

  --ref PATH       reference forms, default tools/progress/rdna2_isa_forms.txt
  --date YYYY-MM-DD  the day this point belongs to, default today (UTC)
  --check          exit 1 if any generated file differs from what it would write
  --table          print the compact summary table to stdout and stop

Everything is counted from this repository's sources and one public reference
list, so CI can run it: no title, no dumps, no hashes, no network.

Inputs (all in the repo)
  tools/progress/rdna2_isa_forms.txt   every RDNA2 form, by encoding (generated)
  crates/victory-shader/src/gfx10/opcodes.rs   the decoder's opcode tables
  crates/victory-shader/src/ir.rs, emit/*.rs   the DXIL path (translation)
  crates/victory-shader/src/emit/spirv.rs      the direct SPIR-V emitter, if any
  crates/victory-shader/src/emit/lower.rs      typed-buffer formats
  crates/victory-formats/src/texture.rs        texture formats with a layout
  crates/victory-hle/src/modules/*.rs          HLE exports per module

Outputs
  docs/progress/isa.json               counts only, what the page renders from
  docs/progress/isa.md                 the docs/PROGRESS.md section, markers included
  docs/progress/shaders.svg            the compact progress box
  docs/progress/index.html             the GitHub Pages page
  docs/progress/progress-history.json  one point per day, appended
"""

import argparse
import datetime
import glob
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

ROW_ORDER = [
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

# How many of the undispatched forms to name in the "still needed" list.
NEEDED_SHOWN = 60


def rel(path):
    return os.path.relpath(path, ROOT).replace(os.sep, "/")


def read(path):
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        return f.read()


# --------------------------------------------------------------------------
# The reference list


def load_reference(path):
    """Return (forms, modifiers) from the generated reference file."""
    forms = []
    modifiers = []
    section = "forms"
    for line in read(path).splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("#"):
            if line.startswith("# [modifiers]"):
                section = "modifiers"
            continue
        parts = line.split("\t")
        if section == "forms" and len(parts) == 3:
            forms.append({"enc": parts[0], "opcode": int(parts[1], 16), "name": parts[2]})
        elif section == "modifiers" and len(parts) == 2:
            modifiers.append({"modifier": parts[0], "bases": parts[1].split()})
    return forms, modifiers


# --------------------------------------------------------------------------
# The decoder's opcode tables


def decoder_tables(path):
    """encoding -> {opcode: name}, and the set of every name it carries."""
    tables = {}
    current = None
    for line in read(path).splitlines():
        head = re.match(r"(?:pub\(super\)\s+)?const\s+(\w+)\s*:", line)
        if head:
            current = head.group(1)
            tables.setdefault(current, {})
            continue
        row = re.match(r'\s*\(0x([0-9a-fA-F]+),\s*"([a-z0-9_]+)"\)', line)
        if current and row:
            tables[current][int(row.group(1), 16)] = row.group(2)
    names = {name for table in tables.values() for name in table.values()}
    return tables, names


# --------------------------------------------------------------------------
# The backends


def backend_tokens(paths):
    """Every lowercase instruction-shaped string literal in the given files."""
    tokens = set()
    literal = re.compile(r'"([a-z][a-z0-9_]*)"')
    for path in paths:
        tokens |= set(literal.findall(read(path)))
    return tokens


def dxil_sources():
    files = [
        path
        for path in sorted(glob.glob(os.path.join(ROOT, "crates/victory-shader/src/emit/*.rs")))
        if not path.endswith("_tests.rs")
        # The direct emitter's files are not the DXIL path: a literal only
        # they use is not a case for a translated instruction.
        and not os.path.basename(path).startswith("spirv")
    ]
    ir = os.path.join(ROOT, "crates/victory-shader/src/ir.rs")
    if os.path.exists(ir):
        files.append(ir)
    return files


def spirv_sources():
    """The direct emitter's files, or [] when the branch does not have it."""
    found = []
    for pattern in (
        "crates/victory-shader/src/emit/spirv*.rs",
        "crates/victory-shader/src/spirv/*.rs",
    ):
        found += [
            path
            for path in sorted(glob.glob(os.path.join(ROOT, pattern)))
            if not path.endswith("_tests.rs")
        ]
    return found


def families(tokens):
    """Literals that name an instruction family, e.g. `ds_read`, `buffer_load_`."""
    return {token for token in tokens if len(token) > 3 and token.endswith("_")}


def coverage(forms, names, tokens, prefix):
    """Per form: is it decoded, and does a backend have a case for it?

    `names` are the decoder's; `tokens` are the backend's. A form counts as a
    case when its own name occurs in the backend's sources, or when a family
    prefix does (`ds_read_` covers `ds_read_b32`): both are one dispatch.
    Mesa tags the 64-bit VOP3 encoding with a `_e64` suffix; our tables name
    the same opcode without it, so the suffix is dropped before comparing.
    """
    names = names | {name[: -len("_e64")] for name in names if name.endswith("_e64")}
    tokens = tokens | {
        token[: -len("_e64")] for token in tokens if token.endswith("_e64")
    }
    def short_of(name):
        return name[: -len("_e64")] if name.endswith("_e64") else name

    # One opcode, several names (VOP2 0x006 is both `v_fmac_legacy_f32` and
    # `v_mac_legacy_f32`, which the decoder names): the opcode is decoded,
    # so every name of it is.
    decoded_keys = set()
    for form in forms:
        if short_of(form["name"]) not in names:
            continue
        decoded_keys.add((form["enc"], form["opcode"]))
    rows = []
    for form in forms:
        name = form["name"]
        short = short_of(name)
        decoded = (form["enc"], form["opcode"]) in decoded_keys
        exact = decoded and short in tokens
        via = ""
        if decoded and not exact:
            for token in prefix:
                if short.startswith(token):
                    via = token
                    break
        rows.append(
            {
                "enc": form["enc"],
                "name": name,
                "opcode": form["opcode"],
                "decoded": decoded,
                "case": exact,
                "family": via,
            }
        )
    return rows


def summarise(rows):
    out = []
    for enc in ROW_ORDER:
        mine = [row for row in rows if row["enc"] == enc]
        if not mine:
            continue
        out.append(
            {
                "enc": enc,
                "forms": len(mine),
                "decoded": sum(1 for row in mine if row["decoded"]),
                "case": sum(1 for row in mine if row["case"]),
                "family": sum(1 for row in mine if row["family"]),
                "spirv": sum(1 for row in mine if row["spirv"]),
            }
        )
    return out


# --------------------------------------------------------------------------
# Formats


def typed_buffer_formats(path):
    """Number of GFX10 typed-buffer formats the emitter can convert."""
    text = read(path)
    start = text.find("fn buffer_format(")
    if start < 0:
        return None
    end = text.find("\n}\n", start)
    body = text[start:end]
    families = re.search(r"FAMILIES:\s*\[.*?\];", body, re.S)
    if not families:
        return None
    wide = re.search(r"const WIDE:\s*&\[Numeric\]\s*=\s*&\[([^\]]*)\]", body)
    norm = re.search(r"const NORM:\s*&\[Numeric\]\s*=\s*&\[([^\]]*)\]", body)
    allk = re.search(r"const ALL:\s*&\[Numeric\]\s*=\s*&\[([^\]]*)\]", body)
    kinds = {
        "WIDE": len(wide.group(1).split(",")) if wide else 0,
        "NORM": len(norm.group(1).split(",")) if norm else 0,
        "ALL": len(allk.group(1).split(",")) if allk else 0,
    }
    total = 0
    for kind in re.findall(r"&\[[^\]]*\],\s*(\w+)\)", families.group(0)):
        total += kinds.get(kind, 0)
    return total


def vulkan_format_count():
    """Distinct Vulkan formats this tree maps a console format to, by name."""
    found = set()
    for path in glob.glob(os.path.join(ROOT, "crates/*/src/**/*.rs"), recursive=True):
        text = read(path)
        found |= set(re.findall(r"vk::Format::([A-Z0-9_]+)", text))
        found |= set(re.findall(r"VK_FORMAT_([A-Z0-9_]+)", text))
    return len(found)


def texture_formats(path):
    """DXGI format numbers that victory-formats gives a pixel layout."""
    text = read(path)
    start = text.find("pub fn format(dxgi: u32)")
    if start < 0:
        return 0
    end = text.find("\n}\n", start)
    body = text[start:end]
    numbers = set()
    for arm in re.findall(r"\n\s*([0-9][0-9x\.\s=>|]*)\s*=>", body):
        arm = arm.split("..=")[0].strip()
        for chunk in arm.split("|"):
            chunk = chunk.strip()
            if "..=" in chunk:
                continue
            if chunk.isdigit():
                numbers.add(int(chunk))
    # Ranges are written as `1..=4` inside an or-pattern of the same arm.
    for low, high in re.findall(r"(\d+)\.\.=(\d+)", body):
        numbers.update(range(int(low), int(high) + 1))
    for single in re.findall(r"\|\s*(\d+)\s*(?=[|=>])", body):
        numbers.add(int(single))
    return len(numbers)


# --------------------------------------------------------------------------
# HLE modules


def hle_modules(directory):
    """name, DLLs, exports and stubs per module, from the module sources."""
    modules = []
    for path in sorted(glob.glob(os.path.join(directory, "*.rs"))):
        text = read(path)
        start = text.find("fn module() -> HleModule")
        if start < 0:
            continue
        end = text.find("\n}\n", start)
        body = text[start:end]
        name = re.search(r'name:\s*"([^"]+)"', body)
        dlls = re.findall(r'"([^"]+\.dll)"', body)
        exports = re.findall(r"hle_export!\(\s*([A-Za-z0-9_]+)\s*\)", body)
        exports += re.findall(r'name:\s*"([^"]+)"', body)
        exports = sorted(set(exports) - {name.group(1) if name else ""})
        stubs = [fn for fn in stubbed_functions(text) if fn in exports]
        modules.append(
            {
                "module": name.group(1) if name else os.path.basename(path)[:-3],
                "dlls": len(dlls),
                "exports": len(exports),
                "stubs": len(stubs),
            }
        )
    modules.sort(key=lambda row: (-row["exports"], row["module"]))
    return modules


def stubbed_functions(text):
    """Functions whose body says it is not implemented."""
    found = []
    for match in re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(<]", text):
        name = match.group(1)
        brace = text.find("{", match.end())
        if brace < 0:
            continue
        depth = 0
        for index in range(brace, min(len(text), brace + 4000)):
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
                if depth == 0:
                    body = text[brace:index]
                    if "E_NOTIMPL" in body or "not implemented" in body:
                        found.append(name)
                    break
    return found


# --------------------------------------------------------------------------
# The scan


def scan(args):
    forms, modifiers = load_reference(args.ref)
    tables, names = decoder_tables(
        os.path.join(ROOT, "crates/victory-shader/src/gfx10/opcodes.rs")
    )
    dxil = backend_tokens(dxil_sources())
    spirv_files = spirv_sources()
    spirv = backend_tokens(spirv_files) if spirv_files else None

    rows = coverage(forms, names, dxil, families(dxil))
    spirv_rows = coverage(forms, names, spirv or set(), families(spirv or set()))
    for row, other in zip(rows, spirv_rows):
        row["spirv"] = other["case"] or bool(other["family"])
    # A "case" in the SPIR-V column is only counted when that branch has it.
    per_encoding = summarise(rows if spirv_files else [
        {**row, "spirv": False} for row in rows
    ])

    totals = {
        "forms": len(rows),
        "decoded": sum(1 for row in rows if row["decoded"]),
        "case": sum(1 for row in rows if row["case"]),
        "family": sum(1 for row in rows if row["family"]),
    }
    totals["dxil"] = totals["case"] + totals["family"]
    totals["spirv"] = sum(1 for row in rows if row["spirv"]) if spirv_files else None

    decoded_names = {row["name"] for row in rows if row["decoded"]}
    needed = [
        row for row in rows if row["decoded"] and not row["case"] and not row["family"]
    ]
    undecoded = [row for row in rows if not row["decoded"]]

    modifier_rows = []
    decoder_fields = read(os.path.join(ROOT, "crates/victory-shader/src/gfx10/fields.rs"))
    lower_and_emit = " ".join(read(path) for path in dxil_sources())
    seen = {
        "DPP16": ("Dpp::Row", "dpp"),
        "DPP8": ("Dpp::Lanes", "dpp"),
        "SDWA": ("Sdwa {", "sdwa"),
    }
    for entry in modifiers:
        mine = [row for row in rows if row["enc"] in entry["bases"]]
        needle, backend_word = seen.get(entry["modifier"], ("", ""))
        modifier_rows.append(
            {
                "modifier": entry["modifier"],
                "bases": entry["bases"],
                "forms": len(mine),
                "decoded": sum(1 for row in mine if row["decoded"]),
                "case": sum(1 for row in mine if row["case"] or row["family"]),
                "decoder": bool(needle) and needle in decoder_fields,
                "backend": bool(backend_word) and backend_word in lower_and_emit,
            }
        )

    typed = typed_buffer_formats(
        os.path.join(ROOT, "crates/victory-shader/src/emit/lower.rs")
    )
    textured = texture_formats(
        os.path.join(ROOT, "crates/victory-formats/src/texture.rs")
    )
    hle = hle_modules(os.path.join(ROOT, "crates/victory-hle/src/modules"))

    return {
        "date": args.date,
        "reference": {
            "file": rel(args.ref),
            "forms": len(forms),
        },
        "totals": totals,
        "per_encoding": per_encoding,
        "modifiers": modifier_rows,
        "formats": {
            "typed_buffer": typed,
            "texture": textured,
            "vulkan": vulkan_format_count(),
        },
        "hle": {
            "modules": hle,
            "exports": sum(row["exports"] for row in hle),
            "stubs": sum(row["stubs"] for row in hle),
        },
        "spirv_branch": bool(spirv_files),
        "needed": [
            {"enc": row["enc"], "name": row["name"]} for row in needed[:NEEDED_SHOWN]
        ],
        "needed_total": len(needed),
        "undecoded": [{"enc": row["enc"], "name": row["name"]} for row in undecoded],
        "decoder_forms": sum(len(table) for table in tables.values()),
        "decoded_names": len(decoded_names),
    }


# --------------------------------------------------------------------------
# Rendering: text


def pct(done, total):
    if total <= 0:
        return "n/a"
    return "%d %%" % int(round(done * 100.0 / total))


def bar(done, total, cells=12):
    if total <= 0:
        return "n/a"
    filled = int(round(max(0, min(done, total)) * cells / float(total)))
    if done > 0:
        filled = max(1, filled)
    if done >= total:
        filled = cells
    else:
        filled = min(cells - 1, filled)
    return "\u2588" * filled + "\u2591" * (cells - filled) + " " + pct(done, total)


def md_section(data, svg_name="shaders.svg"):
    t = data["totals"]
    if data["spirv_branch"]:
        spirv_cell = lambda row: "%d / %d" % (row["spirv"], row["forms"])
    else:
        spirv_cell = lambda row: "not on this branch"
    out = []
    out.append("<!-- isa:start -->")
    out.append("## ISA coverage, counted from source")
    out.append("")
    out.append(
        "![ISA coverage](progress/%s)" % svg_name
    )
    out.append("")
    out.append(
        "**CI regenerates this section from source on every push, without an executable. "
        "It does not rerun the runtime measurements above.** It compares the "
        "decoder's opcode tables with a public reference list of every RDNA2 instruction form, "
        "then asks each backend whether it has a case for the form. Decoded, and a case in a "
        "backend, are statements about our code, not about a shader that ran: a form we name "
        "can still be translated wrongly, and nothing here is a picture."
    )
    out.append("")
    out.append(
        "**%d of %d reference forms decoded; %d with a DXIL-path case; %s.**"
        % (
            t["decoded"],
            t["forms"],
            t["dxil"],
            (
                "%d with a direct SPIR-V case" % t["spirv"]
                if data["spirv_branch"]
                else "the direct SPIR-V emitter is not on this branch, so that column is empty"
            ),
        )
    )
    out.append("")
    out.append("| Encoding | Forms | Decoded | DXIL path | … via a family prefix | SPIR-V direct | Bar |")
    out.append("|---|---|---|---|---|---|---|")
    for row in data["per_encoding"]:
        out.append(
            "| %s | %d | %d | %d | %d | %s | %s |"
            % (
                row["enc"],
                row["forms"],
                row["decoded"],
                row["case"] + row["family"],
                row["family"],
                spirv_cell(row),
                bar(row["decoded"], row["forms"]),
            )
        )
    out.append(
        "| **All encodings** | **%d** | **%d** | **%d** | **%d** | **%s** | **%s** |"
        % (
            t["forms"],
            t["decoded"],
            t["case"] + t["family"],
            t["family"],
            ("%d" % t["spirv"]) if data["spirv_branch"] else "—",
            bar(t["decoded"], t["forms"]),
        )
    )
    out.append("")
    out.append(
        "- A form is one opcode in one encoding, from `%s` (public source, provenance in the "
        "file header). \"Decoded\" means the decoder's opcode tables name it; \"DXIL path\" means "
        "the translator's own sources contain that name, or a family prefix it dispatches on "
        "(`ds_read_` covers `ds_read_b32`), which is one code path, not one case per opcode."
        % data["reference"]["file"]
    )
    out.append(
        "- A case is where a form is *mentioned*, so the count over-reads in one direction and "
        "under-reads in another: a form can be named by a length or a print helper and still be "
        "refused at translation. The 751-program offline check is the separate, per-title measure."
    )
    out.append("")
    out.append("### Modifier forms")
    out.append("")
    out.append(
        "DPP and SDWA re-address a base instruction's operands in a second dword; they are not "
        "opcodes of their own, so they are counted once per base encoding rather than per form."
    )
    out.append("")
    out.append("| Modifier | Base encodings | Forms they apply to | Decoder parses it | DXIL path reads it |")
    out.append("|---|---|---|---|---|")
    for row in data["modifiers"]:
        out.append(
            "| %s | %s | %d | %s | %s |"
            % (
                row["modifier"],
                ", ".join(row["bases"]),
                row["forms"],
                "yes" if row["decoder"] else "no",
                "yes" if row["backend"] else "no",
            )
        )
    out.append("")
    out.append(
        "- The form count is the base encodings' own: the modifier is a dword, so a form is "
        "\"decoded\" here when the base opcode is and the modifier's fields are parsed."
    )
    out.append("")
    out.append("### Formats")
    out.append("")
    out.append("| Set | Handled | Of | Mapped to a Vulkan format | What it means |")
    out.append("|---|---|---|---|---|")
    typed = data["formats"]["typed_buffer"] or 0
    textured = data["formats"]["texture"]
    vulkan = data["formats"]["vulkan"]
    out.append(
        "| Typed-buffer formats (`buffer_load_format_*`) | %d | %d | %d | the emitter converts "
        "the channel widths and numeric kind itself, so no Vulkan format is chosen for these"
        % (typed, typed, 0)
    )
    out.append(
        "| Texture formats with a pixel layout (DXGI numbers) | %d | %d | %d | `victory-formats` "
        "knows the bytes and block size of these; the host view kind is vkd3d-proton's business"
        % (textured, textured, 0)
    )
    out.append(
        "| Image formats (MIMG) | — | — | %d | the format is in the descriptor the title sets, "
        "not in the instruction, so there is nothing for the shader side to map" % 0
    )
    out.append("")
    out.append(
        "- \"Mapped to a Vulkan format\" is %d distinct format names in this tree, which is the "
        "honest answer for a stack that hands formats to vkd3d-proton and converts typed-buffer "
        "reads in the shader instead." % vulkan
    )
    out.append("")
    out.append("### Console modules (HLE)")
    out.append("")
    out.append("| Module | DLL names served | Exports | Stubs | Declared |")
    out.append("|---|---|---|---|---|")
    for row in data["hle"]["modules"]:
        out.append(
            "| %s | %d | %d | %d | %s |"
            % (
                row["module"],
                row["dlls"],
                row["exports"],
                row["stubs"],
                bar(row["exports"] - row["stubs"], max(row["exports"], 1)),
            )
        )
    out.append(
        "| **All modules** | | **%d** | **%d** | **%s** |"
        % (
            data["hle"]["exports"],
            data["hle"]["stubs"],
            bar(
                data["hle"]["exports"] - data["hle"]["stubs"],
                max(data["hle"]["exports"], 1),
            ),
        )
    )
    out.append("")
    out.append(
        "- Exports are `hle_export!` entries in each module's registry; a stub is an export whose "
        "body returns `E_NOTIMPL` or logs \"not implemented\". Counted from source, so a module "
        "we wrote a case for still counts as ours even if no title has called it."
    )
    out.append("")
    out.append("### Forms still needed")
    out.append("")
    if data["undecoded"]:
        out.append(
            "%d reference forms are not in the decoder's tables at all: %s."
            % (
                len(data["undecoded"]),
                ", ".join("`%s`" % row["name"] for row in data["undecoded"]),
            )
        )
        out.append("")
    out.append(
        "%d decoded forms have no DXIL-path case; the first %d, by encoding, are %s. The rest are "
        "in the translator's gaps, and the list is generated, so it shrinks as cases land."
        % (
            data["needed_total"],
            min(data["needed_total"], NEEDED_SHOWN),
            ", ".join("`%s`" % row["name"] for row in data["needed"]),
        )
    )
    out.append("")
    out.append(
        "- Read the two counts together: a form can be decoded and have no case (a shader using "
        "it stops at translation), or have a case and still be wrong (nothing here runs)."
    )
    out.append("")
    out.append(
        "Raw numbers: [progress/isa.json](progress/isa.json). History: "
        "[progress-history.json](progress/progress-history.json). Generated by "
        "`tools/progress/shader_scan.py`; the page is rebuilt on pushes to `main`, daily, and on "
        "demand by the Pages workflow."
    )
    out.append("<!-- isa:end -->")
    out.append("")
    return "\n".join(out)


# --------------------------------------------------------------------------
# Rendering: SVG


LIGHT = {
    "text": "#1f2328",
    "muted": "#59636e",
    "track": "#d8dee4",
    "done": "#0969da",
    "case": "#8250df",
    "bg": "#ffffff",
}
DARK = {
    "text": "#e6edf3",
    "muted": "#9198a1",
    "track": "#30363d",
    "done": "#4493f8",
    "case": "#a371f7",
    "bg": "#0d1117",
}


def esc(text):
    return (
        text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    )


def svg_document(width, height, body):
    """A standalone SVG that follows the reader's light or dark theme.

    The colours are plain CSS rules, not custom properties, so a renderer that
    does not support variables (librsvg, older sanitizers) falls back to the
    light theme instead of losing the fills.
    """

    def rules(colors):
        return (
            "text{fill:%s}.muted{fill:%s}.track{fill:%s}.done{fill:%s}"
            ".case{fill:%s}.bg{fill:%s}"
            % (
                colors["text"],
                colors["muted"],
                colors["track"],
                colors["done"],
                colors["case"],
                colors["bg"],
            )
        )

    style = (
        "<style>text{font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif}"
        + rules(LIGHT)
        + "@media (prefers-color-scheme:dark){"
        + rules(DARK)
        + "}"
        + "</style>"
    )
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" '
        'viewBox="0 0 %d %d" role="img" aria-label="RDNA2 form coverage">%s'
        '<rect class="bg" width="%d" height="%d" rx="8"/>%s</svg>'
        % (width, height, width, height, style, width, height, body)
    )


def progress_svg(data):
    t = data["totals"]
    rows = data["per_encoding"]
    label_w = 96
    bar_w = 360
    # The aggregate labels sit after their track (`label_w + total_w + 8`):
    # the document has to be wide enough for them.
    width = label_w + bar_w + 128
    top = 116
    step = 19
    height = top + step * len(rows) + 46
    parts = []
    parts.append(
        '<text x="20" y="28" font-size="15" font-weight="600">RDNA2 shader forms</text>'
    )
    parts.append(
        '<text x="20" y="46" font-size="11" class="muted">'
        + esc(
            "%d of %d decoded (%.0f%%) · %d with a DXIL case (%.0f%%)%s"
            % (
                t["decoded"],
                t["forms"],
                100.0 * t["decoded"] / max(t["forms"], 1),
                t["dxil"],
                100.0 * t["dxil"] / max(t["forms"], 1),
                " · SPIR-V direct: not on this branch"
                if not data["spirv_branch"]
                else " · %d with a SPIR-V case" % (t["spirv"] or 0),
            )
        )
        + "</text>"
    )
    total_w = bar_w + label_w - 20
    for index, (key, css, label) in enumerate(
        (("decoded", "done", "decoded"), ("dxil", "case", "DXIL case")),
    ):
        y = 58 + index * 20
        parts.append(
            '<text x="20" y="%d" font-size="10" class="muted">%s</text>' % (y + 8, label)
        )
        parts.append(
            '<rect class="track" x="%d" y="%d" width="%d" height="9" rx="4"/>'
            % (label_w, y, total_w)
        )
        filled = total_w * t[key] / float(max(t["forms"], 1))
        if filled > 0:
            parts.append(
                '<rect class="%s" x="%d" y="%d" width="%.1f" height="9" rx="4"/>'
                % (css, label_w, y, filled)
            )
        parts.append(
            '<text x="%d" y="%d" font-size="10" class="muted">%d/%d</text>'
            % (label_w + total_w + 8, y + 8, t[key], t["forms"])
        )
    for index, row in enumerate(rows):
        y = top + index * step
        total = max(row["forms"], 1)
        done_w = bar_w * row["decoded"] / float(total)
        case_w = bar_w * (row["case"] + row["family"]) / float(total)
        parts.append(
            '<text x="20" y="%d" font-size="11">%s</text>' % (y + 9, esc(row["enc"]))
        )
        for offset, length, css in ((0, done_w, "done"), (5, case_w, "case")):
            parts.append(
                '<rect class="track" x="%d" y="%d" width="%d" height="4" rx="2"/>'
                % (label_w, y + offset, bar_w)
            )
            if length > 0:
                parts.append(
                    '<rect class="%s" x="%d" y="%d" width="%.1f" height="4" rx="2"/>'
                    % (css, label_w, y + offset, length)
                )
        parts.append(
            '<text x="%d" y="%d" font-size="10" class="muted">%d/%d</text>'
            % (label_w + bar_w + 8, y + 9, row["decoded"], row["forms"])
        )
    foot = top + step * len(rows) + 12
    parts.append(
        '<text x="20" y="%d" font-size="10" class="muted">%s</text>'
        % (
            foot,
            esc(
                "Decoded and case-in-backend are counts from source, not a run: a named form can "
                "still translate wrongly."
            ),
        )
    )
    parts.append(
        '<text x="20" y="%d" font-size="10" class="muted">%s</text>'
        % (
            foot + 14,
            esc("tools/progress/shader_scan.py · %d forms · %s" % (t["forms"], data["date"])),
        )
    )
    return svg_document(width, height, "".join(parts))


# --------------------------------------------------------------------------
# Rendering: HTML


def history_chart(points):
    """A line chart of the daily points, as inline SVG."""
    if not points:
        return ""
    width, height = 760, 220
    left, right, top, bottom = 44, 16, 16, 42
    plot_w = width - left - right
    plot_h = height - top - bottom
    total = max(point["forms"] for point in points) or 1

    def x(index):
        if len(points) == 1:
            return left + plot_w / 2.0
        return left + plot_w * index / float(len(points) - 1)

    def y(value):
        return top + plot_h * (1 - value / float(total))

    parts = []
    for grid in range(0, 5):
        value = total * grid / 4.0
        line_y = y(value)
        parts.append(
            '<line x1="%d" y1="%.1f" x2="%d" y2="%.1f" stroke="#8b949e" '
            'stroke-opacity="0.25" stroke-width="1"/>' % (left, line_y, width - right, line_y)
        )
        parts.append(
            '<text x="%d" y="%.1f" font-size="10" class="muted" text-anchor="end">%d</text>'
            % (left - 6, line_y + 3, int(value))
        )
    for key, css in (("decoded", "done"), ("dxil", "case")):
        path = " ".join(
            "%s%.1f,%.1f" % ("M" if index == 0 else "L", x(index), y(point[key]))
            for index, point in enumerate(points)
        )
        parts.append(
            '<path d="%s" class="line %s" fill="none" stroke-width="2" '
            'stroke-linejoin="round"/>' % (path, css)
        )
        for index, point in enumerate(points):
            parts.append(
                '<circle cx="%.1f" cy="%.1f" r="2.5" class="%s"/>' % (x(index), y(point[key]), css)
            )
    parts.append(
        '<text x="%d" y="%d" font-size="10" class="muted">%s</text>'
        % (left, height - 12, esc(points[0]["date"]))
    )
    parts.append(
        '<text x="%d" y="%d" font-size="10" class="muted" text-anchor="end">%s</text>'
        % (width - right, height - 12, esc(points[-1]["date"]))
    )
    parts.append(
        '<text x="%d" y="%d" font-size="10" class="muted">decoded</text>'
        % (left + 6, top + 10)
    )
    parts.append(
        '<text x="%d" y="%d" font-size="10" class="muted">DXIL case</text>'
        % (left + 62, top + 10)
    )
    # Styles come from the page's CSS: this SVG is inlined in index.html.
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" '
        'viewBox="0 0 %d %d" role="img" aria-label="form coverage history">%s</svg>'
        % (width, height, width, height, "".join(parts))
    )


def html_page(data, points):
    t = data["totals"]
    style = """
:root{color-scheme:light dark;--bg:#fff;--fg:#1f2328;--muted:#59636e;--line:#d8dee4;
--accent:#0969da;--accent2:#8250df}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;
--line:#30363d;--accent:#4493f8;--accent2:#a371f7}}
body{margin:0;padding:32px 20px;background:var(--bg);color:var(--fg);
font:15px/1.55 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;max-width:940px;margin:0 auto}
h1{font-size:26px;margin:0 0 4px}h2{margin-top:36px;font-size:19px}
p.lead{color:var(--muted)}
table{border-collapse:collapse;width:100%;margin:12px 0;font-size:13px}
th,td{text-align:left;padding:5px 8px;border-bottom:1px solid var(--line)}
th{color:var(--muted);font-weight:600}td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
code{font-size:12px;background:var(--line);padding:1px 4px;border-radius:4px}
.note{font-size:13px;color:var(--muted)}ul{padding-left:20px}a{color:var(--accent)}
svg{max-width:100%;height:auto}
text{fill:var(--fg)}text.muted{fill:var(--muted)}
.track{fill:var(--line)}.done{fill:var(--accent)}.case{fill:var(--accent2)}
.line.done{stroke:var(--accent)}.line.case{stroke:var(--accent2)}
.bg{fill:var(--bg)}
"""
    out = []
    out.append("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">")
    out.append('<meta name="viewport" content="width=device-width,initial-scale=1">')
    out.append("<title>Scarlight: RDNA2 shader form coverage</title>")
    out.append("<style>%s</style></head><body>" % style)
    out.append("<h1>RDNA2 shader form coverage</h1>")
    out.append(
        '<p class="lead">Counted from source only, against a public reference list of every '
        "RDNA2 instruction form. %d of %d forms decoded · %d with a DXIL-path case · %s. "
        "Updated %s.</p>"
        % (
            t["decoded"],
            t["forms"],
            t["dxil"],
            ("%d with a direct SPIR-V case" % t["spirv"])
            if data["spirv_branch"]
            else "direct SPIR-V emitter not on this branch",
            data["date"],
        )
    )
    out.append(progress_svg(data).replace("<svg ", '<svg style="border:1px solid var(--line);border-radius:8px" '))
    out.append("<h2>Per encoding</h2>")
    out.append("<table><tr><th>Encoding</th><th class=\"num\">Forms</th>"
               "<th class=\"num\">Decoded</th><th class=\"num\">DXIL path</th>"
               "<th class=\"num\">family prefix</th><th class=\"num\">SPIR-V</th></tr>")
    for row in data["per_encoding"]:
        out.append(
            "<tr><td>%s</td><td class=\"num\">%d</td><td class=\"num\">%d</td>"
            "<td class=\"num\">%d</td><td class=\"num\">%d</td><td class=\"num\">%s</td></tr>"
            % (
                esc(row["enc"]),
                row["forms"],
                row["decoded"],
                row["case"] + row["family"],
                row["family"],
                ("%d" % row["spirv"]) if data["spirv_branch"] else "—",
            )
        )
    out.append("</table>")
    out.append("<h2>History</h2>")
    out.append(
        '<p class="note">One point per day the workflow ran; a count that goes down is a form '
        "list that grew, not code removed.</p>"
    )
    out.append(history_chart(points))
    out.append("<h2>Formats</h2>")
    out.append(
        "<table><tr><th>Set</th><th class=\"num\">Handled</th>"
        "<th class=\"num\">Mapped to a Vulkan format</th></tr>"
    )
    typed = data["formats"]["typed_buffer"] or 0
    textured = data["formats"]["texture"]
    out.append(
        "<tr><td>Typed-buffer formats (<code>buffer_load_format_*</code>)</td>"
        "<td class=\"num\">%d / %d</td><td class=\"num\">0</td></tr>" % (typed, typed)
    )
    out.append(
        "<tr><td>Texture formats with a pixel layout</td>"
        "<td class=\"num\">%d</td><td class=\"num\">0</td></tr>" % textured
    )
    out.append(
        "<tr><td>Image formats (MIMG)</td><td class=\"num\">—</td>"
        "<td class=\"num\">—</td></tr>"
    )
    out.append("</table>")
    out.append(
        '<p class="note">No Vulkan format is chosen in this tree for a console format: the stack '
        "hands formats to vkd3d-proton, and typed-buffer reads are converted in the emitted "
        "shader.</p>"
    )
    out.append("<h2>Console modules (HLE)</h2>")
    out.append(
        "<table><tr><th>Module</th><th class=\"num\">DLL names</th>"
        "<th class=\"num\">Exports</th><th class=\"num\">Stubs</th></tr>"
    )
    for row in data["hle"]["modules"]:
        out.append(
            "<tr><td>%s</td><td class=\"num\">%d</td><td class=\"num\">%d</td>"
            "<td class=\"num\">%d</td></tr>"
            % (esc(row["module"]), row["dlls"], row["exports"], row["stubs"])
        )
    out.append("</table>")
    out.append("<h2>Forms still needed</h2>")
    if data["undecoded"]:
        out.append(
            '<p class="note">Not in the decoder\'s tables at all: %s.</p>'
            % ", ".join("<code>%s</code>" % esc(row["name"]) for row in data["undecoded"])
        )
    out.append(
        "<p class=\"note\">%d decoded forms have no DXIL-path case. First %d: %s.</p>"
        % (
            data["needed_total"],
            min(data["needed_total"], NEEDED_SHOWN),
            ", ".join("<code>%s</code>" % esc(row["name"]) for row in data["needed"]),
        )
    )
    out.append("<h2>What these numbers do not mean</h2>")
    out.append(
        "<ul>"
        "<li>A decoded form with a translator case is not a form that executes correctly. "
        "Correctness is a picture, and no count on this page is one.</li>"
        "<li>The per-title offline check (the 751-program world set translated, validated and "
        "built into real pipelines) is a separate measurement and needs the title's own files; "
        "it is in the project's progress page, not here.</li>"
        "<li>The reference list is every form the hardware family defines, including ones no "
        "shader of ours contains. 100 % would still say nothing about speed or the picture.</li>"
        "<li>Counts come from string and table scans: a case is where a form's name appears, so "
        "a helper or a length can be mistaken for support in one direction, and a form refused "
        "later in translation still counts.</li>"
        "</ul>"
    )
    out.append(
        '<p class="note">Reference: Mesa\'s public ACO opcode table for GFX10 (RDNA1/RDNA2), '
        "provenance in <code>tools/progress/rdna2_isa_forms.txt</code>. The dashboard format is "
        "borrowed from the PS5 project AnyPS5, which asked the same question first.</p>"
    )
    out.append(
        '<p class="note"><a href="../PROGRESS.md">Project progress</a> · '
        '<a href="isa.json">isa.json</a> · '
        '<a href="progress-history.json">progress-history.json</a></p>'
    )
    out.append("</body></html>\n")
    return "".join(out)


# --------------------------------------------------------------------------
# History


def update_history(path, data):
    """Append today's point (or replace it) and return every point, oldest first."""
    points = []
    if os.path.exists(path):
        try:
            points = json.loads(read(path)).get("points", [])
        except ValueError:
            points = []
    point = {
        "date": data["date"],
        "forms": data["totals"]["forms"],
        "decoded": data["totals"]["decoded"],
        "dxil": data["totals"]["dxil"],
    }
    if data["spirv_branch"]:
        point["spirv"] = data["totals"]["spirv"] or 0
    points = [old for old in points if old.get("date") != point["date"]]
    points.append(point)
    points.sort(key=lambda old: old.get("date", ""))
    return {
        "note": (
            "One point per day, appended by tools/progress/shader_scan.py from source only. "
            "Counts are forms: decoded, and with a DXIL-path case. A point's date moving on to "
            "the next day re-renders every file, so only the point moves."
        ),
        "points": points,
    }


# --------------------------------------------------------------------------
# Entry point


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", default=os.path.join(ROOT, "tools/progress/rdna2_isa_forms.txt"))
    ap.add_argument(
        "--date", default=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    )
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--table", action="store_true")
    args = ap.parse_args()

    data = scan(args)
    docs = os.path.join(ROOT, "docs")
    outs = {
        os.path.join(docs, "progress/isa.json"): json.dumps(data, indent=1, sort_keys=True) + "\n",
        os.path.join(docs, "progress/isa.md"): md_section(data),
        os.path.join(docs, "progress/shaders.svg"): progress_svg(data),
    }
    history_path = os.path.join(docs, "progress/progress-history.json")
    history = update_history(history_path, data)
    outs[history_path] = json.dumps(history, indent=1) + "\n"
    outs[os.path.join(docs, "progress/index.html")] = html_page(data, history["points"])

    if args.table:
        t = data["totals"]
        print(
            "%d/%d decoded, %d DXIL, %s SPIR-V, %d typed formats, %d HLE exports"
            % (
                t["decoded"],
                t["forms"],
                t["dxil"],
                t["spirv"] if data["spirv_branch"] else "n/a",
                data["formats"]["typed_buffer"] or 0,
                data["hle"]["exports"],
            )
        )
        return 0

    if args.check:
        stale = [
            rel(path)
            for path, text in outs.items()
            if not os.path.exists(path) or read(path) != text
        ]
        if stale:
            print("stale: %s (run shader_scan.py)" % ", ".join(stale), file=sys.stderr)
            return 1
        return 0

    for path, text in outs.items():
        write(path, text)
    t = data["totals"]
    print(
        "wrote 5 files: %d/%d decoded, %d DXIL, %d family, SPIR-V %s"
        % (
            t["decoded"],
            t["forms"],
            t["dxil"],
            t["family"],
            t["spirv"] if data["spirv_branch"] else "not on this branch",
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
