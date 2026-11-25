#!/usr/bin/env python3
"""Record the launcher demo that goes in the README (docs/img/launcher.gif).

Nothing about the machine that runs this ends up in the recording: the launcher
is started with HOME, XDG_* and the profile pointed at a fresh temporary
directory, and the profile names a fake build and a fake title executable
(empty files). The recording shows our own interface only - no game data, no
screencast of a title, no personal path.

    python3 tools/scarlight-launcher/demo/record.py /tmp/launcher.cast
    agg --font-size 15 --speed 1.2 --theme asciinema /tmp/launcher.cast docs/img/launcher.gif

Needs a terminal driver (pty, in the standard library) and, for the GIF,
asciinema's `agg`. It also needs `fzf` or `whiptail` for the menu front end; fzf
is what the shipped recording used.
"""

import codecs
import fcntl
import json
import os
import pty
import select
import struct
import sys
import tempfile
import termios
import time

COLS, ROWS = 100, 30
DOWN = "\x1b[B"

# What a user types, and how long to let the screen settle before the next key.
STEPS = [
    ("scarlight --profile demo --check\r", 2.4),  # the system check
    ("\x0c", 0.5),  # clear
    ("scarlight --profile demo\r", 2.0),  # the main menu
    (DOWN * 2, 1.3),  # highlight Presets
    ("\r", 1.8),
    ("\x1b", 1.3),
    (DOWN * 6, 1.3),  # highlight Quit
    ("\r", 1.0),
    ("exit\r", 0.4),
]

PROFILE = """PRESET='Performance'
TARGET='normal'
RT='off'
MESA='mesa-cache radv-wave32'
LOG='info'
WATCHDOG='on'
BACKEND='d3d12'
PROTON=''
WINE='wine'
PORT_EXE='{root}/build/victory.exe'
GAME_EXE='{root}/data/title.exe'
GAME_DIR='{root}/data'
DLLDIR='{root}/build'
PREFIX='{root}/prefix'
"""


def fake_machine(root):
    """A build folder, a title executable and a profile, all empty placeholders."""
    for folder in ("build", "data", "prefix", "home", "config/scarlight"):
        os.makedirs(os.path.join(root, folder), exist_ok=True)
    for name in ("victory.exe", "d3d12.dll", "d3d12core.dll", "dxil.dll", "dxcompiler.dll"):
        open(os.path.join(root, "build", name), "w").close()
    open(os.path.join(root, "data", "title.exe"), "w").close()
    with open(os.path.join(root, "config", "scarlight", "demo.env"), "w") as f:
        f.write(PROFILE.format(root=root))


MARKER_PREFIX = "\x1b[?2026"  # synchronized-output markers, "[?2026h" and "[?2026l"


def strip_markers(text, carry):
    """Drop synchronized-output markers; keep a marker split across reads.

    agg (the cast-to-GIF renderer) drops whatever fzf draws between these
    markers, so a menu recorded as-is renders as empty frames. They only say
    "this redraw is atomic"; dropping them changes nothing a viewer sees.
    """
    text = carry + text
    for marker in (MARKER_PREFIX + "h", MARKER_PREFIX + "l"):
        text = text.replace(marker, "")
    for cut in range(min(len(MARKER_PREFIX), len(text)), 0, -1):
        if text.endswith(MARKER_PREFIX[:cut]):
            return text[:-cut], text[-cut:]
    return text, ""


def record(path, root, launcher_dir):
    env = dict(os.environ)
    env.update(
        HOME=os.path.join(root, "home"),
        XDG_CONFIG_HOME=os.path.join(root, "config"),
        # The launcher prefers SCARLIGHT_CONFIG_DIR over XDG; an ambient one
        # would put a real profile in the recording.
        SCARLIGHT_CONFIG_DIR=os.path.join(root, "config", "scarlight"),
        XDG_STATE_HOME=os.path.join(root, "home", "state"),
        XDG_DATA_HOME=os.path.join(root, "home", "share"),
        PATH=launcher_dir + os.pathsep + env.get("PATH", ""),
        TERM="xterm-256color",
        PS1="\u276f ",
        SCARLIGHT_UI=env.get("SCARLIGHT_UI", "fzf"),
    )
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe("bash", ["bash", "--norc", "--noprofile", "-i"], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    events, start = [], time.monotonic()
    # A 700-byte read can end inside a UTF-8 sequence or inside a marker.
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    carry = ""

    def pump(seconds):
        nonlocal carry
        deadline = time.monotonic() + seconds
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return True
            ready, _, _ = select.select([fd], [], [], min(left, 0.05))
            if not ready:
                continue
            try:
                data = os.read(fd, 700)
            except OSError:
                return False
            if not data:
                return False
            text, carry = strip_markers(decoder.decode(data), carry)
            if text:
                events.append([round(time.monotonic() - start, 4), "o", text])

    for keys, wait in STEPS:
        if not pump(wait):
            break
        if keys:
            os.write(fd, keys.encode())
    pump(0.6)
    text, _ = strip_markers(decoder.decode(b"", True), carry)
    if text:
        events.append([round(time.monotonic() - start, 4), "o", text])

    with open(path, "w") as f:
        f.write(json.dumps({"version": 2, "width": COLS, "height": ROWS,
                            "env": {"SHELL": "/bin/bash", "TERM": "xterm-256color"}}) + "\n")
        for event in events:
            f.write(json.dumps(event) + "\n")
    return len(events), events[-1][0] if events else 0.0


def main(argv):
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: record.py <out.cast>", file=sys.stderr)
        return 2
    launcher_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with tempfile.TemporaryDirectory(prefix="scarlight-demo-") as root:
        fake_machine(root)
        count, duration = record(argv[1], root, launcher_dir)
    print(f"{argv[1]}: {count} events, {duration:.1f} s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
