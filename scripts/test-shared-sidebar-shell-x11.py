#!/usr/bin/env python3
"""Current-build shared-shell acceptance in a private X11 session.

Each invocation records one frontend/backend result. --deep additionally tests
host ownership through frontend replacement/crashes, mode changes and rollback.
All desktop configuration and displays are private. Optional hardware probes require
an explicitly selected local PulseAudio endpoint and restore exact mixer values.
"""
import argparse
import ctypes
import importlib.util
import hashlib
import fcntl
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURE_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def release_private_gvfs_mount(work):
    """Detach only GVFS mounted inside this fixture's private runtime folder."""
    target = (work / "runtime" / "gvfs").absolute()
    mountinfo = Path("/proc/self/mountinfo")
    if not mountinfo.exists():
        return None

    def mounted():
        for line in mountinfo.read_text().splitlines():
            before, separator, after = line.partition(" - ")
            fields = before.split()
            if not separator or len(fields) < 5:
                continue
            path = re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), fields[4])
            if path != str(target):
                continue
            filesystem, _, source = after.partition(" ")
            if not filesystem.startswith("fuse") or "gvfs" not in filesystem + source:
                raise RuntimeError(f"unexpected private runtime mount: {target}")
            return True
        return False

    if not mounted():
        return None
    unmount = shutil.which("fusermount3") or shutil.which("fusermount")
    if not unmount:
        raise RuntimeError("private GVFS mount requires fusermount for cleanup")
    result = subprocess.run([unmount, "-u", "-z", "--", str(target)],
                            capture_output=True, text=True, timeout=5)
    if mounted():
        raise RuntimeError(f"private GVFS unmount failed: {result.stderr.strip()}")
    return {"path": str(target), "status": "detached", "command": unmount}


