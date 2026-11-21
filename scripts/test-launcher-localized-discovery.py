#!/usr/bin/env python3
"""Exercise installed desktop entries through actual Alt+R keys on private X11."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time

APPS = {"notes": "org.jwm.Notes", "viewer": "org.jwm.ImageViewer"}
CASES = [
    ("notes-en", "JWM Notes", "notes"),
    ("viewer-en", "JWM Image Viewer", "viewer"),
    ("notes-cn", "备忘录", "notes"),
    ("notes-cn-name", "JWM 备忘录", "notes"),
    ("viewer-cn", "图片查看器", "viewer"),
    ("viewer-cn-keyword", "照片", "viewer"),
]
UNICODE_CONTROL = "主题"


def run(*args):
    return subprocess.check_output(list(map(str, args)), text=True, stderr=subprocess.PIPE,
                                   timeout=8).strip()


def char_keys(text):
    return ["space" if ch == " " else ch if ord(ch) < 128 else f"U{ord(ch):04X}" for ch in text]


def keys(*argv):
    run("xdotool", "key", "--clearmodifiers", "--delay", "45", *argv)


def wait(probe, label, seconds=8):
    expires = time.monotonic() + seconds
    last = None
    while time.monotonic() < expires:
        try:
            if (value := probe()):
                return value
        except (subprocess.CalledProcessError, json.JSONDecodeError) as exc:
            last = str(exc)
        time.sleep(.04)
    raise AssertionError(f"{label}, last={last}")


def xids(*spec):
    try:
        return run("xdotool", "search", "--onlyvisible", *spec).splitlines()
    except subprocess.CalledProcessError as exc:
        if exc.returncode != 1:
            raise
        return []


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--phase", required=True, choices=("keymap", "run"))
    p.add_argument("--private-root", type=Path, required=True)
    p.add_argument("--tool", type=Path)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--expect", required=True, choices=("baseline", "fixed"))
    a = p.parse_args()
    root = a.private_root.resolve()
    for key, tail in {"HOME": "", "XDG_RUNTIME_DIR": "run", "XDG_DATA_HOME": "data"}.items():
        if os.environ.get(key) != str(root / tail):
            p.error(f"{key} is not bound to the private root")
    if (os.geteuid() == 0 or os.environ.get("JWM_LAUNCHER_ALIAS_PRIVATE") != "1"
            or not os.environ.get("DISPLAY", "").startswith(":") or not os.environ.get("XAUTHORITY")
            or not os.environ.get("DBUS_SESSION_BUS_ADDRESS")
            or any(os.environ.get(key) for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET"))):
        p.error("requires the caller's private Xvfb Xauthority, profile and D-Bus")
    a.out.mkdir(parents=True, exist_ok=True)

    if a.phase == "keymap":
        before = run("xmodmap", "-pke")
        free = re.findall(r"^keycode\s+(\d+)\s*=\s*$", before, re.MULTILINE)
        points = sorted({ord(ch) for txt in [UNICODE_CONTROL, *(row[1] for row in CASES)]
                         for ch in txt if ord(ch) > 127})
        assert len(free) >= len(points), (len(free), len(points))
        exprs = [f"keycode {number} = U{cp:04X}" for number, cp in zip(free, points)]
        run("xmodmap", *(arg for entry in exprs for arg in ("-e", entry)))
        after = run("xmodmap", "-pke")
        for number, cp in zip(free, points):
            assert re.search(rf"^keycode\s+{number}\s*= U{cp:04X}\b", after, re.MULTILINE), (number, cp)
        (a.out / "keymap.txt").write_text(after + "\n")
        return

    assert a.tool is not None and a.tool.is_file()

    def ui():
        reply = json.loads(run(a.tool, "msg", "get_system_ui", "--raw"))
        assert reply.get("success", True), reply
        return reply.get("data", reply)

    def app_windows():
        return {k: xids("--class", "^" + re.escape(v) + "$") for k, v in APPS.items()}

    def focused(xid):
        return run("xprop", "-root", "_NET_ACTIVE_WINDOW").lower().endswith(hex(int(xid)))

    def capture(name):
        run("import", "-window", "root", a.out / (name + ".png"))

    def open_query(query):
        assert not ui()["active"], ui()
        keys("Alt+r")
        wait(lambda: ui().get("kind") == "launcher", "Alt+R did not open the real launcher")
        keys(*char_keys(query))

    report = {"expect": a.expect, "results": [], "status": "failed"}
    control = subprocess.Popen(["xmessage", "-name", "alias-focus-control", "-title",
                                "alias-focus-control", "Independent X11 focus control"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        control_id, = wait(lambda: xids("--name", "^alias-focus-control$"),
                           "separate X11 window did not map")
        run("xdotool", "windowactivate", "--sync", control_id)
        wait(lambda: focused(control_id), "control window was not focused")
        open_query(UNICODE_CONTROL)
        capture("unicode-control")
        keys("Return")
        wait(lambda: ui().get("kind") == "theme_picker",
             "the pre-existing Chinese control failed; no result can be attributed to indexing")
        keys("Escape")
        wait(lambda: not ui()["active"], "the Chinese control picker did not close")
        report["unicode_control"] = "theme_picker"
        usage = root / "data/jwm/launcher-usage"

        for label, query, expected in CASES:
            assert not any(app_windows().values()), "test requires both applications closed"
            run("xdotool", "windowactivate", "--sync", control_id)
            wait(lambda: focused(control_id), f"{label}: independent client was not focused")
            open_query(query)
            # Let the first async catalog complete; subsequent queries reuse it.
            time.sleep(.7)
            capture(label + "-search")
            before = usage.read_bytes() if usage.exists() else b""
            keys("Return")
            if a.expect == "baseline" and "-cn" in label:
                deadline = time.monotonic() + 1.2
                while time.monotonic() < deadline:
                    assert not any(app_windows().values()), f"{label}: master already launched an app"
                    assert ui().get("kind") == "launcher" and focused(control_id), label
                    assert (usage.read_bytes() if usage.exists() else b"") == before, label
                    time.sleep(.08)
                report["results"].append({"query": query, "outcome": "no installed-app launch"})
                keys("Escape")
                wait(lambda: not ui()["active"], f"{label}: launcher failed to dismiss")
                continue
            target_id, = wait(lambda: app_windows()[expected], f"{label}: expected app did not map")
            wait(lambda: not ui()["active"] and focused(target_id), f"{label}: window did not get focus")
            assert not app_windows()["notes" if expected == "viewer" else "viewer"]
            class_property = run("xprop", "-id", target_id, "WM_CLASS", "_NET_WM_NAME")
            label_expected = "JWM Notes" if expected == "notes" else "JWM Image Viewer"
            after = usage.read_bytes()
            assert after != before and any(line.endswith(f" {label_expected}") for line in
                                           after.decode().splitlines()), (before, after)
            report["results"].append({"query": query, "outcome": "launched",
                                      "recorded_usage_id": label_expected, "window": class_property})
            capture(label + "-opened")
            # Ask the actual app to close from its focused editor/viewer.
            keys("Alt+Shift+c")
            wait(lambda: not any(app_windows().values()), f"{label}: app did not close")
        report["status"] = "passed"
    except Exception as exc:
        report["error"] = str(exc)
        try:
            report["final_ui"] = ui()
            report["final_windows"] = app_windows()
            capture("error")
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            pass
        raise
    finally:
        if control.poll() is None:
            control.terminate()
            try:
                control.wait(timeout=2)
            except subprocess.TimeoutExpired:
                control.kill()
                control.wait(timeout=2)
        (a.out / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
