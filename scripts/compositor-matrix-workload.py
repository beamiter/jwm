#!/usr/bin/env python3
"""Drive a bounded, named workload in a dedicated JWM benchmark session."""
import argparse
import json
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("kind", choices=["animation", "tag-switch", "window-move"])
parser.add_argument("--tool", default="jwm-tool")
parser.add_argument("--window", help="X11 fixture window id, required for window-move")
args = parser.parse_args()
if args.kind == "window-move" and not args.window:
    parser.error("window-move requires --window")


def message(name, values):
    subprocess.run([args.tool, "msg", name, "--args", json.dumps(values)], check=True,
                   stdout=subprocess.DEVNULL, timeout=10)


if args.kind == "window-move":
    message("setlayout", {"layout": "float"})
for index in range(7200):  # also exits without a parent after one hour
    started = time.monotonic()
    if args.kind == "animation":
        message("toggle_overview", {})
        interval = 0.5
    elif args.kind == "tag-switch":
        message("view", {"tag": 1 if index % 2 == 0 else 2})
        interval = 0.5
    else:
        x = 100 + (index % 60) * 4
        subprocess.run(["xdotool", "windowmove", args.window, str(x), "160"], check=True, timeout=5)
        interval = 1 / 30
    time.sleep(max(0, interval - (time.monotonic() - started)))
