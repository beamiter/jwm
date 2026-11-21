#!/usr/bin/env python3
"""Compare managed autosave acknowledgements using real X11 Notes input."""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--notes", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--count", type=int, required=True, choices=[40, 1000])
    p.add_argument("--arm", required=True, choices=["a", "b"])
    a = p.parse_args()
    assert os.environ.get("JWM_AUTOSAVE_PRIVATE") == "1" and os.geteuid() != 0
    assert os.environ.get("DISPLAY", "").startswith(":") and os.environ.get("XAUTHORITY")
    assert not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("WAYLAND_SOCKET"))
    root = Path(os.environ["HOME"])
    assert Path(os.environ["XDG_DATA_HOME"]) == root / "data"
    a.out.mkdir(mode=0o700)
    notebook = root / "data/jwm/notes"
    notebook.mkdir(parents=True, mode=0o700)
    title = "Needle autosave witness"
    original = (title + "\nKeep the current document open during timing.\n").encode()
    main_note = notebook / "witness.txt"
    body = "普通笔记示例：含中文、English 与 Ω，混合查找。\n".encode() * 420
    for i in range(a.count - 1):
        prefix = "Needle" if i % 4 == 0 else "Unmatched"
        (notebook / f"seed-{i:04}.txt").write_bytes(f"{prefix} fixture {i:04}\n".encode() + body)
    main_note.write_bytes(original)
    # Deterministically open the witness, not an arbitrarily ordered seed.
    now = time.time_ns()
    os.utime(main_note, ns=(now + 1_000_000_000, now + 1_000_000_000))

    def run(*argv):
        return subprocess.check_output([str(x) for x in argv], text=True,
                                       stderr=subprocess.PIPE, timeout=7).strip()

    def wait(probe, problem, seconds=10):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if (v := probe()):
                return v
            time.sleep(.008)
        raise AssertionError(problem)

    def search_window():
        try:
            return run("xdotool", "search", "--onlyvisible", "--pid", child.pid).splitlines()[0]
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                return None
            raise

    def shot(name):
        run("import", "-window", xid, a.out / (name + ".png"))

    report = {"arm": a.arm, "corpus_count": a.count, "body_bytes": len(body),
              "status": "failed", "samples": [], "backend": "real app X11 on private Xvfb",
              "debounce_ms": 700, "physical_acceptance": False}
    with (a.out / "notes.log").open("w") as log:
        child = subprocess.Popen([a.notes.resolve()], stdout=log, stderr=log)
        try:
            xid = wait(search_window, "Notes window not visible", 20)
            def name():
                return run("xdotool", "getwindowname", xid)

            wait(lambda: name() == title + " — JWM 备忘录", "wrong initial managed note")
            run("xdotool", "windowfocus", "--sync", xid)
            run("xdotool", "key", "--clearmodifiers", "ctrl+f")
            run("xdotool", "type", "--clearmodifiers", "--delay", "30", "needle")
            time.sleep(1)
            shot("settled-filter")
            run("xdotool", "mousemove", "--window", xid, "390", "145", "click", "1")
            run("xdotool", "key", "--clearmodifiers", "ctrl+End")
            clean_name = name()
            assert clean_name == title + " — JWM 备忘录"
            for i in range(8):
                suffix = b"z" * (i + 1)
                expected = original + suffix
                before_key = time.monotonic_ns()
                run("xdotool", "key", "--delay", "0", "--clearmodifiers", "z")
                wait(lambda: name() == "• " + clean_name, "key did not dirty note", 2)
                wrote = clean = None
                until = time.monotonic() + 3
                while time.monotonic() < until and not (wrote and clean):
                    if wrote is None and main_note.read_bytes() == expected:
                        wrote = time.monotonic_ns()
                    if clean is None and name() == clean_name:
                        clean = time.monotonic_ns()
                        assert main_note.read_bytes() == expected, "save acknowledgement preceded exact bytes"
                    if not (wrote and clean):
                        time.sleep(.003)
                assert wrote and clean, "automatic save or clean title timed out"
                assert i == 0 or report["samples"][-1]["saved_bytes"] == len(expected) - 1
                report["samples"].append({"sample": i, "warmup": i == 0, "saved_bytes": len(expected),
                                          "key_to_bytes_ms": (wrote - before_key) / 1e6,
                                          "key_to_clean_title_ms": (clean - before_key) / 1e6,
                                          "bytes_to_clean_title_ms": max(0, clean - wrote) / 1e6})
                time.sleep(.35)
            shot("after-autosave")
            assert main_note.read_bytes() == original + b"z" * 8
            # Check the active filter persisted; show fresh rows after the
            # last worker completion and demonstrate it can still open one.
            run("xdotool", "key", "--clearmodifiers", "ctrl+f", "Down", "Down", "Return")
            wait(lambda: re.match(r"Needle fixture \d+ — JWM 备忘录$", name()),
                 "active filter could not open a returned fixture note")
            shot("opened-filter-result")
            for k in ("key_to_bytes_ms", "key_to_clean_title_ms", "bytes_to_clean_title_ms"):
                report["median_" + k] = statistics.median(row[k] for row in report["samples"][1:])
            report["status"] = "passed"
        except Exception as error:
            report["error"] = repr(error)
            raise
        finally:
            (a.out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)


if __name__ == "__main__":
    main()
