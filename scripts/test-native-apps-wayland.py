#!/usr/bin/env python3
"""Exercise native app input and pixels with no X server on private headless Sway.

Requires sway, wtype, grim, dbus-run-session and Pillow. All configuration,
clipboard and app data belong to the private compositor and session bus.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/native-apps/debug")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--scale", type=int, choices=(1, 2), default=1)
    parser.add_argument("--under-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not args.under_bus:
        return subprocess.call(["dbus-run-session", "--", sys.executable, __file__,
                                *sys.argv[1:], "--under-bus"])
    from PIL import Image, ImageChops

    for tool in ("sway", "swaymsg", "wtype", "grim"):
        if not shutil.which(tool):
            raise SystemExit(f"Required tool unavailable: {tool}")
    with tempfile.TemporaryDirectory(prefix="jwm-native-wayland-") as temporary:
        work = Path(temporary)
        runtime = work / "runtime"
        runtime.mkdir(mode=0o700)
        evidence = args.evidence_dir or work / "evidence"
        evidence.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ, HOME=str(work), XDG_RUNTIME_DIR=str(runtime),
                   XDG_DATA_HOME=str(work / "data"), XDG_CONFIG_HOME=str(work / "config"),
                   XDG_CACHE_HOME=str(work / "cache"), GIO_USE_VFS="local",
                   WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_LIBINPUT_NO_DEVICES="1")
        for name in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "SWAYSOCK"):
            env.pop(name, None)
        config = work / "sway.conf"
        config.write_text(f"output HEADLESS-1 resolution {1280 * args.scale}x{900 * args.scale} scale {args.scale}\n"
                          "default_border none\nfocus_follows_mouse no\n")
        processes = []
        with (evidence / f"wayland-{args.scale}x.log").open("w+") as log:
            def launch(*argv):
                process = subprocess.Popen(list(map(str, argv)), env=env, stdout=log, stderr=log)
                processes.append(process)
                return process

            def run(*argv):
                return subprocess.check_output(list(map(str, argv)), env=env, text=True,
                                               stderr=subprocess.STDOUT, timeout=10)

            def wait(predicate, label):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    try:
                        value = predicate()
                        if value:
                            return value
                    except (OSError, subprocess.CalledProcessError):
                        pass
                    time.sleep(.05)
                log.flush()
                raise AssertionError(label + "\n" + log.name + "\n" + Path(log.name).read_text())

            def nodes(node):
                yield node
                for child in node.get("nodes", []) + node.get("floating_nodes", []):
                    yield from nodes(child)

            def window(app):
                tree = json.loads(run("swaymsg", "-t", "get_tree", "-r"))
                return next((node for node in nodes(tree) if node.get("app_id") == app), None)

            def type_keys(*argv):
                # A headless seat has no physical keyboard. Prime its keyboard
                # capability and allow winit to bind before the first real key.
                run("wtype", "-k", "Shift_L", "-s", "250", *argv)

            def capture(name):
                path = evidence / f"wayland-{args.scale}x-{name}.png"
                run("grim", path)
                return path

            def image_bounds(name):
                image = Image.open(capture(name)).convert("RGB")
                red, green, _ = image.split()
                mask = ImageChops.multiply(red.point(lambda p: 255 if p > 180 else 0),
                                          green.point(lambda p: 255 if p < 80 else 0))
                bounds = mask.getbbox()
                return (bounds[2] - bounds[0], bounds[3] - bounds[1]) if bounds else None

            try:
                launch("sway", "--unsupported-gpu", "-c", config)
                env["SWAYSOCK"] = str(wait(lambda: next(runtime.glob("sway-ipc.*.sock"), None), "Sway socket"))
                env["WAYLAND_DISPLAY"] = wait(lambda: next((p.name for p in runtime.glob("wayland-*")
                                                                           if p.is_socket()), None), "Wayland socket")
                notes = launch(args.bin_dir / "jwm-notes")
                wait(lambda: window("org.jwm.Notes"), "Native notes app_id")
                run("swaymsg", '[app_id="org.jwm.Notes"] focus')
                type_keys("-d", "10", "原生 Wayland")
                directory = work / "data/jwm/notes"
                wait(lambda: any(path.read_text() == "原生 Wayland" for path in directory.glob("*.txt")),
                     "Wayland Unicode input and autosave")
                type_keys("-M", "ctrl", "-k", "a", "-k", "c", "-m", "ctrl", "-k", "End",
                          "-k", "Return", "-M", "ctrl", "-k", "v", "-m", "ctrl")
                wait(lambda: any(path.read_text() == "原生 Wayland\n原生 Wayland" for path in directory.glob("*.txt")),
                     "Wayland clipboard")
                capture("notes")
                type_keys("-M", "ctrl", "-k", "w", "-m", "ctrl")
                wait(lambda: notes.poll() is not None, "Native notebook close")
                print(f"Wayland {args.scale}x Unicode input, clipboard, autosave and close: PASS", flush=True)

                path = work / "red image.png"
                Image.new("RGB", (400, 200), (240, 20, 20)).save(path)
                viewer = launch(args.bin_dir / "jwm-image-viewer", "--", path)
                wait(lambda: window("org.jwm.ImageViewer"), "Native viewer app_id")
                # Wait for decoded pixels, then change the view through real keys.
                wait(lambda: image_bounds("viewer"), "Decoded image pixels")
                type_keys("1")
                wait(lambda: image_bounds("viewer") == (400 * args.scale, 200 * args.scale), "Actual size")
                type_keys("r")
                wait(lambda: image_bounds("rotated") == (200 * args.scale, 400 * args.scale), "Rotation")
                type_keys("-k", "F11")
                wait(lambda: window("org.jwm.ImageViewer")["fullscreen_mode"] == 1, "Native fullscreen")
                type_keys("-k", "Escape")
                wait(lambda: window("org.jwm.ImageViewer")["fullscreen_mode"] == 0, "Leave fullscreen")
                type_keys("-M", "ctrl", "-k", "w", "-m", "ctrl")
                wait(lambda: viewer.poll() is not None, "Native viewer close")
                print(f"Wayland {args.scale}x image pixels, rotation, actual size and fullscreen: PASS", flush=True)
            finally:
                for process in reversed(processes):
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(3)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(3)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
