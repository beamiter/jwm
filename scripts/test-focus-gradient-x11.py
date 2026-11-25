#!/usr/bin/env python3
"""Sample the native focused gradient in an owned software GL X11 session."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("terminal_exit", HERE / "test-terminal-exit-x11.py")
EXIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXIT)
FIXTURE = EXIT.FIXTURE
DURATION_MS = 1600


def contrast(pair):
    return sum(abs(a - b) for a, b in zip(*pair))


def sample_pair(image, points):
    return [image.getpixel(point) for point in points]


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    children = FIXTURE.Children()
    capture = None
    with tempfile.TemporaryDirectory(prefix="jwm-focus-gradient-") as name:
        work = Path(name)
        marker = work / "alive"
        marker.touch()
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("JWM_", "OPENAI_", "PULSE_", "PIPEWIRE_"))
               and key not in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "WAYLAND_SOCKET",
                               "DBUS_SESSION_BUS_ADDRESS", "DBUS_SYSTEM_BUS_ADDRESS",
                               "NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC")}
        for key, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                          ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
            (work / leaf).mkdir(mode=0o700)
            env[key] = str(work / leaf)
        env.update(LIBGL_ALWAYS_SOFTWARE="1", JWM_COMPOSITOR_API="glx", RUST_LOG="warn",
                   DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"))
        (work / "config/jwm").mkdir()
        socket = work / "runtime/jwm-ipc.sock"
        with args.jwm.open("rb") as binary:
            binary_sha = hashlib.file_digest(binary, "sha256").hexdigest()
        report = {"backend": args.backend, "status": "failed", "binary_sha256": binary_sha,
                  "source_sha": os.environ.get("GITHUB_SHA"), "samples": []}

        def command(*parts):
            return subprocess.check_output(list(map(str, parts)), env=env,
                                           stderr=subprocess.STDOUT, timeout=20)

        def wait(predicate, label):
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                try:
                    if value := predicate():
                        return value
                except (OSError, RuntimeError, ValueError, subprocess.SubprocessError):
                    pass
                time.sleep(.02)
            raise AssertionError(label)

        def config_file(path):
            content = path.read_text()
            changes = [
                ("status_bar", "show_bar", "false"), ("status_bar", "name", '"/bin/true"'),
                ("behavior", "compositor", "true"), ("behavior", "border_enabled", "true"),
                ("behavior", "border_width", "8.0"), ("behavior", "border_glow_enabled", "false"),
                ("behavior", "border_gradient_enabled", "true"),
                ("behavior", "border_gradient_color_a", "[1.0, 0.0, 0.0, 1.0]"),
                ("behavior", "border_gradient_color_b", "[0.0, 0.0, 1.0, 1.0]"),
                ("behavior", "border_gradient_angle", "0.0"),
                ("behavior", "border_gradient_speed", "0.0"),
                ("behavior", "focus_highlight", "true"),
                ("behavior", "focus_highlight_color", "[0.0, 1.0, 0.0, 1.0]"),
                ("behavior", "focus_highlight_duration_ms", str(DURATION_MS)),
                ("animation", "enabled", "false"),
            ]
            for section, key, literal in changes:
                content = FIXTURE.edit_setting(content, section, key, literal)
            content = FIXTURE.edit_setting(content, "desktop_shell", "enabled", "false", False)
            path.write_text(content)

        def row(record):
            return EXIT.producer_row(FIXTURE.query(socket, "get_windows"), record)

        def focus(record):
            FIXTURE.command(socket, "focus_window", {"id": record["id"]})
            wait(lambda: row(record)["is_focused"], "fixture focus did not change")

        def producer(kind, x, y):
            record = work / (kind + ".json")
            children.spawn([sys.executable, str(HERE / "test-terminal-exit-x11.py"), "--producer",
                            kind, "--record", str(record), "--alive", str(marker)],
                           env, args.output / (kind + ".log"))
            data = wait(lambda: json.loads(record.read_text()), "X11 producer missing")
            data["id"] = wait(lambda: row(data), "managed window missing")["id"]
            focus(data)
            if not row(data)["is_floating"]:
                FIXTURE.command(socket, "togglefloating", {})
            command("xdotool", "windowsize", data["xid"], 420, 260)
            command("xdotool", "windowmove", data["xid"], x, y)
            wait(lambda: row(data)["w"] == 420 and row(data)["h"] == 260, "resize not applied")
            return data

        try:
            command(args.jwm, "--backend", args.backend, "--gen-config")
            for path in (work / "config/jwm").glob("config_*.toml"):
                config_file(path)
            command(args.jwm, "--backend", args.backend, "--check-config")
            env["DISPLAY"] = children.display("Xvfb", env, args.output)
            env["DISPLAY"] = children.display("Xephyr", env, args.output)
            wm = children.spawn(["dbus-run-session", "--", str(args.jwm), "--backend",
                                 args.backend], env, args.output / "jwm.log")
            wait(lambda: socket.exists() and FIXTURE.query(socket, "get_status")["compositor_active"],
                 "software compositor not active")
            target = producer("pip", 100, 180)
            other = producer("term", 740, 180)
            capture = EXIT.RootCapture(env["DISPLAY"])

            focus(target)
            time.sleep(DURATION_MS / 1000 + .25)
            ready = row(target)
            baseline = capture.grab()
            baseline.save(args.output / "rest.png")
            x, y, w = (ready[key] for key in ("x", "y", "w"))
            points_at = lambda line: [(x + w // 5, line), (x + 4 * w // 5, line)]
            line = max(range(y - 12, y + 7),
                       key=lambda line: contrast(sample_pair(baseline, points_at(line))))
            points = points_at(line)
            rest = sample_pair(baseline, points)
            assert contrast(rest) > 100 and all(c[1] < 25 for c in rest), ("rest gradient", rest)
            report["points"], report["rest"] = points, rest

            focus(other)
            time.sleep(.08)
            focus(target)
            start = time.monotonic()
            mixed = peak = False
            while time.monotonic() - start < DURATION_MS / 1000 + .2:
                assert wm.poll() is None, "compositor exited during the pulse"
                shot = capture.grab()
                pair = sample_pair(shot, points)
                report["samples"].append({"elapsed": time.monotonic() - start, "rgb": pair})
                if not mixed and min(c[1] for c in pair) > 35 and contrast(pair) > 55:
                    shot.save(args.output / "mixed.png")
                    mixed = True
                if not peak and min(c[1] for c in pair) > 200 and contrast(pair) < 25:
                    shot.save(args.output / "peak.png")
                    peak = True
                time.sleep(.018)
            after = capture.grab()
            after.save(args.output / "restored.png")
            restored = sample_pair(after, points)
            report.update(mixed=mixed, peak=peak, restored=restored)
            assert mixed and peak, "did not observe a distinct mixed gradient and near-flat peak"
            assert contrast(restored) > 100 and all(
                abs(a - b) <= 4 for left, right in zip(rest, restored) for a, b in zip(left, right)
            ), ("restored gradient", restored)
            report["status"] = "passed"
        finally:
            print(json.dumps(report), flush=True)
            (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            if capture:
                capture.close()
            marker.unlink(missing_ok=True)
            children.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--backend", choices=("x11rb", "xcb"), required=True)
    args = parser.parse_args()
    args.jwm = args.jwm.resolve(strict=True)
    args.output = args.output.resolve()
    run(args)


if __name__ == "__main__":
    main()
