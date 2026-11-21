#!/usr/bin/env python3
"""Private X11 witness for Workscene launch vs app-class reuse."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("tool", "root", "exact-notes", "desktop", "out"):
        p.add_argument("--" + name, type=Path, required=True)
    p.add_argument("--candidate", action="store_true")
    p.add_argument("--prepare", action="store_true")
    a = p.parse_args()
    root = a.root.resolve()
    assert os.environ.get("JWM_WORKSCENE_PRIVATE") == "1" and os.geteuid() != 0
    assert os.environ.get("DISPLAY", "").startswith(":") and os.environ.get("XAUTHORITY")
    assert os.environ.get("DBUS_SESSION_BUS_ADDRESS")
    assert os.environ.get("HOME") == str(root)
    assert os.environ.get("XDG_DATA_HOME") == str(root / "data")
    assert os.environ.get("XDG_RUNTIME_DIR") == str(root / "run")
    assert not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("WAYLAND_SOCKET"))
    a.out.mkdir(exist_ok=True)
    witness = root / "notes-launches"

    if a.prepare:
        data = root / "data/applications"
        data.mkdir(exist_ok=False)
        bindir = root / "bin"
        bindir.mkdir()
        source = a.desktop.read_bytes()
        assert a.desktop.name == "org.jwm.Notes.desktop"
        assert b"Name=JWM Notes\n" in source and b"Exec=jwm-notes -- %F\n" in source
        (data / a.desktop.name).write_bytes(source)
        wrapper = bindir / "jwm-notes"
        wrapper.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> " +
                           shlex.quote(str(witness)) + "\nexec " +
                           shlex.quote(str(a.exact_notes.resolve())) + ' "$@"\n')
        wrapper.chmod(0o700)
        config = root / "config/jwm/config_x11.toml"
        with config.open("a") as handle:
            handle.write('\n[[workscenes]]\nname = "Identity Witness"\n'
                         'applications = ["JWM Notes"]\n'
                         '[[workscenes.outputs]]\noutput = ""\ntag = 1\n')
        return

    def run(*argv):
        return subprocess.check_output(list(map(str, argv)), text=True,
                                       stderr=subprocess.PIPE, timeout=8).strip()

    def key(*names):
        run("xdotool", "key", "--clearmodifiers", "--delay", "55", *names)

    def wait(probe, problem, timeout=8):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if (value := probe()):
                return value
            time.sleep(.04)
        raise AssertionError(problem)

    def wait_quiet(probe, default, label, interval=1.15):
        end = time.monotonic() + interval
        while time.monotonic() < end:
            assert probe() == default, label
            time.sleep(.07)

    def ui():
        r = json.loads(run(a.tool, "msg", "get_system_ui", "--raw"))
        assert r.get("success", True), r
        return r.get("data", r)["kind"]

    def xids(prop, value):
        try:
            return run("xdotool", "search", "--onlyvisible", prop,
                       "^" + re.escape(value) + "$").splitlines()
        except subprocess.CalledProcessError as error:
            if error.returncode != 1:
                raise
            return []

    def notes():
        return xids("--class", "org.jwm.Notes")

    def calls():
        return witness.read_text().splitlines() if witness.exists() else []

    def capture(name):
        run("import", "-window", "root", a.out / (name + ".png"))

    def open_scene(label):
        assert ui() is None, ui()
        key("Alt+r")
        wait(lambda: ui() == "launcher", f"{label}: launcher did not open")
        run("xdotool", "type", "--clearmodifiers", "--delay", "45", "scenes: Identity Witness")
        key("Return")
        wait(lambda: ui() == "workscene_preview", f"{label}: scene did not open")
        time.sleep(.8)
        capture(label)

    def action(label):
        assert ui() == "workscene_preview", ui()
        key("Down", "Return")
        time.sleep(.3)
        capture(label)

    def dismiss():
        key("Escape")
        wait(lambda: ui() is None, "scene preview did not dismiss")

    def close(xid):
        run("xdotool", "windowactivate", "--sync", xid)
        key("Alt+Shift+c")
        wait(lambda: not notes(), "Notes did not close")

    report = {"status": "failed", "candidate": a.candidate, "cases": []}
    children = []
    try:
        decoy = subprocess.Popen(["xmessage", "-name", "org.unrelated.Notes", "-title",
                                  "Independent Notes lookalike", "This is not JWM Notes"])
        children.append(decoy)
        decoy_id, = wait(lambda: xids("--classname", "org.unrelated.Notes"), "decoy did not map")
        assert not calls() and not notes()
        run("xdotool", "windowactivate", "--sync", decoy_id)
        # Warm the background catalog without explicitly picking from it.
        key("Alt+r")
        wait(lambda: ui() == "launcher", "catalog warmup launcher missing")
        time.sleep(1)
        key("Escape")
        wait(lambda: ui() is None, "catalog warmup launcher did not dismiss")

        open_scene("decoy-only")
        assert not calls() and not notes()
        action("decoy-only-action")
        owned, = wait(notes, "missing-app scene action did not start real Notes")
        wait(lambda: calls() == ["--"], "exactly one closed-app dispatch expected")
        report["cases"].append({"case": "decoy only", "calls": calls(),
                                "app": run("xprop", "-id", owned, "WM_CLASS")})
        dismiss()

        open_scene("existing")
        action("existing-action")
        count = 1 if a.candidate else 2
        wait_quiet(calls, ["--"] * count, "existing real Notes incorrectly launched by scene")
        assert notes() == [owned]
        report["cases"].append({"case": "existing Notes", "calls": calls(), "window": owned})
        dismiss()
        close(owned)

        open_scene("stale-preview-before-external-start")
        assert not notes() and calls() == ["--"] * count
        # Not through the search wrapper: this is an outside launch after preview.
        child = subprocess.Popen([a.exact_notes.resolve(), "--"])
        children.append(child)
        stale, = wait(notes, "externally launched Notes did not map")
        assert calls() == ["--"] * count and ui() == "workscene_preview"
        action("stale-preview-action")
        count += 0 if a.candidate else 1
        wait_quiet(calls, ["--"] * count, "existing Notes incorrectly launched from stale action")
        assert notes() == [stale]
        report["cases"].append({"case": "opened after preview", "calls": calls(), "window": stale})
        dismiss()
        close(stale)
        report["status"] = "passed" if a.candidate else "baseline duplicate dispatch reproduced"
    except Exception as exc:
        report["error"] = str(exc)
        try:
            capture("error")
        except Exception:
            pass
        raise
    finally:
        report["launch_witness"] = calls()
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)
        (a.out / "result.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
