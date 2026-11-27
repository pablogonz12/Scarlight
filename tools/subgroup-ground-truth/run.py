#!/usr/bin/env python3
"""Capture and decode cases using an explicitly supplied local RADV build.

This requires an instrumented Mesa 25.3 RADV that writes HARNESS_ISA_PATH;
it does not translate raw ISA or claim semantic equivalence.
"""
import argparse
import csv
import fcntl
import json
import os
import pathlib
import subprocess
import time


def invoke(args, env):
    return subprocess.run(args, env=env, text=True, capture_output=True, timeout=60)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--icd", type=pathlib.Path, required=True)
    parser.add_argument("--vkpipe", type=pathlib.Path, required=True)
    parser.add_argument("--decoder", type=pathlib.Path, required=True)
    parser.add_argument("--cases", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    env = os.environ.copy()
    env.update(VK_DRIVER_FILES=str(args.icd.resolve()), RADV_FORCE_FAMILY="navi21",
               RADV_DEBUG="shaders,nocache", MESA_SHADER_CACHE_DISABLE="true",
               VK_LOADER_LAYERS_DISABLE="~implicit~", VKPIPE_TRACE="1")
    args.output.mkdir(parents=True, exist_ok=True)
    rows = []
    for case in json.loads((args.cases / "manifest.json").read_text()):
        name = case["name"]
        code = args.output / (name + ".isa")
        if code.exists():
            raise SystemExit(f"refusing to overwrite {code}")
        env["HARNESS_ISA_PATH"] = str(code.resolve())
        with open("/tmp/vk-jobs.lock", "a") as gate:
            pending = pathlib.Path("/tmp/vk-jobs.pending")
            if pending.exists() and time.time() - pending.stat().st_mtime < 600:
                raise SystemExit("Vulkan boot gate pending; stopping")
            try:
                fcntl.flock(gate, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                raise SystemExit("Vulkan boot gate busy; stopping")
            result = invoke([str(args.vkpipe), str(args.cases / case["spirv"])], env)
        (args.output / (name + ".log")).write_text(result.stdout + result.stderr)
        if "TRACE physdev Null hardware (RADV NAVI21)" not in result.stderr:
            raise SystemExit("Null Navi 21 verification failed; stopping")
        status = "capture-failed"
        detail = "pipeline or capture failed"
        if result.returncode == 0 and code.exists() and code.stat().st_size:
            decoded = invoke([str(args.decoder), str(code)], env)
            status = "decoded" if decoded.returncode == 0 else "decode-failed"
            detail = (decoded.stdout + decoded.stderr).strip().replace("\n", "; ")
        rows.append([name, status, detail, "not attempted: console ABI required"])
    with (args.output / "report.tsv").open("w", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["case", "decode", "detail", "translate"])
        writer.writerows(rows)
    print(f"reported {len(rows)} cases")


if __name__ == "__main__":
    main()
