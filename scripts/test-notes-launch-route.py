#!/usr/bin/env python3
"""Exercise the installed Notes entry in a caller-owned private JWM X11 session.

The caller must supply a real window manager, Xvfb and private D-Bus/profile.
Run once against each exact product build and retain JSON even on failure.
"""
from pathlib import Path
import argparse
import json
import os
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("notes-bin", "desktop-entry", "private-root", "out"):
        p.add_argument("--" + name, type=Path, required=True)
    p.add_argument("--expect", choices=("baseline", "fixed"), required=True)
    a = p.parse_args()
    root = a.private_root.resolve()
    known = {"HOME": root, "XDG_DATA_HOME": root / "data", "XDG_RUNTIME_DIR": root / "run"}
    for k, value in known.items():
        if os.environ.get(k) != str(value):
            p.error(f"requires the private root's {k}")
    if (os.environ.get("JWM_NOTES_ROUTE_PRIVATE") != "1" or os.geteuid() == 0
            or not os.environ.get("DISPLAY", "").startswith(":")
            or not os.environ.get("XAUTHORITY")
            or not os.environ.get("DBUS_SESSION_BUS_ADDRESS")
            or os.environ.get("WAYLAND_DISPLAY") or os.environ.get("WAYLAND_SOCKET")):
        p.error("private X11 with explicit Xauthority and D-Bus required")
    env = dict(os.environ, PATH=str(a.notes_bin.resolve().parent) + ":/usr/bin:/bin")
    if a.notes_bin.resolve().name != "jwm-notes":
        p.error("desktop entry must invoke the exact jwm-notes binary")

    def run(*argv):
        return subprocess.check_output([str(a) for a in argv], env=env, timeout=5,
                                       stderr=subprocess.PIPE).decode().strip()

    def wait(probe, label, seconds=5):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if (value := probe()):
                return value
            time.sleep(.025)
        raise AssertionError(label)

    def notes_windows():
        try:
            ids = run("xdotool", "search", "--onlyvisible", "--class", "org.jwm.Notes")
        except subprocess.CalledProcessError as error:
            if error.returncode != 1:
                raise
            return {}
        return {xid: run("xprop", "-id", xid, "_NET_WM_NAME") for xid in ids.splitlines()}

    def exact(name):
        owned = [xid for xid, title in notes_windows().items() if title.endswith(f'= "{name} — JWM 备忘录"')]
        if len(owned) > 1:
            raise AssertionError(f"ambiguous title: {name}")
        return owned[0] if owned else None

    def focused(xid):
        return run("xdotool", "getwindowfocus", "-f") == xid and run("xprop", "-root", "_NET_ACTIVE_WINDOW").lower().endswith(hex(int(xid)))

    def launch(*paths):
        with a.out.with_suffix(".launch.log").open("ab") as log:
            subprocess.run(["gio", "launch", str(a.desktop_entry.resolve()), *map(str, paths)],
                           env=env, stdout=log, stderr=log, timeout=5, check=True)

    def key(*keys):
        run("xdotool", "key", "--clearmodifiers", "--delay", "25", *keys)

    notebook = root / "data/jwm/notes"
    notebook.mkdir(parents=True, exist_ok=False)
    (notebook / "notebook.txt").write_text("Notebook route sentinel\n")
    filename = root / "standalone.txt"
    filename.write_bytes(b"standalone original")
    report = {"status": "failure", "expect": a.expect}
    other = None
    try:
        launch(filename)
        ext = wait(lambda: exact("standalone.txt"), "desktop entry did not open the specified file")
        run("xdotool", "windowactivate", "--sync", ext)
        wait(lambda: focused(ext), "WM did not focus external editor")
        key("Ctrl+End", "z")
        wait(lambda: exact("• standalone.txt") == ext, "standalone edit did not become dirty")
        assert filename.read_bytes() == b"standalone original"
        assert len(notes_windows()) == 1
        # Activate from a different application, not from a Notes window.
        other = subprocess.Popen(["xmessage", "-name", "route-focus-control", "-title", "route-focus-control",
                                  "Separate X11 focus control"],
                                 env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def control_xid():
            try:
                return run("xdotool", "search", "--onlyvisible", "--name", "route-focus-control").splitlines()[0]
            except (subprocess.CalledProcessError, IndexError):
                return None
        control = wait(control_xid, "separate focus control did not map")
        run("xdotool", "windowactivate", "--sync", control)
        wait(lambda: focused(control), "separate focus control did not become active")
        launch()
        if a.expect == "baseline":
            wait(lambda: focused(ext), "baseline did not actually focus the standalone editor")
            until = time.monotonic() + 2
            while time.monotonic() < until:
                assert notes_windows().keys() == {ext}, "baseline produced a managed notebook"
                time.sleep(.05)
            report.update(status="baseline-focus-regression-reproduced", standalone=ext, focus_control=control)
            run("import", "-window", "root", a.out.with_suffix(".png"))
            return 0

        managed = wait(lambda: exact("Notebook route sentinel"), "no managed notebook from no-file entry")
        assert len(notes_windows()) == 2 and managed != ext
        wait(lambda: focused(managed), "new managed notebook did not get actual focus")
        run("xdotool", "windowactivate", "--sync", control)
        wait(lambda: focused(control), "separate focus control did not become active")
        launch()
        wait(lambda: focused(managed), "activation did not return to the existing managed notebook")
        assert set(notes_windows()) == {ext, managed}
        assert exact("• standalone.txt") == ext and filename.read_bytes() == b"standalone original"

        # Explicit file activation must still find the original dirty window.
        launch(filename)
        wait(lambda: focused(ext), "direct file activation did not target its owned window")
        key("Ctrl+s")
        wait(lambda: exact("standalone.txt") == ext, "external editor did not acknowledge save")
        assert filename.read_bytes() == b"standalone originalz"
        assert set(notes_windows()) == {ext, managed}
        report.update(status="passed", standalone=ext, managed=managed, focus_control=control)
        run("import", "-window", "root", a.out.with_suffix(".png"))
        return 0
    except Exception as error:
        report["error"] = str(error)
        try:
            report["windows"] = notes_windows()
            report["focus"] = run("xdotool", "getwindowfocus", "-f")
        except (OSError, subprocess.SubprocessError):
            pass
        raise
    finally:
        if other is not None and other.poll() is None:
            other.terminate()
            try:
                other.wait(2)
            except subprocess.TimeoutExpired:
                other.kill()
                other.wait(2)
        a.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
