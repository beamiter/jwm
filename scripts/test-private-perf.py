#!/usr/bin/env python3
"""Repeat a fixed X11 scroll workload on an owned Xvfb session, never user display.

This measures one isolated1280x800 X11 workload, not physical GPU output or
input-to-photon. Three nominal30second frame windows are defaults; every run preserves exact
binary/config/source identities and raw logs. Requires a freshly built JWM/tool.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import tempfile
import struct
import zlib

from acceptance_evidence import artifact, provenance

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "private_desktop", ROOT / "eva/tests/desktop-ipc.py"
)
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, required=True)
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--backend", choices=("x11rb", "xcb"), default="x11rb")
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--refresh-hz", type=int, default=60)
    parser.add_argument("--warmup-seconds", type=int, default=5)
    parser.add_argument(
        "--idle-seconds",
        type=int,
        default=30,
        help="read-only core idle window before scroll workload (zero skips)",
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument(
        "--log-level", default="warn", help="RUST_LOG filter for the private compositor"
    )
    parser.add_argument(
        "--reopen-producer",
        action="store_true",
        help="lifecycle diagnostic: destroy/recreate the producer between repeats",
    )
    parser.add_argument(
        "--remap-producer",
        action="store_true",
        help="same-native-lifetime diagnostic: unmap/map one live producer between repeats",
    )
    args = parser.parse_args()
    if args.reopen_producer and args.remap_producer:
        parser.error(
            "reopen-producer and remap-producer are separate lifecycle diagnostics"
        )
    if (
        args.samples < 1
        or not 2 <= args.seconds <= 60
        or not 1 <= args.refresh_hz <= 360
    ):
        parser.error("samples>=1,2<=seconds<=60,1<=refresh-hz<=360 required")
    if not 0 <= args.idle_seconds <= 3600:
        parser.error("idle-seconds must be within0..3600")
    if not 1 <= args.warmup_seconds <= 30:
        parser.error("warmup-seconds must be within1..30")
    tools = {
        name: shutil.which(name)
        for name in ("Xvfb", "xdotool", "import", "dbus-run-session")
    }
    ready = all(tools.values()) and args.jwm.is_file() and args.tool.is_file()
    if args.preflight:
        print(
            json.dumps(
                {"ready": ready, "dependencies": tools, "uses_user_display": False},
                indent=2,
            )
        )
        return int(not ready)
    if not ready:
        parser.error("missing executable or private-session dependencies")
    work = args.output.resolve()
    work.mkdir(parents=True, exist_ok=False)
    children = FIXTURE.Children()
    runtime = tempfile.TemporaryDirectory(prefix="jwm-perf-")

    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")

    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("JWM_", "OPENAI_", "PULSE_", "PIPEWIRE_"))
        and key
        not in (
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "XAUTHORITY",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
        )
    }
    for key, leaf in (
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ):
        path = work / leaf
        path.mkdir(mode=0o700)
        env[key] = str(path)
    # Unix endpoints have a108-byte path bound; evidence directories may be long.
    env["XDG_RUNTIME_DIR"] = runtime.name
    env.update(
        JWM_COMPOSITOR_API="glx",
        LIBGL_ALWAYS_SOFTWARE="1",
        RUST_LOG=args.log_level,
        DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"),
    )
    report = {
        "format": 2,
        "backend": args.backend,
        "level": "isolated",
        "scenario": "managed-scroll500-v2",
        "uses_user_display": False,
        "provenance": provenance(
            ["src", "tools/perf.rs", "tools/perf_contract.rs", "Cargo.lock"]
        ),
        "harness_inputs": [
            artifact(Path(__file__)),
            artifact(ROOT / "scripts/fixtures/private-scroll-workload.py"),
        ],
        "binaries": [artifact(args.jwm), artifact(args.tool)],
        "runs": [],
        "status": "fail",
    }

    def run(*argv, **kwargs):
        return subprocess.run(
            list(map(str, argv)), env=env, check=True, timeout=400, **kwargs
        )

    try:
        jwm, tool = args.jwm.resolve(), args.tool.resolve()
        run(jwm, "--backend", args.backend, "--gen-config", stdout=subprocess.DEVNULL)
        wallpaper = work / "wallpaper.png"

        def png_chunk(kind, payload):
            return (
                struct.pack(">I", len(payload))
                + kind
                + payload
                + struct.pack(">I", zlib.crc32(kind + payload))
            )

        wallpaper.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 32, 24, 8, 2, 0, 0, 0))
            + png_chunk(b"IDAT", zlib.compress((b"\0" + bytes([26, 31, 40]) * 32) * 24))
            + png_chunk(b"IEND", b"")
        )
        for config in (work / "config/jwm").glob("config_*.toml"):
            text = config.read_text()
            for section, key, value, required in (
                ("status_bar", "show_bar", "false", True),
                ("status_bar", "name", '"/bin/true"', True),
                ("desktop_shell", "enabled", "false", False),
                ("behavior", "compositor", "true", True),
                ("behavior", "wallpaper_effect", '"off"', False),
                ("behavior", "wallpaper", json.dumps(str(wallpaper)), True),
            ):
                text = FIXTURE.edit_setting(text, section, key, value, required)
            # Generated defaults omit the optional desktop_shell table. A
            # best-effort edit alone would silently leave the real Shell on.
            if "[desktop_shell]" not in text.splitlines():
                text += "\n[desktop_shell]\nenabled = false\n"
            config.write_text(text)
        report["configs"] = [artifact(p) for p in (work / "config/jwm").glob("*.toml")]
        env["DISPLAY"] = children.display("Xvfb", env, work)
        wm = children.spawn(
            ["dbus-run-session", "--", str(jwm), "--backend", args.backend],
            env,
            work / "jwm.log",
        )
        socket = Path(runtime.name) / "jwm-ipc.sock"
        deadline = time.monotonic() + 30
        while not socket.exists():
            if wm.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("private compositor startup failed; seejwm.log")
            time.sleep(0.05)
        while True:
            try:
                report["topology"] = FIXTURE.query(socket, "get_monitors")
                break
            except (OSError, RuntimeError):
                if wm.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(
                        "private compositor did not become IPC-ready; see jwm.log"
                    )
                time.sleep(0.1)
        run("xdotool", "mousemove", "--sync", "640", "350")
        run("xdotool", "mousemove", "--sync", "640", "400")
        time.sleep(args.warmup_seconds)
        shell = FIXTURE.query(socket, "get_desktop_shell")
        if shell.get("host", {}).get("pid") is not None:
            raise RuntimeError(
                "desktop_shell.enabled=false did not stop the Shell; fixture is not isolated core-only"
            )
        report["desktop_shell_verified"] = shell
        report["pointer_before_idle"] = subprocess.check_output(
            ["xdotool", "getmouselocation", "--shell"], env=env, text=True, timeout=3
        )
        report["idle_conditions_before"] = {
            "effects": FIXTURE.query(socket, "get_effect_status"),
            "metrics": FIXTURE.query(socket, "get_metrics"),
        }
        if args.idle_seconds:
            idle_log = work / "idle-core.json"
            with idle_log.open("w") as stream:
                run(tool, "perf", "idle", "--seconds", args.idle_seconds, stdout=stream)
            idle_result = json.loads(idle_log.read_text())
            report["idle_conditions_after"] = {
                "effects": FIXTURE.query(socket, "get_effect_status"),
                "metrics": FIXTURE.query(socket, "get_metrics"),
            }
            report["idle_core"] = {
                "result": artifact(idle_log),
                "scope": "compositor only; default Shell not running",
                "data": idle_result,
            }
        measurement_hz = None
        workload = None
        previous_target = None
        previous_native = None
        report["producer_lifetime"] = (
            "destroy/recreate between repeats"
            if args.reopen_producer
            else "unmap/map same native drawable and live PID between repeats"
            if args.remap_producer
            else "one persistent managed window across all measured repeats; window reopen is a separate lifecycle test"
        )
        for sample in range(args.samples):
            output = work / f"sample-{sample + 1}.json"
            log = work / f"sample-{sample + 1}.log"
            # The benchmark sees the same named scrolling primitive each time.
            # Its concurrent procfs sample is explicitly not an idle result.
            if workload is None:
                workload = children.spawn(
                    [
                        sys.executable,
                        str(ROOT / "scripts/fixtures/private-scroll-workload.py"),
                    ],
                    env,
                    work / f"workload-{sample + 1}.log",
                )
            ready = time.monotonic() + 10
            managed = []
            while time.monotonic() < ready:
                if workload.poll() is not None:
                    raise RuntimeError(
                        "managed scroll producer exited; see workload log"
                    )
                windows = FIXTURE.query(socket, "get_windows")
                managed = [
                    window
                    for window in windows
                    if window.get("pid") == workload.pid
                    and (
                        window.get("class")
                        in ("JwmPerfScroll", "PerfScrollFixture", "perfscrollfixture")
                        or window.get("name") == "Controlled scroll500 workload"
                    )
                ]
                if (
                    len(managed) == 1
                    and managed[0].get("w", 0) >= 500
                    and managed[0].get("h", 0) >= 500
                ):
                    break
                time.sleep(0.05)
            if (
                len(managed) != 1
                or managed[0].get("w", 0) < 500
                or managed[0].get("h", 0) < 500
            ):
                raise RuntimeError(
                    f"scroll window was not managed at required size: {windows[:3]!r}"
                )
            context = FIXTURE.query(socket, "get_execution_context")
            targets = [
                window
                for window in context.get("windows", [])
                if str(window.get("id")) == str(managed[0]["id"])
                and window.get("pid") == workload.pid
            ]
            if len(targets) != 1 or not targets[0].get("target_token"):
                raise RuntimeError(
                    "current managed producer lacks a unique logical lifetime token"
                )
            logical_target = {
                key: targets[0].get(key)
                for key in ("id", "window_ref", "target_token", "pid")
            }
            native = (
                int(
                    json.loads(
                        (work / f"workload-{sample + 1}.log")
                        .read_text()
                        .splitlines()[0]
                    )["window"]
                )
                if args.reopen_producer or sample == 0
                else previous_native
            )
            if args.reopen_producer and previous_target is not None:
                if logical_target["target_token"] == previous_target:
                    raise RuntimeError(
                        "reopened native window reused the old logical lifetime token"
                    )
                if native != previous_native:
                    raise RuntimeError(
                        "native XID did not recycle; this run is not the requested reuse regression"
                    )
            report.setdefault("logical_lifetimes", []).append(
                {
                    "sample": sample + 1,
                    "logical_target": logical_target,
                    "native_window_id": native,
                    "previous_target": previous_target,
                    "same_native_id": native == previous_native
                    if previous_native is not None
                    else None,
                }
            )
            previous_target = logical_target["target_token"]
            previous_native = native
            time.sleep(1)
            from PIL import Image, ImageChops

            first_image = work / f"preflight-{sample + 1}-a.png"
            second_image = work / f"preflight-{sample + 1}-b.png"
            run("import", "-window", "root", first_image)
            before = FIXTURE.query(socket, "get_metrics")
            before_at = time.monotonic()
            time.sleep(1)
            run("import", "-window", "root", second_image)
            after = FIXTURE.query(socket, "get_metrics")
            preflight_seconds = time.monotonic() - before_at
            frame_delta = after.get("frame_count", 0) - before.get("frame_count", 0)
            image_a = Image.open(first_image).convert("RGB")
            image_b = Image.open(second_image).convert("RGB")
            difference = ImageChops.difference(image_a, image_b).convert("L")
            changed = sum(
                count for value, count in enumerate(difference.histogram()) if value > 8
            )
            if frame_delta < 5 or changed < 1000:
                report["failed_preflight"] = {
                    "sample": sample + 1,
                    "managed_window": managed[0],
                    "logical_target": logical_target,
                    "native_window_id": native,
                    "producer_pid": workload.pid,
                    "producer_exit": workload.poll(),
                    "frame_delta": frame_delta,
                    "changed_pixels": changed,
                    "metrics_before": before,
                    "metrics_after": after,
                    "screenshots": [artifact(first_image), artifact(second_image)],
                }
                raise RuntimeError(
                    f"ineffective scroll producer: {frame_delta} frames/{changed} changed pixels"
                )
            if measurement_hz is None:
                # A cold one-second probe is not a stable rate estimate and
                # must not silently shorten later warmed measurement windows.
                # Keep the operator's nominal frame count fixed across repeats.
                measurement_hz = args.refresh_hz
            report.setdefault("preflight", []).append(
                {
                    "sample": sample + 1,
                    "managed_window": managed[0],
                    "frame_delta": frame_delta,
                    "seconds": preflight_seconds,
                    "changed_pixels": changed,
                    "measurement_hz": measurement_hz,
                    "screenshots": [artifact(first_image), artifact(second_image)],
                }
            )
            command = [
                str(tool),
                "perf",
                "record",
                "--out",
                str(output),
                "--frames",
                str(args.seconds * measurement_hz),
                "--warmup",
                str(args.warmup_seconds * measurement_hz),
                "--idle-seconds",
                "2",
            ]
            metrics_before = FIXTURE.query(socket, "get_metrics")
            start = time.monotonic()
            with log.open("w") as stream:
                run(*command, stdout=stream, stderr=subprocess.STDOUT)
            elapsed = time.monotonic() - start
            metrics_after = FIXTURE.query(socket, "get_metrics")
            result = json.loads(output.read_text())
            for name, scenario in result["scenarios"].items():
                if scenario.get("measurement"):
                    scenario["measurement"]["workload"] = "managed-scroll500-v2"
                if name == "idle":
                    # perf's idle sampler ran during controlled scroll. Keep its
                    # raw sample as workload counters, never claim idle overhead.
                    scenario["status"] = "skipped"
                    scenario["reason"] = (
                        "fixed scroll workload active; not an idle measurement"
                    )
                    scenario["metrics"] = {}
            output.write_text(json.dumps(result, indent=2) + "\n")
            before_count = metrics_before.get("frame_count")
            after_count = metrics_after.get("frame_count")
            observed_rate = (
                (after_count - before_count) / elapsed
                if isinstance(before_count, int)
                and isinstance(after_count, int)
                and after_count >= before_count
                and elapsed > 0
                else None
            )
            report["runs"].append(
                {
                    "sample": sample + 1,
                    "command": command,
                    "elapsed_seconds": elapsed,
                    "frame_count_before": before_count,
                    "frame_count_after": after_count,
                    "observed_whole_command_fps": observed_rate,
                    "observed_rate_scope": "whole tool command including warmup and procfs sampling; not presentation FPS",
                    "benchmark_fps_avg_semantics": "inverse CPU render-pass duration, not inter-frame cadence",
                    "baseline": artifact(output),
                    "log": artifact(log),
                    "scenario_status": {
                        name: r["status"] for name, r in result["scenarios"].items()
                    },
                }
            )
            (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            if args.remap_producer and sample + 1 < args.samples:
                run("xdotool", "windowunmap", str(native))
                native_pid = subprocess.check_output(
                    ["xprop", "-id", str(native), "_NET_WM_PID"],
                    env=env,
                    text=True,
                    timeout=3,
                ).strip()
                if (
                    not native_pid.endswith(str(workload.pid))
                    or workload.poll() is not None
                ):
                    raise RuntimeError(
                        "Unmap destroyed or replaced the producer native lifetime"
                    )
                run("xdotool", "windowmap", str(native))
                report.setdefault("native_remaps", []).append(
                    {
                        "after_sample": sample + 1,
                        "native_window_id": native,
                        "pid": workload.pid,
                        "pid_property": native_pid,
                        "producer_stayed_alive": True,
                    }
                )
            if args.reopen_producer:
                if workload.poll() is None:
                    workload.terminate()
                    workload.wait(timeout=5)
                workload = None
                retired_deadline = time.monotonic() + 5
                while time.monotonic() < retired_deadline:
                    remaining = FIXTURE.query(socket, "get_execution_context").get(
                        "windows", []
                    )
                    if all(
                        window.get("target_token") != previous_target
                        for window in remaining
                    ):
                        report.setdefault("retired_logical_targets", []).append(
                            previous_target
                        )
                        break
                    time.sleep(0.01)
                else:
                    raise RuntimeError(
                        "destroyed producer logical token remained live beyond deadline"
                    )
        report["status"] = (
            "pass"
            if all(
                r["scenario_status"].get("steady_frame") == "recorded"
                for r in report["runs"]
            )
            else "skip"
        )
        report["raw_evidence"] = [artifact(work / "jwm.log")]
        if report["binaries"] != [artifact(args.jwm), artifact(args.tool)]:
            raise RuntimeError("executable changed during measurement")
    except KeyboardInterrupt as error:
        report["status"], report["reason"] = "skip", f"interrupted: {error}"
    except Exception as error:
        report["status"], report["reason"] = "fail", str(error)
    finally:
        children.close()
        runtime.cleanup()
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return int(report["status"] == "fail")


if __name__ == "__main__":
    sys.exit(main())
