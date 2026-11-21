#!/usr/bin/env python3
"""Managed Notes active-filter refresh with real keys, pointer and X11 pixels."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--notes", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--arm", choices=["a", "b"], required=True)
    a = p.parse_args()
    assert os.environ.get("JWM_REFRESH_PRIVATE") == "1" and os.geteuid() != 0
    assert os.environ.get("XAUTHORITY") and os.environ.get("DISPLAY", "").startswith(":")
    assert not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("WAYLAND_SOCKET"))
    root = Path(os.environ["HOME"])
    assert Path(os.environ["XDG_DATA_HOME"]) == root / "data"
    a.out.mkdir(mode=0o700)
    d = root / "data/jwm/notes"
    d.mkdir(parents=True)
    current, old, hidden = (d / name for name in ("current.txt", "old.txt", "hidden.txt"))
    original = b"Needle Current\nThe editor stays on this note.\n"
    old.write_text("Needle Original\n")
    hidden.write_text("Excluded fixture\n")
    current.write_bytes(original)
    now = time.time_ns()
    os.utime(current, ns=(now, now + 1_000_000_000))
    (d / "decoy.txt").write_text("Excluded decoy\n")

    def run(*args):
        return subprocess.check_output([str(x) for x in args], text=True,
                                       stderr=subprocess.PIPE, timeout=7).strip()

    def key(*names):
        run("xdotool", "key", "--clearmodifiers", "--delay", "50", *names)

    def wait(probe, problem, seconds=10):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if (v := probe()):
                return v
            time.sleep(.04)
        raise AssertionError(problem)

    def find_window():
        try:
            return run("xdotool", "search", "--onlyvisible", "--pid", proc.pid).splitlines()[0]
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                return None
            raise

    report = {"arm": a.arm, "status": "failed", "view": "private native X11", "checks": {}}
    with (a.out / "notes.log").open("w") as log:
        proc = subprocess.Popen([a.notes.resolve()], stdout=log, stderr=log)
        try:
            xid = wait(find_window, "Notes window unavailable", 20)
            def title():
                return run("xdotool", "getwindowname", xid)

            def seen(label):
                png = a.out / f"{label}.png"
                crop = a.out / f"{label}-sidebar.png"
                run("import", "-window", xid, png)
                run("convert", png, "-crop", "220x480+8+119", "+repage", "-resize", "220%", "-colorspace", "gray", "-threshold", "55%", "-negate", crop)
                words = run("tesseract", crop, "stdout", "--psm", "6")
                report["checks"][label] = words
                return words.lower()

            initial = "Needle Current — JWM 备忘录"
            wait(lambda: title() == initial, "wrong initial managed note")
            run("xdotool", "windowfocus", "--sync", xid)
            key("ctrl+f")
            run("xdotool", "type", "--clearmodifiers", "--delay", "30", "needle")
            time.sleep(.70)
            initial_words = seen("before")
            assert "needle current" in initial_words and "needle original" in initial_words, initial_words
            assert "excluded" not in initial_words, initial_words
            hidden.write_text("Needle Inplace\n") # external in-place editor
            # Atomic writes with realistic non-.txt temporary file then rename.
            temp = d / "new.tmp"
            temp.write_text("Needle Atomic\n")
            atomic = d / "atomic.txt"
            temp.replace(atomic)
            renamed = d / "new-name.txt"
            old.rename(renamed)
            renamed.write_text("Needle Renamed\n")
            # Known recency order, separated without sleeps or wall-clock races.
            for i, path in enumerate([renamed, hidden, atomic], 1):
                os.utime(path, ns=(now, now + (2 + i) * 1_000_000_000))
            unchanged = seen("external-before-f5")
            assert "needle original" in unchanged and "needle atomic" not in unchanged, unchanged
            key("F5")
            time.sleep(.35)
            refreshed = seen("after-f5")
            assert title() == initial and current.read_bytes() == original
            if a.arm == "a":
                assert "needle original" in refreshed and "needle atomic" not in refreshed, refreshed
                report["status"] = "baseline unchanged filter remained stale"
            else:
                tokens = ("needle atomic", "needle inplace", "needle renamed", "needle current")
                assert all(s in refreshed for s in tokens), refreshed
                indices = [refreshed.index(s) for s in tokens]
                assert indices == sorted(indices), refreshed
                assert "needle original" not in refreshed and "excluded" not in refreshed, refreshed
                atomic.write_text("Needle Clicked\n")
                run("xdotool", "mousemove", "--window", xid, "423", "72", "click", "1")
                time.sleep(.35)
                via_button = seen("after-toolbar-click")
                assert "needle clicked" in via_button and "needle atomic" not in via_button, via_button
                assert title() == initial and current.read_bytes() == original
                key("ctrl+f", "Down", "Return")
                wait(lambda: title() == "Needle Renamed — JWM 备忘录"
                             or title() == "Needle Clicked — JWM 备忘录"
                             or title() == "Needle Inplace — JWM 备忘录",
                     "refreshed result was not actionable")
                report["selected_after_refresh"] = title()
                report["status"] = "passed"
        except Exception as error:
            report["error"] = repr(error)
            raise
        finally:
            (a.out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=2)


if __name__ == "__main__":
    main()
