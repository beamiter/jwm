#!/usr/bin/env python3
"""Verify actual native-app pixels follow private IPC theme changes atomically.

Uses owned Xvfb/privateD-Bus/privateconfig and a same-UID fakeIPC server. No
running JWM, user display, user notes or external service is used.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "private_desktop", ROOT / "eva/tests/desktop-ipc.py"
)
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bin-dir", type=Path, default=ROOT / "target/native-apps/debug"
    )
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--under-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not args.under_bus:
        return subprocess.call(
            [
                "dbus-run-session",
                "--",
                sys.executable,
                __file__,
                *sys.argv[1:],
                "--under-bus",
            ]
        )
    from PIL import Image, ImageChops

    binary = (args.bin_dir / "jwm-notes").resolve()
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="jwm-native-theme-") as temporary:
        work = Path(temporary)
        env = dict(os.environ, GIO_USE_VFS="local")
        for key, leaf in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
        ]:
            path = work / leaf
            path.mkdir(mode=0o700)
            env[key] = str(path)
        for key in (
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "JWM_APP_THEME",
            "JWM_APP_FONT",
        ):
            env.pop(key, None)
        children = FIXTURE.Children()
        stopped = threading.Event()
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        endpoint = work / "runtime/theme.sock"
        listener.bind(str(endpoint))
        listener.listen()
        listener.settimeout(0.1)
        env["JWM_SOCKET"] = str(endpoint)
        current = {
            "protocol_version": 1,
            "font": "Sans 11",
            "radius": 6.0,
            "native_colors": [
                [0.05, 0.07, 0.09, 1.0],
                [0.12, 0.14, 0.16, 1.0],
                [0.20, 0.22, 0.24, 1.0],
                [0.3, 0.3, 0.3, 1.0],
                [0.98, 0.98, 0.98, 1.0],
                [0.65, 0.65, 0.65, 1.0],
                [0.25, 0.5, 0.9, 1.0],
                [0.2, 0.3, 0.4, 1.0],
            ],
        }
        requests = []

        def serve():
            while not stopped.is_set():
                try:
                    client, _ = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    return
                with client:
                    client.settimeout(2)
                    try:
                        data = bytearray()
                        while b"\n" not in data and len(data) < 4096:
                            block = client.recv(1024)
                            if not block:
                                break
                            data.extend(block)
                        query = json.loads(data)
                        requests.append(query)
                        reply = {"success": True, "data": dict(current)}
                        client.sendall(json.dumps(reply).encode() + b"\n")
                    except (OSError, ValueError):
                        pass

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            env["DISPLAY"] = children.display("Xvfb", env, work)
            app = children.spawn([str(binary)], env, evidence / "theme-app.log")

            def command(*argv):
                return subprocess.check_output(
                    list(map(str, argv)), env=env, text=True, timeout=5
                ).strip()

            def pixels(name):
                ids = command(
                    "xdotool", "search", "--onlyvisible", "--class", "org.jwm"
                ).splitlines()
                if not ids:
                    return None
                output = evidence / (name + ".png")
                command("import", "-window", ids[0], output)
                return Image.open(output).convert("RGB")

            def count_color(image, expected):
                return sum(
                    count
                    for count, color in image.getcolors(image.width * image.height)
                    if max(abs(a - b) for a, b in zip(color, expected)) <= 3
                )

            def wait_color(name, rgb):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    if app.poll() is not None:
                        raise AssertionError("native app exited; see theme-app.log")
                    try:
                        image = pixels(name)
                        if image and count_color(image, rgb) > 5000:
                            return image
                    except subprocess.CalledProcessError:
                        pass
                    time.sleep(0.15)
                raise AssertionError(
                    f"{name}: actual canvas never acquired expectedRGB{rgb}"
                )

            wait_color("dark", [13, 18, 23])
            light_colors = [list(v) for v in current["native_colors"]]
            light_colors[0] = [0.9, 0.92, 0.94, 1.0]
            light_colors[1] = [0.82, 0.84, 0.86, 1.0]
            light_colors[4] = [0.05, 0.05, 0.05, 1.0]
            current = {**current, "native_colors": light_colors}
            light = wait_color("light", [230, 235, 240])
            region = (40, 0, light.width - 120, min(64, light.height))
            current = {**current, "font": "Sans 18"}
            deadline = time.monotonic() + 10
            font_changed_pixels = 0
            while time.monotonic() < deadline:
                enlarged = pixels("font-18")
                diff = ImageChops.difference(
                    light.crop(region), enlarged.crop(region)
                ).convert("L")
                font_changed_pixels = sum(
                    count for value, count in enumerate(diff.histogram()) if value > 10
                )
                if font_changed_pixels > 50:
                    break
                time.sleep(0.15)
            assert font_changed_pixels > 50, (
                "Font-only11→18 update did not change actual header text pixels"
            )
            # A partial publication must never replace the last complete theme.
            current = {**current, "native_colors": []}
            time.sleep(2.5)
            image = pixels("malformed-keeps-last")
            assert count_color(image, [230, 235, 240]) > 5000
            assert requests and all(q.get("query") == "get_ui_theme" for q in requests)
            (evidence / "theme-result.json").write_text(
                json.dumps(
                    {
                        "status": "pass",
                        "level": "isolated",
                        "binary": str(binary),
                        "assertions": [
                            "dark tokens painted",
                            "live light tokens painted",
                            "font-only11→18 changes actual header text pixels",
                            "malformed token publication preserved complete prior theme",
                        ],
                        "queries": len(requests),
                        "font_changed_pixels": font_changed_pixels,
                        "uses_user_display": False,
                    },
                    indent=2,
                )
                + "\n"
            )
            print(
                "Native application live dark→light theme,font11→18 and malformed-update pixels: PASS"
            )
        finally:
            children.close()
            stopped.set()
            listener.close()
            worker.join(timeout=3)
    return 0


if __name__ == "__main__":
    sys.exit(main())