def executable_hash(pid):
    digest = hashlib.sha256()
    with Path(f"/proc/{pid}/exe").open("rb") as executable:
        for block in iter(lambda: executable.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()

spec = importlib.util.spec_from_file_location("four_edge_fixture", ROOT / "scripts/test-four-edge-shell-x11.py")
legacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(legacy)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bar", default="tauri_leptos_bar")
    parser.add_argument("--bar-bin", type=Path)
    parser.add_argument("--backend", choices=("xcb", "x11rb"), default="x11rb")
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/debug")
    parser.add_argument("--shell-bin", type=Path, default=ROOT / "crates/jwm_shell/target/debug/jwm-shell")
    parser.add_argument("--bridge-bin", type=Path, help="current jwm-bridge executable for the private deep MPRIS test")
    parser.add_argument("--second-bar", default="tao_pixels_bar")
    parser.add_argument("--second-bar-bin", type=Path)
    parser.add_argument("--evidence-dir", type=Path, default=ROOT / "artifacts/shared-shell")
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--theme", choices=("light", "dark"), default="light")
    parser.add_argument("--deep", action="store_true")
    parser.add_argument("--ui-font", help="optional private system UI font override for visual fixtures")
    parser.add_argument("--submenu-themes", action="store_true", help="capture Hub child pages and verify live theme changes without replacing the shared host")
    parser.add_argument("--workarea-only", action="store_true", help="run real external-strut, fullscreen and one-switch rollback acceptance after the standard smoke")
    parser.add_argument("--topology-only", action="store_true", help="run logical output/monitor lock acceptance after the standard smoke")
    parser.add_argument("--legacy-four-edge", action="store_true", help="exercise migration from status_bar.layout=four-edge without an explicit desktop_shell table")
    parser.add_argument("--legacy-edge-shell", action="store_true", help="exercise live legacy xbar edge_shell migration")
    args = parser.parse_args()
    if args.submenu_themes and not shutil.which("import"):
        parser.error("--submenu-themes requires ImageMagick import for final composited X11 display captures")
    bar_dir = ROOT / "bars" / args.bar
    bar_bin = (args.bar_bin or bar_dir / ("src-tauri/target/debug" if (bar_dir / "src-tauri").is_dir() else "target/debug") / args.bar).resolve()
    second_bin = (args.second_bar_bin or ROOT / "bars" / args.second_bar / "target/debug" / args.second_bar).resolve()
    bridge_bin = (args.bridge_bin or args.bin_dir / "jwm-bridge").resolve()
    width, height = map(int, args.size.split("x"))
    assert not (args.legacy_four_edge and args.legacy_edge_shell), "choose one legacy source"
    suffix = "-legacy-four-edge" if args.legacy_four_edge else ("-legacy-edge-shell" if args.legacy_edge_shell else "")
    evidence = (args.evidence_dir / f"{args.bar}-{args.backend}-{args.size}-{args.theme}{suffix}").resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    result = {"bar": args.bar, "backend": args.backend, "size": [width, height],
              "theme": args.theme, "binary": str(bar_bin), "configuration": "legacy_four_edge" if args.legacy_four_edge else ("legacy_edge_shell" if args.legacy_edge_shell else "desktop_shell"), "steps": [], "status": "running"}
    started = time.monotonic()
    children = []
    launched_application = None
    service_identities = []
    with tempfile.TemporaryDirectory(prefix="jwm-shared-shell-") as directory:
        work = Path(directory)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11",
                   JWM_COMPOSITOR_API="glx", JWM_SHELL_BIN=str(args.shell_bin.resolve()))
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                    "JWM_SOCKET", "JWM_BACKEND", "JWM_DESKTOP_SHELL", "JWM_BAR_LAYOUT"):
            env.pop(key, None)
        for key, name in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                          ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                          ("XDG_RUNTIME_DIR", "runtime"), ("XDG_PICTURES_DIR", "pictures")):
            (work / name).mkdir(mode=0o700)
            env[key] = str(work / name)
        if pulse_server := os.environ.get("JWM_TEST_PULSE_SERVER"):
            assert pulse_server.startswith("unix:"), "the hardware probe requires an explicitly selected local PulseAudio socket"
            assert Path(pulse_server[len("unix:"):]).is_socket(), pulse_server
            # Keep all desktop configuration isolated while explicitly routing
            # this opt-in mixer test to the real, already authorized local sink.
            alsa_config = work / "alsa-hardware-test.conf"
            alsa_config.write_text("</usr/share/alsa/alsa.conf>\nctl.!default { type pulse }\n")
            env["ALSA_CONFIG_PATH"] = str(alsa_config)
            env["PULSE_SERVER"] = pulse_server
        env["XBAR_CONFIG"] = str(work / "xbar.toml")
        (work / "xbar.toml").write_text(f'theme="{args.theme}"\n[edge_shell]\nenabled=true\nleft="always"\nright="always"\nbottom="always"\n')
        (work / "bin").mkdir()
        if args.deep:
            env["JWM_TEST_LAUNCH_PID"] = str(work / "launched-application.pid")
            terminal = work / "bin/x-terminal-emulator"
            terminal.write_text("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$JWM_TEST_LAUNCH_PID\"\nexec xmessage -name SharedLaunchedTerminal -title SharedLaunchedTerminal 'shared shell application survival'\n")
            terminal.chmod(0o700)
            env["JWM_TEST_BRIDGE_PID"] = str(work / "bridge.pid")
            bridge_launcher = work / "bin/capture-bridge-pid"
            bridge_launcher.write_text("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$JWM_TEST_BRIDGE_PID\"\nexec \"$@\"\n")
            bridge_launcher.chmod(0o700)
        env["PATH"] = str(work / "bin") + os.pathsep + env.get("PATH", "")
        log = (evidence / "session.log").open("w+")

        def run(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=15)

        def spawn(*argv, **kwargs):
            process = subprocess.Popen(list(map(str, argv)), env=env, stdout=log, stderr=log,
                                       start_new_session=True, **kwargs)
            children.append(process)
            return process

        def wait_for(predicate, label, timeout=20):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, AssertionError, subprocess.SubprocessError, StopIteration) as error:
                    last = str(error)
                time.sleep(0.08)
            raise AssertionError(f"{label}; last={last}")

        def msg(name, values=None, require_success=True):
            argv = [args.bin_dir / "jwm-tool", "msg", name, "--raw"]
            if values is not None:
                argv += ["--args", json.dumps(values)]
            completed = subprocess.run(list(map(str, argv)), env=env, text=True,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
            if require_success and completed.returncode:
                raise subprocess.CalledProcessError(completed.returncode, argv, completed.stdout, completed.stderr)
            reply = json.loads(completed.stdout)
            if require_success:
                assert reply.get("success", True), reply
                return reply.get("data", reply)
            return reply

        def state():
            return msg("get_desktop_shell")

        def set_config(key, value):
            if args.legacy_edge_shell and key.startswith("desktop_shell."):
                field = key.split(".", 1)[1]
                config = work / "xbar.toml"
                text = config.read_text()
                replacement = field + "=" + json.dumps(value)
                if re.search(r"(?m)^" + re.escape(field) + r"\s*=", text):
                    text = re.sub(r"(?m)^" + re.escape(field) + r"\s*=.*$", replacement, text)
                else:
                    text += replacement + "\n"
                config.write_text(text)
                return None
            return msg("set_config", {"key": key, "value": value})

        def visible(window):
            return "IsViewable" in run("xwininfo", "-id", window)

        def panels_ready():
            current = state()
            panels = current["host"]["panels"]
            if not current["host"]["ready"] or len(panels) != 3:
                return None
            assert {(p["output"], p["role"]) for p in panels} == {(0, role) for role in ("left", "right", "bottom")}, panels
            assert len({p["window"] for p in panels}) == 3
            for panel in panels:
                if not visible(panel["window"]):
                    return None
                owner = int(run("xprop", "-id", panel["window"], "_NET_WM_PID").split("=")[-1])
                assert owner == current["host"]["pid"], (owner, current)
            return current

        def top_pid():
            for top in state()["host"].get("top_bars", []):
                pid = top.get("pid")
                if not pid or Path(f"/proc/{pid}/exe").resolve() != bar_bin:
                    continue
                try:
                    windows = run("xdotool", "search", "--pid", pid).split()
                except subprocess.CalledProcessError:
                    continue
                if any(visible(window) for window in windows):
                    return pid
            return None

        def current_window(window):
            return next(item for item in msg("get_windows") if item["id"] == window)

        def edge_controls(panel):
            raw = run("xprop", "-id", panel["window"], "-notype", "_JWM_EDGE_CONTROLS")
            return json.loads(json.loads(raw.split("=", 1)[1].strip()))

        def panel_control(role, kind, window=None):
            panel = next(p for p in state()["host"]["panels"] if p["role"] == role)
            control = next((control for control in edge_controls(panel)
                            if control["kind"] == kind and (window is None or control.get("id") == window)), None)
            if not control:
                return None
            x, y, _, _ = geometry(panel["window"])
            cx, cy, cw, ch = control["rect"]
            return x + int(cx + cw / 2), y + int(cy + ch / 2), [x + cx, y + cy, cw, ch]

        def seek_task_control(task_id):
            target = panel_control("bottom", "task", task_id)
            if target:
                return target
            overflow = panel_control("bottom", "task_overflow")
            if overflow:
                run("xdotool", "mousemove", overflow[0], overflow[1])
                run("xdotool", "click", "1")
                result["overflow_activations"] = result.get("overflow_activations", 0) + 1
            return None

        def provider_sample():
            lines = run(args.shell_bin, "--inspect-providers").splitlines()
            return next(json.loads(line) for line in reversed(lines) if line.startswith("{"))

        def alsa_values(endpoint, numid):
            raw = run("amixer", "-D", endpoint, "cget", "numid=" + str(numid))
            return re.search(r"(?m)^\s*:\s*values=(.+)$", raw).group(1).strip()

        def audio_restore_points(device):
            # Capture ALSA control values, including each channel's mute bit,
            # rather than restoring a rounded percentage or another device.
            selectors = [os.environ["JWM_TEST_ALSA_CTL"]] if os.environ.get("JWM_TEST_ALSA_CTL") else ["default", "pulse"]
            for endpoint in selectors:
                try:
                    controls = run("amixer", "-D", endpoint, "contents")
                except subprocess.CalledProcessError:
                    continue
                points = []
                for block in re.split(r"(?m)(?=^numid=)", controls):
                    header = re.match(r"numid=(\d+),iface=MIXER,name='([^']+)'", block)
                    if header and header[2] in (device + " Playback Volume", device + " Playback Switch"):
                        values = re.search(r"(?m)^\s*:\s*values=(.+)$", block)
                        assert values, block
                        points.append((int(header[1]), values[1].strip()))
                if points:
                    return endpoint, points
            raise AssertionError(f"available ALSA device {device!r} has no independently restorable raw controls")

        def exercise_provider_controls():
            # Private displays still share the real mixer: serialize baseline,
            # native gestures and exact restoration across acceptance sessions.
            descriptor = os.open(f"/tmp/jwm-shell-hardware-{os.getuid()}.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
            try:
                assert os.fstat(descriptor).st_uid == os.getuid()
                fcntl.flock(descriptor, fcntl.LOCK_EX)
                exercise_provider_controls_locked()
            finally:
                os.close(descriptor)

        def exercise_provider_controls_locked():
            initial = provider_sample()
            if os.environ.get("JWM_TEST_PULSE_SERVER"):
                assert initial["audio"]["available"], "explicit real PulseAudio hardware endpoint was not sampled"
            result["hardware"] = {"initial": initial, "checks": {}}
            right = next(panel for panel in state()["host"]["panels"] if panel["role"] == "right")
            px, py, _, _ = geometry(right["window"])
            for provider, kind in (("audio", "audio_slider"), ("brightness", "brightness_slider")):
                control = wait_for(lambda: next((item for item in edge_controls(right) if item["kind"] == kind), None), kind + " metadata")
                available = bool(initial[provider]["available"])
                assert control["enabled"] == available, (provider, control, initial)
                sx, sy, sw, sh = control["rect"]
                if not available:
                    assert control.get("value") is None, control
                    click(px + int(sx + sw / 2), py + int(sy + sh / 2))
                    run("xdotool", "mousemove", px + int(sx + sw / 2), py + int(sy + sh / 2))
                    run("xdotool", "click", "4")
                    assert not provider_sample()[provider]["available"]
                    result["hardware"]["checks"][provider] = "unavailable reading and disabled native pointer/wheel target verified"
                    continue
                percent = float(initial[provider]["percent"])
                if provider == "audio":
                    device = initial[provider]["device"]["name"]
                    assert shutil.which("amixer"), "amixer required for exact audio restoration"
                    endpoint, raw_values = audio_restore_points(device)
                    result["hardware"]["audio_raw_controls"] = {"endpoint": endpoint, "device": device, "values": raw_values}
                    def restore():
                        for numid, values in raw_values:
                            run("amixer", "-D", endpoint, "cset", "numid=" + str(numid), values)
                        assert all(alsa_values(endpoint, numid) == values for numid, values in raw_values)
                else:
                    assert shutil.which("brightnessctl"), "brightnessctl required for exact brightness restoration"
                    raw_brightness = int(run("brightnessctl", "get").strip())
                    def restore():
                        run("brightnessctl", "set", raw_brightness)
                        assert int(run("brightnessctl", "get").strip()) == raw_brightness
                try:
                    tx = px + int(sx + sw / 2)
                    ty = py + round(sy + sh * (1.0 - percent / 100.0))
                    run("xdotool", "mousemove", tx, ty)
                    run("xdotool", "click", "1")
                    # Horizontal dragging outside exercises pointer capture
                    # while requesting the same level, avoiding a volume jump.
                    run("xdotool", "mousedown", "1")
                    try:
                        run("xdotool", "mousemove", width // 2, ty)
                        time.sleep(0.08)
                    finally:
                        run("xdotool", "mouseup", "1")
                    restore()
                    run("xdotool", "mousemove", tx, ty)
                    run("xdotool", "click", "5" if percent >= 95 else "4")
                    changed = wait_for(lambda: (sample if abs(float((sample := provider_sample())[provider]["percent"]) - percent) >= 1 else None), provider + " native wheel adjustment")
                    if provider == "audio":
                        assert changed[provider]["muted"] == initial[provider]["muted"]
                    result["hardware"]["checks"][provider] = {"native_same_level_click_and_drag": True, "native_wheel_percent": changed[provider]["percent"], "restoration": "exact original raw control values"}
                finally:
                    restore()
                    run("xdotool", "mousemove", width // 2, height // 2)
            result["hardware"]["after"] = provider_sample()
            available_names = [name for name in ("audio", "brightness") if initial[name]["available"]]
            if available_names:
                record("real native hardware controls exercised and exact raw values restored: " + ", ".join(available_names))
            else:
                record("missing audio and brightness readings/disabled targets verified; no hardware adjustment performed")

        def exercise_media_controls():
            assert bridge_bin.is_file() and os.access(bridge_bin, os.X_OK), f"build current jwm-bridge first: {bridge_bin}"
            # Both processes are launched by JWM so they inherit its private
            # dbus-run-session bus, never the user's real media-player bus.
            msg("spawn", {"cmd": [str(work / "bin/capture-bridge-pid"), str(bridge_bin)]})
            bridge_pid = int(wait_for(lambda: (work / "bridge.pid").read_text().strip() if (work / "bridge.pid").is_file() else None, "private media bridge launched"))
            identity = legacy.process_identity(bridge_pid)
            assert identity
            service_identities.append(identity)
            destination = work / "mpris.json"
            helper = ROOT / "scripts/fixtures/shared_shell_mpris.py"
            msg("spawn", {"cmd": ["/usr/bin/python3", str(helper), str(destination)]})
            def media_log():
                return json.loads(destination.read_text()) if destination.is_file() else None
            initial = wait_for(lambda: (entry if (entry := media_log()) and entry.get("ready") else None), "private MPRIS service ready")
            identity = legacy.process_identity(initial["pid"])
            assert identity
            service_identities.append(identity)
            wait_for(lambda: state()["outputs"][0]["snapshot"]["media"]["title"] == "Shared sidebar media track 0", "MPRIS state reaches the shared atomic snapshot")
            expectations = [("media_next", "Next", "Shared sidebar media track 1", "Playing"),
                            ("media_play_pause", "PlayPause", "Shared sidebar media track 1", "Paused"),
                            ("media_previous", "Previous", "Shared sidebar media track 0", "Paused")]
            for kind, command, title, playback in expectations:
                target = wait_for(lambda: panel_control("bottom", kind), "native " + kind + " control")
                click(target[0], target[1])
                wait_for(lambda: command in media_log()["events"], "real MPRIS " + command + " method received")
                wait_for(lambda: (entry := state()["outputs"][0]["snapshot"]["media"])["title"] == title and entry["playback"] == playback, "MPRIS updated media state propagated")
            final = media_log()
            assert final["events"] == ["Next", "PlayPause", "Previous"], final
            (evidence / "mpris-methods.json").write_text(json.dumps(final, indent=2) + "\n")
            result["mpris"] = {"bridge_pid": bridge_pid, "bridge_sha256": executable_hash(bridge_pid), "service_pid": initial["pid"], "events": final["events"], "transport": "real private D-Bus MPRIS; no audio emitted"}
            record("native media Previous/PlayPause/Next traverse the real private D-Bus service")

        def sample_process(pid):
            raw = Path(f"/proc/{pid}/stat").read_text()
            fields = raw[raw.rfind(")") + 2:].split()
            status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in line)
            return {"pid": pid, "ticks": int(fields[11]) + int(fields[12]),
                    "rss_kib": int(status["VmRSS"].split()[0]),
                    "peak_process_lifetime_rss_kib": int(status["VmHWM"].split()[0]),
                    "voluntary_context_switches": int(status["voluntary_ctxt_switches"]),
                    "nonvoluntary_context_switches": int(status["nonvoluntary_ctxt_switches"])}

        def profile_idle(host, top):
            before = {"host": sample_process(host), "top": sample_process(top)}
            start = time.monotonic()
            time.sleep(1.2)
            elapsed = time.monotonic() - start
            after = {"host": sample_process(host), "top": sample_process(top)}
            metrics = {}
            for role, sample in after.items():
                previous = before[role]
                metrics[role] = {"pid": sample["pid"], "rss_kib": sample["rss_kib"],
                    "peak_process_lifetime_rss_kib": sample["peak_process_lifetime_rss_kib"],
                    "idle_cpu_percent": round((sample["ticks"] - previous["ticks"]) / os.sysconf("SC_CLK_TCK") / elapsed * 100, 3),
                    "voluntary_context_switches_per_second": round((sample["voluntary_context_switches"] - previous["voluntary_context_switches"]) / elapsed, 3),
                    "nonvoluntary_context_switches_per_second": round((sample["nonvoluntary_context_switches"] - previous["nonvoluntary_context_switches"]) / elapsed, 3)}
            result["resources"] = {"interval_seconds": round(elapsed, 3), "processes": metrics,
                "wakeups": "context-switch counters are separate; optional tracepoint results are in kernel_performance"}
            if os.environ.get("JWM_MEASURE_WAKEUPS") == "1":
                from shell_performance import measure
                result["kernel_performance"] = {"host": measure(host), "top": measure(top)}

        def subscription():
            stream = socket.socket(socket.AF_UNIX)
            stream.settimeout(3)
            stream.connect(str(work / "runtime/jwm-ipc.sock"))
            stream.sendall(b'{"subscribe":["desktop_shell/state"]}\n')
            reader = stream.makefile("r")
            acknowledgement = json.loads(reader.readline())
            assert acknowledgement["success"], acknowledgement
            return stream, reader

        def geometry(window):
            fields = dict(re.findall(r"^(\w+)=(\S+)$", run("xdotool", "getwindowgeometry", "--shell", window), re.M))
            return tuple(int(fields[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))

        def click(x, y):
            run("xdotool", "mousemove", x, y)
            time.sleep(0.15)
            run("xdotool", "click", "1")
            time.sleep(0.15)
            run("xdotool", "mousemove", width // 2, height // 2)

        def capture(name, final_display=False):
            destination = evidence / f"{name}.png"
            if final_display:
                # IPC screenshots intentionally run before modal chrome. Read
                # the final composited X11 display for menu visual acceptance.
                run("import", "-window", "root", destination)
                data = destination.read_bytes()
            else:
                queued = msg("take_screenshot_fullscreen")
                source = Path(queued["path"])
                assert source.is_relative_to(work / "pictures")
                wait_for(lambda: source.is_file() and source.stat().st_size > 32, "compositor screenshot")
                data = source.read_bytes()
                destination.write_bytes(data)
            assert data[:8] == b"\x89PNG\r\n\x1a\n" and struct.unpack(">II", data[16:24]) == (width, height)
            artifact_name = str(destination.relative_to(ROOT))
            if artifact_name not in result.setdefault("screenshots", []):
                result["screenshots"].append(artifact_name)
            result.setdefault("screenshot_sources", {})[artifact_name] = "composited_x11_display" if final_display else "compositor_ipc_scene"
            return destination

        def record(step):
            result["steps"].append(step)
            print("PASS:", step, flush=True)

        def exercise_workarea():
            client = spawn("xmessage", "-name", "shared-shell-client", "native fullscreen client")
            native_window = int(wait_for(lambda: run("xdotool", "search", "--name", "shared-shell-client").strip().splitlines()[0], "test client"))
            window = wait_for(lambda: next((item["id"] for item in msg("get_windows") if item["name"] == "shared-shell-client"), None), "WM test client identity")
            msg("focus_window", {"id": window})
            msg("maximize")
            maximized = wait_for(lambda: (w if (w := current_window(window))["is_maximized"] else None), "maximized client")
            assert maximized["x"] >= 90 and maximized["x"] + maximized["w"] <= width - 90, maximized
            assert maximized["y"] >= 42 and maximized["y"] + maximized["h"] <= height - 76, maximized
            before_strut = maximized["x"]
            dock = spawn("/usr/bin/python3", ROOT / "scripts/fixtures/shared_shell_dock.py", height)
            dock_window = int(wait_for(lambda: run("xdotool", "search", "--name", "^SharedExternalDock$").strip().splitlines()[0], "external dock probe"))
            dock_id = wait_for(lambda: next((item["id"] for item in msg("get_windows") if item["name"] == "SharedExternalDock" and item["is_dock"]), None), "WM external dock identity")
            wait_for(lambda: geometry(dock_window) == (0, 80, 60, max(100, height - 160)), "external dock physical geometry")
            wait_for(lambda: msg("get_struts")[0]["left"] >= 90, "external dock strut observed")
            assert abs(current_window(window)["x"] - before_strut) <= 8, (before_strut, current_window(window))
            dock.terminate()
            dock.wait(timeout=5)
            wait_for(lambda: all(item["id"] != dock_id for item in msg("get_windows")), "external dock retired")
            msg("focus_window", {"id": window})
            msg("maximize")
            wait_for(lambda: not current_window(window)["is_maximized"], "leave maximize")
            record("maximized clients avoid pinned edges and overlapping external struts are discounted")
            legacy.fullscreen(env["DISPLAY"], native_window, True)
            wait_for(lambda: not state()["outputs"][0]["visible"], "real fullscreen visibility")
            wait_for(lambda: all(not visible(p["window"]) for p in state()["host"]["panels"]), "fullscreen panels hidden")
            legacy.fullscreen(env["DISPLAY"], native_window, False)
            wait_for(panels_ready, "fullscreen exit restores shell")
            record("real client fullscreen hides and restores shared panels")
            set_config("desktop_shell.enabled", False)
            wait_for(lambda: state()["host"]["pid"] is None, "disabled host exits")
            assert not state()["host"]["panels"]
            monitor = msg("get_monitors")[0]
            assert monitor["wx"] == 0 and monitor["ww"] == width, monitor
            record("one-switch rollback removes host and workspace reservation")
            set_config("desktop_shell.enabled", True)
            wait_for(panels_ready, "re-enabled host")

        def exercise_topology():
            before_topology = state()
            retained_host = before_topology["host"]["pid"]
            prior_identities = {output["identity"] for output in before_topology["outputs"]}
            # RandR logical monitors exercise genuine monitor-resource
            # notifications in the private X server. The physical screen
            # remains available, so locking the new output never locks the
            # whole session and no password or authentication bypass is used.
            mm = re.search(r"dimensions:\s*\d+x\d+\s+pixels\s+\((\d+)x(\d+)\s+millimeters\)", run("xdpyinfo"))
            assert mm, "private X server physical size unavailable"
            physical_mm = (int(mm[1]), int(mm[2]))
            topology_announcements = 0
            def announce_topology():
                nonlocal topology_announcements
                topology_announcements += 1
                delta = topology_announcements % 2
                # Xvfb changes its GetMonitors inventory on SetMonitor but
                # emits no RandR event for that request. A real screen-mm
                # update emits SCREEN_CHANGE without altering any pixels.
                run("xrandr", "--fbmm", f"{physical_mm[0] + delta}x{physical_mm[1] + delta}")
            run("xrandr", "--setmonitor", "shared-extra", f"{width // 2}/170x{height}/210+{width // 2}+0", "none")
            announce_topology()
            topology = wait_for(lambda: (current if len((current := state())["outputs"]) == 2 and len(current["host"]["panels"]) == 6 else None), "RandR output added")
            prior = before_topology["outputs"][0]
            remapped = next(output for output in topology["outputs"] if output["id"] == prior["id"])
            if remapped["connector"] != prior["connector"]:
                assert remapped["identity"] != prior["identity"], "reused monitor slot retained authority over a replacement connector"
                rejected = msg("desktop_shell_action", {"session": topology["session"], "output": prior["id"],
                    "output_identity": prior["identity"], "command": {"ViewTag": {"tag": 0, "monitor": prior["id"]}}}, require_success=False)
                assert rejected.get("success") is False, rejected
            added = next(output for output in topology["outputs"] if output["identity"] not in prior_identities and output["id"] != prior["id"])
            added_id, old_identity = added["id"], added["identity"]
            assert len({(panel["output"], panel["role"]) for panel in topology["host"]["panels"]}) == 6
            assert topology["host"]["pid"] == retained_host
            msg("lock_monitor", {"value": added_id})
            wait_for(lambda: not next(output for output in state()["outputs"] if output["id"] == added_id)["visible"] and all(not visible(panel["window"]) for panel in state()["host"]["panels"] if panel["output"] == added_id), "monitor lock hides only its panels")
            run("xrandr", "--delmonitor", "shared-extra")
            announce_topology()
            wait_for(panels_ready, "output removal retires panels and stale lock")
            assert not msg("get_monitors")[0]["locked"]
            run("xrandr", "--setmonitor", "shared-extra", f"{width // 2}/170x{height}/210+{width // 2}+0", "none")
            announce_topology()
            replaced = wait_for(lambda: (current if len((current := state())["outputs"]) == 2 and len(current["host"]["panels"]) == 6 else None), "RandR output reconnected")
            reattached = next(output for output in replaced["outputs"] if output["id"] == added_id)
            assert reattached["identity"] != old_identity, (added, reattached)
            assert reattached["visible"]
            run("xrandr", "--delmonitor", "shared-extra")
            announce_topology()
            wait_for(panels_ready, "final output removal")
            assert state()["host"]["pid"] == retained_host
            result["topology"] = {"kind": "RandR logical output with real screen-change notification", "old_identity": old_identity, "new_identity": reattached["identity"], "physical_hotplug": "not exercised"}
            record("logical output hotplug, per-output locking and reused output IDs preserve one shared host")


        try:
            for path in (args.bin_dir / "jwm", args.bin_dir / "jwm-tool", args.shell_bin, bar_bin):
                assert path.is_file() and os.access(path, os.X_OK), f"missing executable: {path}"
            for executable in ("Xvfb", "xdotool", "xprop", "xwininfo", "dbus-run-session"):
                assert shutil.which(executable), f"missing dependency: {executable}"
            (work / "bin" / args.bar).symlink_to(bar_bin)
            if args.deep and args.second_bar != args.bar:
                assert second_bin.is_file(), second_bin
                (work / "bin" / args.second_bar).symlink_to(second_bin)
            read_fd, write_fd = os.pipe()
            try:
                spawn("Xvfb", "-displayfd", write_fd, "-screen", "0", f"{width}x{height}x24",
                      "-nolisten", "tcp", "-noreset", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                number = bytearray()
                deadline = time.monotonic() + 10
                while b"\n" not in number:
                    assert select.select([read_fd], [], [], max(0, deadline - time.monotonic()))[0], "Xvfb readiness"
                    chunk = os.read(read_fd, 32)
                    assert chunk, "Xvfb closed its readiness pipe"
                    number.extend(chunk)
                env["DISPLAY"] = ":" + number.decode().strip()
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            run(args.bin_dir / "jwm", "--backend", args.backend, "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            text = config.read_text()
            layout = "four-edge" if args.legacy_four_edge else "single"
            text = re.sub(r'(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)',
                          f'[status_bar]\nname="{args.bar}"\nshow_bar=true\nposition="top"\nlayout="{layout}"\n\n', text)
            text = re.sub(r'(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)', '', text)
            if not (args.legacy_four_edge or args.legacy_edge_shell):
                text += '\n[desktop_shell]\nenabled=true\nleft="always"\nright="always"\nbottom="always"\n'
            for key, value in {"wallpaper": json.dumps(str(ROOT / "assets/wallpapers/08-cloud-sea.png")),
                               "wallpaper_effect": '"off"', "wallpaper_colors": "false",
                               "status_bar_height": "42", "status_bar_padding": "5"}.items():
                text = re.sub(r'(?m)^' + key + r' = .*$', f'{key} = {value}', text)
            if args.ui_font:
                text = re.sub(r'(?m)^system_ui_font = .*$', 'system_ui_font = ' + json.dumps(args.ui_font), text)
                result["ui_font"] = args.ui_font
            ui_theme = "glass-dark" if args.theme == "dark" else "glass"
            text = re.sub(r'(?m)^ui_theme = .*$', 'ui_theme = "' + ui_theme + '"', text)
            config.write_text(text)
            run(args.bin_dir / "jwm", "--backend", args.backend, "--check-config")
            wm = spawn("dbus-run-session", "--", args.bin_dir / "jwm", "--backend", args.backend)
            initial = wait_for(panels_ready, "three registered/mapped panels", timeout=60)
            if args.legacy_four_edge:
                assert initial["source"] == "legacy_four_edge", initial
                record("legacy four-edge layout selects one shared pinned shell")
            if args.legacy_edge_shell:
                assert initial["source"] == "legacy_edge_shell", initial
                record("legacy xbar edge_shell selects the session-owned shared shell")
            host_pid = initial["host"]["pid"]
            primary_pid = wait_for(top_pid, "actual current frontend mapped", timeout=45)
            result.update(host_pid=host_pid, top_pid=primary_pid, initial_state=initial)
            wm_pid = int(next(line.split(":", 1)[1] for line in Path(f"/proc/{host_pid}/status").read_text().splitlines() if line.startswith("PPid:")))
            result["binary_sha256"] = {"frontend": executable_hash(primary_pid), "host": executable_hash(host_pid), "jwm": executable_hash(wm_pid)}
            wm_identity = legacy.process_identity(wm_pid)
            result["fixture_sha256"] = FIXTURE_SHA256
            record("current frontend top and three unique registered shared panels")
            monitor = msg("get_monitors")[0]
            assert monitor["wx"] > 0 and monitor["wy"] > 0 and monitor["ww"] < width and monitor["wh"] < height, monitor
            record("pinned panels reserve workspace on all three sides")
            # Prove an actual painted top survives, rather than accepting a
            # mapped transparent webview. The wallpaper is fixed and effects
            # are disabled, so this comparison isolates the primary bar strip.
            if args.bar.startswith("tauri_"):
                frontend_diagnostics = {}
                def frontend_ready():
                    for xid in run("xdotool", "search", "--pid", primary_pid).split():
                        raw = run("xprop", "-id", xid, "_JWM_FRONTEND_READY", "_JWM_FRONTEND_RENDERED", "_JWM_FRONTEND_BOOT", "_JWM_FRONTEND_ERROR")
                        frontend_diagnostics[xid] = raw
                        if re.search(r"_JWM_FRONTEND_READY\([^)]*\)\s*=\s*1\s*$", raw, re.M) and re.search(r"_JWM_FRONTEND_RENDERED\([^)]*\)\s*=\s*1\s*$", raw, re.M):
                            return True
                    return False
                try:
                    wait_for(frontend_ready, "Tauri frontend service and actual DOM-render acknowledgements", timeout=60)
                finally:
                    result["tauri_frontend_diagnostics"] = frontend_diagnostics
            msg("togglebar")
            wait_for(lambda: not state()["outputs"][0]["visible"], "hide for wallpaper reference")
            time.sleep(0.25)
            hidden_capture = capture("wallpaper-without-bars")
            msg("togglebar")
            wait_for(panels_ready, "shared panels restored after reference capture")
            wait_for(top_pid, "primary surface restored after reference capture")
            time.sleep(1.1)
            from PIL import Image, ImageChops
            region = (8, 8, width - 8, 44)
            wallpaper = Image.open(hidden_capture).convert("RGB").crop(region)
            changed_pixels = 0
            def top_painted():
                nonlocal changed_pixels
                visible_capture = capture("desktop")
                painted = Image.open(visible_capture).convert("RGB").crop(region)
                changed_pixels = sum(1 for pixel in ImageChops.difference(painted, wallpaper).getdata() if max(pixel) >= 12)
                return changed_pixels >= 80
            wait_for(top_painted, "primary bar remains mapped but has no painted content", timeout=40)
            result["top_render"] = {"changed_pixels": changed_pixels, "region": region,
                                    "tauri_frontend_ready": args.bar.startswith("tauri_"), "tauri_frontend_rendered": args.bar.startswith("tauri_")}
            record("primary toolkit surface paints visible content above the fixed wallpaper")
            if args.submenu_themes:
                def key_and_wait(key, kind):
                    # XTest can publish MappingNotify on connect. Match the
                    # shortcut fixture's delay while JWM reinstalls its grabs.
                    run("xdotool", "key", "--clearmodifiers", "--delay", "120", key)
                    last_ui = {}
                    def expected_panel():
                        nonlocal last_ui
                        last_ui = msg("get_system_ui")
                        return last_ui.get("kind") == kind
                    try:
                        wait_for(expected_panel, "native key opens " + str(kind))
                    except AssertionError as error:
                        raise AssertionError(f"{error}; observed UI={last_ui}") from error

                # The IPC reference captures announce completion with a
                # five-second toast; let it expire before reviewing menu chrome.
                time.sleep(5.2)
                desktop_display = Image.open(capture("desktop-display", final_display=True)).convert("RGB")
                menu_region = (width // 4, 48, 3 * width // 4, 3 * height // 4)
                def capture_menu(name):
                    destination = capture(name, final_display=True)
                    painted = Image.open(destination).convert("RGB").crop(menu_region)
                    changed = sum(1 for pixel in ImageChops.difference(painted, desktop_display.crop(menu_region)).getdata() if max(pixel) >= 16)
                    assert changed >= 1000, (name, "menu absent from final display capture", changed)
                    result.setdefault("menu_render", {})[name] = {"changed_pixels": changed, "region": menu_region}

                key_and_wait("alt+F10", "control_center")
                time.sleep(0.8)
                capture_menu("shell-hub")
                for shortcut, kind in [("t", "theme_picker"), ("a", "launcher"),
                                       ("n", "notification_center"), ("d", "calendar")]:
                    key_and_wait(shortcut, kind)
                    time.sleep(0.8)
                    capture_menu(kind)
                    key_and_wait("Escape", "control_center")
                record("native Hub children open and Escape returns to Hub")
                key_and_wait("t", "theme_picker")
                # Glass and GlassDark are adjacent in the public Theme picker.
                next_theme = "Light" if args.theme == "dark" else "Dark"
                run("xdotool", "key", "Up" if args.theme == "dark" else "Down")
                run("xdotool", "key", "Return")
                wait_for(lambda: state()["outputs"][0]["snapshot"]["theme"] == next_theme,
                         "theme choice reaches shared edge snapshot")
                assert state()["host"]["pid"] == host_pid, "theme selection must preserve shared host"
                wait_for(lambda: not msg("get_system_ui")["active"], "applying a theme closes the picker")
                key_and_wait("alt+F10", "control_center")
                key_and_wait("t", "theme_picker")
                time.sleep(0.8)
                capture_menu("theme-switched")
                # Theme selection persists only into this fixture's private TOML.
                assert re.search(r'(?m)^ui_theme\s*=\s*"' + ("glass" if args.theme == "dark" else "glass-dark") + '"', config.read_text())
                key_and_wait("Escape", "control_center")
                key_and_wait("Escape", None)
                set_config("appearance.ui_theme", ui_theme)
                wait_for(lambda: state()["outputs"][0]["snapshot"]["theme"] == ("Dark" if args.theme == "dark" else "Light"),
                         "initial fixture theme restored")
                assert state()["host"]["pid"] == host_pid
                record("Theme picker updates menu and edge theme live, persists choice and keeps shared host PID")
            profile_idle(host_pid, primary_pid)
            # Workspace selection through the physical left panel, rather than
            # an IPC substitute: the second workspace is a shared painter cell.
            left = next(p for p in initial["host"]["panels"] if p["role"] == "left")
            x, y, w, h = geometry(left["window"])
            def workspace_control():
                raw = run("xprop", "-id", left["window"], "-notype", "_JWM_EDGE_CONTROLS")
                controls = json.loads(json.loads(raw.split("=", 1)[1].strip()))
                return next((c for c in controls if c["kind"] == "workspace" and c["index"] == 1), None)
            control = wait_for(workspace_control, "actual second workspace hit region")
            cx, cy, cw, ch = control["rect"]
            run("xdotool", "mousemove", x + int(cx + cw / 2), y + int(cy + ch / 2))
            time.sleep(0.15)
            interaction_start = time.monotonic()
            run("xdotool", "click", "1")
            wait_for(lambda: state()["outputs"][0]["snapshot"]["active_tag"] == 1, "native workspace click applied")
            result.setdefault("latency_ms", {})["workspace_click_roundtrip"] = round((time.monotonic() - interaction_start) * 1000, 3)
            run("xdotool", "mousemove", width // 2, height // 2)
            assert state()["host"]["pid"] == host_pid
            record("native shared-panel workspace click changes JWM workspace")
            bad = msg("desktop_shell_action", {"session": initial["session"] + 1, "output": 0,
                      "name": "focus_window", "args": {"window": 0}}, require_success=False)
            assert bad.get("success") is False, bad
            record("old-session actions rejected")
            if args.topology_only:
                exercise_topology()
            if args.workarea_only:
                exercise_workarea()
            if args.deep:
                exercise_provider_controls()
                exercise_media_controls()
                # The first three painted left controls are Apps, Files and
                # Terminal. Launch through the actual Terminal hit region;
                # PATH selects a private foreground application instead of a
                # real desktop terminal or shell command.
                current = state()
                left_panel = next(panel for panel in current["host"]["panels"] if panel["role"] == "left")
                controls = edge_controls(left_panel)
                launcher = [control for control in controls if control["kind"] == "control"][2]
                px, py, _, _ = geometry(left_panel["window"])
                lx, ly, lw, lh = launcher["rect"]
                click(px + int(lx + lw / 2), py + int(ly + lh / 2))
                launched_pid = int(wait_for(lambda: (work / "launched-application.pid").read_text().strip() if (work / "launched-application.pid").is_file() else None, "shared Terminal action launched the private application"))
                launched_application = legacy.process_identity(launched_pid)
                assert launched_application
                launched_native = int(wait_for(lambda: run("xdotool", "search", "--name", "^SharedLaunchedTerminal$").strip().splitlines()[0], "launched application window"))
                result["launched_application"] = {"pid": launched_pid, "native_window": launched_native}
                record("native shared Terminal action launches an independent application")
                if args.second_bar != args.bar:
                    switch_started = time.monotonic()
                    result["frontend_switch_resources"] = {"host_before": sample_process(host_pid), "top_before": sample_process(primary_pid)}
                    set_config("status_bar.name", args.second_bar)
                    wait_for(lambda: legacy.process_identity(primary_pid) is None, "old top reaped")
                    assert wait_for(panels_ready, "panels after frontend switch")["host"]["pid"] == host_pid
                    set_config("status_bar.name", args.bar)
                    primary_pid = wait_for(top_pid, "original frontend restored")
                    result["frontend_switch_resources"].update(host_after=sample_process(host_pid), top_after=sample_process(primary_pid))
                    result.setdefault("latency_ms", {})["frontend_switch_roundtrip"] = round((time.monotonic() - switch_started) * 1000, 3)
                    assert state()["host"]["pid"] == host_pid
                    record("frontend switches preserve shared host PID")
                os.kill(primary_pid, signal.SIGKILL)
                wait_for(lambda: (pid := top_pid()) and pid != primary_pid, "top crash recovery", timeout=45)
                assert state()["host"]["pid"] == host_pid
                record("top crash recovers without restarting sidebars")
                os.kill(host_pid, signal.SIGKILL)
                recovered = wait_for(lambda: (s := panels_ready()) and s["host"]["pid"] != host_pid and s,
                                     "host crash recovery", timeout=25)
                host_pid = recovered["host"]["pid"]
                assert legacy.process_identity(launched_pid) == launched_application
                assert visible(launched_native)
                record("host crash re-registers one replacement and leaves its launched application alive")
                stalled = state()
                stalled_panels = {panel["role"]: panel["window"] for panel in stalled["host"]["panels"]}
                stalled_host_identity = legacy.process_identity(host_pid)
                assert legacy.process_identity(wm_pid) == wm_identity
                os.kill(wm_pid, signal.SIGSTOP)
                try:
                    time.sleep(12)
                    assert legacy.process_identity(host_pid) == stalled_host_identity
                    assert all(visible(window) for window in stalled_panels.values())
                finally:
                    if legacy.process_identity(wm_pid) == wm_identity:
                        os.kill(wm_pid, signal.SIGCONT)
                resumed = wait_for(panels_ready, "host resynchronizes after blocked WM observer")
                assert resumed["host"]["pid"] == host_pid
                assert {panel["role"]: panel["window"] for panel in resumed["host"]["panels"]} == stalled_panels
                record("twelve-second WM pause preserves the same live host and native panel windows")
                for edge in ("left", "right", "bottom"):
                    set_config("desktop_shell." + edge, "hover")
                wait_for(lambda: all(not visible(p["window"]) for p in state()["host"]["panels"]), "hover hides panels")
                hover_monitor = msg("get_monitors")[0]
                assert hover_monitor["wx"] == 0 and hover_monitor["ww"] == width, hover_monitor
                run("xdotool", "mousemove", 0, height // 2)
                wait_for(lambda: visible(next(p["window"] for p in state()["host"]["panels"] if p["role"] == "left")), "edge sensor reveals left")
                run("xdotool", "mousemove", width // 2, height // 2)
                wait_for(lambda: all(not visible(p["window"]) for p in state()["host"]["panels"]), "hover exit hides panel")
                record("live hover modes reveal independently without workspace reservation")
                if args.legacy_edge_shell:
                    legacy_file = work / "xbar.toml"
                    valid_text = legacy_file.read_text()
                    legacy_file.write_text(valid_text + "this is invalid TOML [")
                    time.sleep(1.3)
                    assert state()["config"]["left"] == "hover"
                    assert state()["host"]["pid"] == host_pid
                    legacy_file.write_text(valid_text)
                    record("invalid legacy edits preserve the last valid modes and host")
                for edge in ("left", "right", "bottom"):
                    set_config("desktop_shell." + edge, "always")
                wait_for(panels_ready, "pinned modes restored")
                # Both task cells carry WM identities and actual painted hit
                # rectangles. Restore through pointer input, then ensure the
                # previous minimized projection is rejected after re-minimize.
                task_ids = []
                for name in ("SharedDockFirst", "SharedDockSecond"):
                    spawn("xmessage", "-name", name, "-title", name, "shared shell task")
                    task = wait_for(lambda: next((item for item in msg("get_windows") if item["name"] == name), None), name)
                    task_ids.append(task["id"])
                    msg("focus_window", {"id": task["id"]})
                    wait_for(lambda: current_window(task["id"])["is_focused"], "task focus")
                    msg("minimize")
                    wait_for(lambda: current_window(task["id"])["is_minimized"], "task minimize")
                overflow_ids = []
                for index in range(3):
                    name = "SharedOverflow" + str(index)
                    spawn("xmessage", "-name", name, "-title", name, "shared task overflow")
                    overflow_ids.append(wait_for(lambda: next((item["id"] for item in msg("get_windows") if item["name"] == name), None), name))
                time.sleep(1.1)
                target = wait_for(lambda: seek_task_control(overflow_ids[-1]), "overflow control realizes the last real task")
                click(target[0], target[1])
                wait_for(lambda: current_window(overflow_ids[-1])["is_focused"], "overflow task focus")
                assert result.get("overflow_activations", 0) > 0, "real overflow control was not exercised"
                record("native task overflow reaches and focuses a real offscreen task")
                stale_snapshot = state()
                stale_generation = stale_snapshot["outputs"][0]["snapshot"]["wm_sequence"]
                for task_id in task_ids:
                    target = wait_for(lambda: seek_task_control(task_id), "native task control")
                    tx, ty, anchor = target
                    assert 0 <= anchor[0] < width and 0 <= anchor[1] < height
                    assert anchor[0] + anchor[2] <= width and anchor[1] + anchor[3] <= height, anchor
                    run("xdotool", "mousemove", width // 2, height // 2)
                    time.sleep(0.45)
                    before_preview = capture("task-before-preview-" + str(task_id))
                    run("xdotool", "mousemove", tx, ty)
                    time.sleep(0.55)
                    preview_region = (100, max(50, height - 400), width - 100, height - 100)
                    reference = Image.open(before_preview).convert("RGB").crop(preview_region)
                    preview_pixels = 0
                    def preview_painted():
                        nonlocal preview_pixels
                        painted = Image.open(capture("task-preview-" + str(task_id))).convert("RGB").crop(preview_region)
                        preview_pixels = sum(max(pixel) >= 12 for pixel in ImageChops.difference(reference, painted).getdata())
                        return preview_pixels > 5000
                    wait_for(preview_painted, "native minimized task preview contains actual captured client pixels", timeout=12)
                    result.setdefault("task_preview_pixels", {})[str(task_id)] = preview_pixels
                    interaction_start = time.monotonic()
                    run("xdotool", "click", "1")
                    wait_for(lambda: not current_window(task_id)["is_minimized"] and current_window(task_id)["is_focused"], "native dock restore and focus")
                    result.setdefault("latency_ms", {})["task_restore_" + str(task_id)] = round((time.monotonic() - interaction_start) * 1000, 3)
                    run("xdotool", "mousemove", width // 2, height // 2)
                    msg("focus_window", {"id": task_id})
                    wait_for(lambda: current_window(task_id)["is_focused"], "restored task refocus after pointer leaves Dock")
                    msg("minimize")
                    wait_for(lambda: current_window(task_id)["is_minimized"], "task re-minimize")
                latest = state()
                stale = msg("desktop_shell_action", {"session": latest["session"], "output": 0,
                    "output_identity": latest["outputs"][0]["identity"],
                    "command": {"RestoreWindow": {"window": task_ids[0], "wm_session_id": latest["session"],
                        "minimized_generation": stale_generation, "monitor": 0,
                        "geometry": {"x": 200, "y": height - 70, "width": 80, "height": 40}}}}, require_success=False)
                assert stale.get("success") is False, stale
                assert current_window(task_ids[0])["is_minimized"]
                stale_output = msg("desktop_shell_action", {"session": latest["session"], "output": 0,
                    "output_identity": latest["outputs"][0]["identity"] + 1,
                    "command": {"ViewTag": {"tag": 0, "monitor": 0}}}, require_success=False)
                assert stale_output.get("success") is False, stale_output
                record("both real task controls preview/restore; stale window and output generations rejected")
                if args.second_bar != args.bar:
                    stable = state()
                    generation = stable["outputs"][0]["snapshot"]["wm_sequence"]
                    stable_panels = {panel["role"]: panel["window"] for panel in stable["host"]["panels"]}
                    set_config("status_bar.name", args.second_bar)
                    wait_for(lambda: any(top["pid"] and Path(f"/proc/{top['pid']}/exe").resolve() == second_bin for top in state()["host"]["top_bars"]), "second top with minimized tasks")
                    assert state()["outputs"][0]["snapshot"]["wm_sequence"] == generation
                    set_config("status_bar.name", args.bar)
                    wait_for(top_pid, "original top with minimized tasks")
                    after = state()
                    assert after["outputs"][0]["snapshot"]["wm_sequence"] == generation
                    assert {panel["role"]: panel["window"] for panel in after["host"]["panels"]} == stable_panels
                    record("frontend replacement preserves the shared Dock projection and native panel windows")

                retained_host = state()["host"]["pid"]
                msg("togglebar")
                wait_for(lambda: not state()["outputs"][0]["visible"] and all(not visible(panel["window"]) for panel in state()["host"]["panels"]), "workspace bars hidden")
                msg("view", {"value": 4})
                wait_for(panels_ready, "another workspace retains its bar preference")
                msg("view", {"value": 2})
                wait_for(lambda: not state()["outputs"][0]["visible"], "hidden workspace preference retained")
                msg("togglebar")
                wait_for(panels_ready, "workspace bar preference restored")
                assert state()["host"]["pid"] == retained_host
                record("per-workspace bar visibility restores the same shared process")
                exercise_topology()

                stream, reader = subscription()
                try:
                    initial_subscription = state()
                    set_config("desktop_shell.right", "hover")
                    deadline = time.monotonic() + 10
                    while True:
                        event = json.loads(reader.readline())
                        if event.get("event") == "desktop_shell/state" and event["payload"]["config"]["right"] == "hover":
                            break
                        assert time.monotonic() < deadline, event
                    assert event["payload"]["session"] == initial_subscription["session"]
                    assert event["payload"]["revision"] > initial_subscription["revision"]
                    result["subscription_revision"] = event["payload"]["revision"]
                finally:
                    reader.close()
                    stream.close()
                set_config("desktop_shell.right", "always")
                # Legacy xbar edits are observed by the health timer. A
                # currently revealed hover panel can already be mapped before
                # the requested pinned mode has reached the WM snapshot.
                wait_for(lambda: state()["config"]["right"] == "always" and panels_ready(),
                         "subscription reconfiguration restored")
                stream, reader = subscription()
                try:
                    synchronized = state()
                    assert synchronized["config"]["right"] == "always"
                    assert synchronized["revision"] > result["subscription_revision"]
                finally:
                    reader.close()
                    stream.close()
                assert state()["host"]["pid"] == retained_host
                record("independent subscription revision ordering and disconnect/full resynchronization")

                exercise_workarea()
                before_restart = state()
                old_session, old_host_pid = before_restart["session"], before_restart["host"]["pid"]
                old_host_identity = legacy.process_identity(old_host_pid)
                preserved_tasks = {item["name"]: item["is_minimized"] for item in msg("get_windows") if item["name"].startswith("Shared") or item["name"] == "shared-shell-client"}
                assert len(preserved_tasks) >= 6, preserved_tasks
                msg("restart")
                restarted = wait_for(lambda: (current if (current := panels_ready()) and current["session"] != old_session and current["host"]["pid"] != old_host_pid else None), "real JWM exec restart resynchronizes a new shell session", timeout=60)
                wait_for(lambda: legacy.process_identity(old_host_pid) != old_host_identity, "old shared host reaped after JWM restart")
                assert legacy.process_identity(launched_pid) == launched_application
                assert visible(launched_native)
                after_tasks = {item["name"]: item["is_minimized"] for item in msg("get_windows") if item["name"] in preserved_tasks}
                assert after_tasks == preserved_tasks, (preserved_tasks, after_tasks)
                stale = msg("desktop_shell_action", {"session": old_session, "output": 0,
                    "output_identity": before_restart["outputs"][0]["identity"],
                    "name": "focus_window", "args": {"id": 0}}, require_success=False)
                assert stale.get("success") is False, stale
                wait_for(lambda: state()["outputs"][0]["snapshot"]["media"]["title"] == "Shared sidebar media track 0", "private MPRIS bridge reconnects after JWM restart")
                result["restart"] = {"old_session": old_session, "new_session": restarted["session"], "old_host_pid": old_host_pid, "new_host_pid": restarted["host"]["pid"], "preserved_tasks": after_tasks}
                record("real JWM restart reaps the old shell, adopts tasks and rejects the old session")
            result["status"] = "passed"
        except Exception as error:
            result.update(status="failed", reason=str(error))
            diagnostics = {}
            for name, probe in (("shell", state), ("monitors", lambda: msg("get_monitors")), ("randr", lambda: run("xrandr", "--listmonitors")), ("randr_active", lambda: run("xrandr", "--listactivemonitors"))):
                try:
                    diagnostics[name] = probe()
                except Exception as failure:
                    diagnostics[name] = str(failure)
            result["failure_diagnostics"] = diagnostics
            raise
        finally:
            cleanup_errors = []
            for identity in reversed(service_identities):
                if legacy.process_identity(identity[0]) == identity:
                    try:
                        os.kill(identity[0], signal.SIGTERM)
                    except ProcessLookupError:
                        pass
            if launched_application and legacy.process_identity(launched_application[0]) == launched_application:
                try:
                    os.kill(launched_application[0], signal.SIGTERM)
                except ProcessLookupError:
                    pass
            for process in reversed(children):
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                        process.wait(timeout=3)
                    except (ProcessLookupError, subprocess.TimeoutExpired):
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        try:
                            # GLX Xvfb can remain in an uninterruptible driver
                            # teardown briefly after SIGKILL. Reap its exact
                            # Child handle before reporting the final result.
                            process.wait(timeout=20)
                        except subprocess.TimeoutExpired:
                            cleanup_errors.append({"pid": process.pid, "command": list(map(str, process.args))})
            try:
                mount_cleanup = release_private_gvfs_mount(work)
                if mount_cleanup:
                    result["private_mount_cleanup"] = mount_cleanup
            except Exception as error:
                cleanup_errors.append({"private_mount": str(work / "runtime" / "gvfs"),
                                       "error": str(error)})
            log.close()
            result["duration_seconds"] = round(time.monotonic() - started, 3)
            if cleanup_errors:
                result["cleanup_errors"] = cleanup_errors
                if result["status"] == "passed":
                    result.update(status="failed", reason="private fixture cleanup did not complete")
            (evidence / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            if cleanup_errors and result.get("reason") == "private fixture cleanup did not complete":
                raise AssertionError(result["reason"])
    print(f"PASS: {args.bar}/{args.backend}; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    main()
