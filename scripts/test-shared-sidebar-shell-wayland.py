#!/usr/bin/env python3
"""Exercise actual native layer-shell surfaces in a private nested JWM session.

The outer Xvfb display only supplies a nested output and physical pointer input;
all three panels and the test task use the real Wayland protocols. XWayland is
checked separately with xmessage. Saves compositor screenshots and raw state.
DRM/KMS physical-session acceptance remains a distinct matrix, never inferred
from this nested run. Build jwm with backend-wayland-nested and jwm_shell's
native_shell_client example first.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=("wayland-x11", "wayland-winit"), default="wayland-winit")
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--shell-bin", type=Path, default=ROOT / "crates/jwm_shell/target/debug/jwm-shell")
    parser.add_argument("--client-bin", type=Path, default=ROOT / "crates/jwm_shell/target/debug/examples/native_shell_client")
    parser.add_argument("--bar", default="tauri_leptos_bar")
    parser.add_argument("--bar-bin", type=Path)
    parser.add_argument("--second-bar", default="tao_pixels_bar")
    parser.add_argument("--second-bar-bin", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--theme", choices=("light", "dark"), default="light")
    parser.add_argument("--host-scale", type=float, default=1.0, help="Outer host DPI; the inner compositor output keeps its independent scale")
    parser.add_argument("--evidence-dir", type=Path, default=ROOT / "artifacts/shared-shell-wayland")
    args = parser.parse_args()
    width, height = map(int, args.size.split("x"))
    directory = ROOT / "bars" / args.bar
    default_bar = directory / ("src-tauri/target/debug" if (directory / "src-tauri").is_dir() else "target/debug") / args.bar
    bar = (args.bar_bin or default_bar).resolve()
    second = (args.second_bar_bin or ROOT / "bars" / args.second_bar / "target/debug" / args.second_bar).resolve()
    evidence = (args.evidence_dir / f"{args.backend}-{args.bar}-{args.size}-{args.theme}-host{args.host_scale:g}x").resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    result = {"backend": args.backend, "bar": args.bar, "size": [width, height], "theme": args.theme,
              "host_scale": args.host_scale, "transport": "native_wayland_layer_shell", "status": "running", "steps": [], "screenshots": []}
    children = []
    launched_application = None
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="jwm-native-shell-") as temporary:
        work = Path(temporary)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11", NO_AT_BRIDGE="1",
                   JWM_SHELL_BIN=str(args.shell_bin.resolve()), WINIT_X11_SCALE_FACTOR=str(args.host_scale))
        for name in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "JWM_SOCKET",
                     "JWM_BACKEND", "JWM_DESKTOP_SHELL_BACKEND", "JWM_DESKTOP_SHELL", "JWM_BAR_LAYOUT"):
            env.pop(name, None)
        for name in ("CONFIG", "CACHE", "DATA", "STATE", "RUNTIME", "PICTURES"):
            path = work / name.lower()
            path.mkdir(mode=0o700)
            env[f"XDG_{name}_HOME" if name in ("CONFIG", "CACHE", "DATA", "STATE") else f"XDG_{name}_DIR"] = str(path)
        env["XBAR_CONFIG"] = str(work / "xbar.toml")
        (work / "xbar.toml").write_text(f'theme="{args.theme}"\n[edge_shell]\nenabled=true\n')
        (work / "bin").mkdir()
        env["PATH"] = str(work / "bin") + os.pathsep + env.get("PATH", "")
        env["JWM_TEST_LAUNCH_PID"] = str(work / "launched-application.pid")
        terminal = work / "bin/x-terminal-emulator"
        terminal.write_text("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$JWM_TEST_LAUNCH_PID\"\nexec xmessage -name SharedNativeLaunchedTerminal -title SharedNativeLaunchedTerminal 'native shared shell application survival' 2> \"$JWM_TEST_LAUNCH_PID.stderr\"\n")
        terminal.chmod(0o700)
        log = (evidence / "session.log").open("w+")

        def run(*argv, extra_env=None):
            return subprocess.check_output(list(map(str, argv)), env=env if extra_env is None else dict(env, **extra_env),
                                           text=True, stderr=subprocess.STDOUT, timeout=20)

        def spawn(*argv, extra_env=None, **kwargs):
            child = subprocess.Popen(list(map(str, argv)), env=env if extra_env is None else dict(env, **extra_env),
                                     stdout=log, stderr=log, start_new_session=True, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate, label, timeout=25):
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

        def msg(name, values=None, success=True):
            argv = [args.bin_dir / "jwm-tool", "msg", name, "--raw"]
            if values is not None:
                argv += ["--args", json.dumps(values)]
            response = json.loads(run(*argv))
            if not success:
                return response
            assert response.get("success", True), response
            return response.get("data", response)

        def state():
            return msg("get_desktop_shell")

        def ready():
            current = state()
            if not current["host"]["ready"] or len(current["host"]["panels"]) != 3:
                return None
            rows = {row["id"]: row for row in msg("get_windows")}
            for panel in current["host"]["panels"]:
                row = rows.get(panel["window"])
                if row is None:
                    return None
                expected = f'jwm-shell-{current["session"]}-{panel["output"]}-{panel["role"]}'
                assert row["name"].startswith(expected + "-") and row["class"] == row["name"], row
                assert row["is_dock"], row
            assert current["host"]["backend"].startswith("wayland"), current
            return current

        def set_config(key, value):
            # Assignments are idempotent; a bounded IPC deadline may expire
            # while a WebKit top is being reaped. Retry only transport failures,
            # retaining their exact output in evidence, never rejected values.
            for attempt in range(3):
                try:
                    return msg("set_config", {"key": key, "value": value})
                except subprocess.CalledProcessError as error:
                    output = str(error.output)
                    result.setdefault("transport_retries", []).append({"key": key, "attempt": attempt + 1, "output": output[-2000:]})
                    transient = any(word in output.lower() for word in ("timed out", "timeout", "deadline", "temporarily unavailable", "connection reset", "broken pipe"))
                    if not transient or attempt == 2:
                        raise
                    time.sleep(0.1)

        def controls(edge, kind, identity):
            rows = json.loads(run(args.shell_bin, "--inspect-controls"))
            row = next(item for item in rows if item["output"] == 0 and item["edge"] == edge)
            key = "index" if kind == "workspace" else "id"
            return next(item["rect"] for item in row["controls"] if item["kind"] == kind and item.get(key) == identity)

        def pointer(x, y, click=False):
            run("xdotool", "windowfocus", outer)
            run("xdotool", "mousemove", "--window", outer, int(x), int(y))
            time.sleep(0.15)
            if click:
                run("xdotool", "click", "1")

        def capture(name):
            source = Path(msg("take_screenshot_fullscreen")["path"])
            assert source.is_relative_to(work / "pictures"), source
            wait_for(lambda: source.is_file() and source.stat().st_size > 32, "compositor screenshot")
            data = source.read_bytes()
            assert data[:8] == b"\x89PNG\r\n\x1a\n" and struct.unpack(">II", data[16:24]) == (width, height)
            destination = evidence / f"{name}.png"
            destination.write_bytes(data)
            result["screenshots"].append(str(destination.relative_to(ROOT)))
            return destination

        def changed_pixels(first, second, rectangle):
            from PIL import Image, ImageChops
            difference = ImageChops.difference(Image.open(first).convert("RGB").crop(rectangle),
                                              Image.open(second).convert("RGB").crop(rectangle))
            return sum(pixel != (0, 0, 0) for pixel in difference.getdata())

        def record(step):
            result["steps"].append(step)
            print("PASS:", step, flush=True)

        def task(identity):
            return next(row for row in msg("get_windows") if row["id"] == identity)

        def process_identity(pid):
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
                return (pid, int(fields[19])) if fields[0] != "Z" else None
            except (OSError, IndexError, ValueError):
                return None

        xprobe_sequence = 0

        def inner_xinfo(title, refresh=False):
            nonlocal xprobe_sequence
            xprobe_sequence += 1
            destination = work / f"xprobe-{xprobe_sequence}.json"
            code = r"""import json,re,subprocess,sys,time
from pathlib import Path
windows=subprocess.check_output(['xdotool','search','--name','^'+re.escape(sys.argv[2])+'$'],text=True).splitlines()
window=windows[0]
if sys.argv[3]=='refresh':
    import ctypes,ctypes.util
    x11=ctypes.CDLL(ctypes.util.find_library('X11'))
    x11.XOpenDisplay.argtypes=[ctypes.c_char_p];x11.XOpenDisplay.restype=ctypes.c_void_p
    display=x11.XOpenDisplay(None)
    class Expose(ctypes.Structure):
        _fields_=[('type',ctypes.c_int),('serial',ctypes.c_ulong),('send_event',ctypes.c_int),('display',ctypes.c_void_p),('window',ctypes.c_ulong),('x',ctypes.c_int),('y',ctypes.c_int),('width',ctypes.c_int),('height',ctypes.c_int),('count',ctypes.c_int)]
    event=(ctypes.c_long*24)();expose=Expose.from_buffer(event)
    expose.type=12;expose.display=display;expose.window=int(window);expose.width=100;expose.height=40
    x11.XSendEvent.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.c_int,ctypes.c_long,ctypes.c_void_p]
    x11.XSendEvent(display,int(window),0,1<<15,ctypes.byref(event))
    x11.XFlush.argtypes=[ctypes.c_void_p];x11.XFlush(display)
    x11.XCloseDisplay.argtypes=[ctypes.c_void_p];x11.XCloseDisplay(display)
    time.sleep(0.2)
raw=subprocess.check_output(['xdotool','getwindowgeometry','--shell',window],text=True)
fields=dict(re.findall(r'^(\w+)=(\S+)$',raw,re.M))
Path(sys.argv[1]).write_text(json.dumps({'window':int(window),'geometry':[int(fields[key]) for key in ('X','Y','WIDTH','HEIGHT')]}))
"""
            msg("spawn", {"cmd": ["python3", "-c", code, str(destination), title, "refresh" if refresh else "query"]})
            wait_for(lambda: destination.is_file(), "actual inner XWayland geometry probe")
            return json.loads(destination.read_text())

        try:
            for path in (args.bin_dir / "jwm", args.bin_dir / "jwm-tool", args.shell_bin, args.client_bin, bar, second):
                assert path.is_file() and os.access(path, os.X_OK), f"missing executable: {path}"
            versions = {}
            for role, path in {"jwm": args.bin_dir / "jwm", "jwm-tool": args.bin_dir / "jwm-tool",
                               "host": args.shell_bin, "native_client": args.client_bin,
                               "primary_bar": bar, "replacement_bar": second}.items():
                with path.open("rb") as executable:
                    digest = hashlib.sha256()
                    for chunk in iter(lambda: executable.read(1024 * 1024), b""):
                        digest.update(chunk)
                    versions[role] = {"path": str(path.resolve()), "sha256": digest.hexdigest()}
            (evidence / "binaries.sha256.json").write_text(json.dumps(versions, indent=2) + "\n")
            for name in ("Xvfb", "xdotool", "xmessage", "dbus-run-session"):
                assert shutil.which(name), f"missing dependency: {name}"
            (work / "bin" / args.bar).symlink_to(bar)
            if args.second_bar != args.bar:
                (work / "bin" / args.second_bar).symlink_to(second)
            read_fd, write_fd = os.pipe()
            try:
                spawn("Xvfb", "-displayfd", write_fd, "-screen", "0", f"{width}x{height}x24", "-nolisten", "tcp", "-noreset", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                assert select.select([read_fd], [], [], 10)[0], "Xvfb readiness"
                env["DISPLAY"] = ":" + os.read(read_fd, 32).decode().strip()
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            run(args.bin_dir / "jwm", "--backend", args.backend, "--gen-config")
            config = work / "config/jwm/config_wayland.toml"
            text = config.read_text()
            text = re.sub(r'(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)', f'[status_bar]\nname="{args.bar}"\nshow_bar=true\nposition="top"\nlayout="single"\n\n', text)
            text = re.sub(r'(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)', '', text)
            text += '\n[desktop_shell]\nenabled=true\nleft="always"\nright="always"\nbottom="always"\n'
            for key, value in {"wallpaper": json.dumps(str(ROOT / "assets/wallpapers/08-cloud-sea.png")),
                               "wallpaper_effect": '"off"', "wallpaper_colors": "false",
                               "status_bar_height": "42", "status_bar_padding": "5"}.items():
                text = re.sub(r'(?m)^' + key + r' = .*$', f'{key} = {value}', text)
            config.write_text(text)
            run(args.bin_dir / "jwm", "--backend", args.backend, "--check-config")
            spawn("dbus-run-session", "--", args.bin_dir / "jwm", "--backend", args.backend)
            outer = wait_for(lambda: run("xdotool", "search", "--name", "JWM ").splitlines()[0], "nested output window")
            # Xvfb has no parent WM to honor winit's EWMH fullscreen request.
            # Set this private nested output to the physical matrix dimensions
            # explicitly, including when the outer toolkit DPI is2x.
            run("xdotool", "windowsize", outer, width, height)
            wait_for(lambda: any(row["w"] == width and row["h"] == height for row in msg("get_monitors")), "nested physical output dimensions")
            initial = wait_for(ready, "three registered native Wayland panels", timeout=60)
            wait_for(lambda: any(row.get("window") and row.get("pid") and Path(f'/proc/{row["pid"]}/exe').resolve() == bar
                                 for row in state()["host"]["top_bars"]), "actual selected top frontend mapped", timeout=45)
            initial = ready()
            host_pid = initial["host"]["pid"]
            assert initial["outputs"][0]["scale"] == 1.0, "Inner output scale must match native wl_output, independently of host DPI"
            result["initial_state"] = initial
            result["initial_windows"] = msg("get_windows")
            windows = {row["id"]: row for row in result["initial_windows"]}
            expected = {"left": (0, 0, 90, height), "right": (width - 90, 0, 90, height),
                        "bottom": (0, height - 76, width, 76)}
            for panel in initial["host"]["panels"]:
                row = windows[panel["window"]]
                assert tuple(row[key] for key in ("x", "y", "w", "h")) == expected[panel["role"]], row
            sensors = [row for row in windows.values() if row["name"].startswith("jwm-shell-") and row["name"].endswith("-sensor")]
            assert len(sensors) == 3, sensors
            assert {(row["x"], row["y"], row["w"], row["h"]) for row in sensors} == {(0, 0, 2, height), (width - 2, 0, 2, height), (0, height - 2, width, 2)}, sensors
            monitor = msg("get_monitors")[0]
            assert monitor["wx"] == 90 and monitor["ww"] == width - 180, monitor
            assert 0 < monitor["wy"] < height and 0 < monitor["wh"] < height - 76, monitor
            from PIL import Image
            header_attempt = 0
            def header_painted():
                nonlocal header_attempt
                header_attempt += 1
                pinned = capture("native-pinned-" + str(header_attempt))
                header = Image.open(pinned).convert("RGB").crop((5, 5, width - 5, 47))
                pixels = sum(sum(pixel) > 30 for pixel in header.getdata())
                colored = sum(max(pixel) > 80 and max(pixel) - min(pixel) > 30 for pixel in header.getdata())
                result["header_visible_pixels"] = pixels
                result["header_colored_widget_pixels"] = colored
                # The monochrome Leptos/React Loading fallback is not an
                # operational primary bar. Require its real workspace/status
                # controls, whose selected state has an explicit accent color.
                return pixels > 200 and colored > 100
            wait_for(header_painted, "mapped primary frontend paints visible header pixels", timeout=25)
            record("native layer-shell roles and exclusive workspace depths, without duplicate reservations")
            rect = controls("left", "workspace", 1)
            pointer(rect[0] + rect[2] / 2, rect[1] + rect[3] / 2, True)
            wait_for(lambda: state()["outputs"][0]["snapshot"]["active_tag"] == 1, "native pointer workspace action")
            record("physical pointer enters the real native panel and changes workspace")
            sockets = wait_for(lambda: list((work / "runtime").glob("wayland-*")), "native Wayland socket")
            wayland = next(path.name for path in sockets if path.is_socket())
            native = spawn(args.client_bin, extra_env={"WAYLAND_DISPLAY": wayland, "GDK_BACKEND": "wayland"})
            native_row = wait_for(lambda: next((row for row in msg("get_windows") if row["name"] == "SharedShellNativeTask"), None), "real native xdg task")
            native_id = native_row["id"]
            # Spawn through JWM's active child environment: it owns the
            # XWayland DISPLAY, which may differ from this fixture's outer Xvfb.
            msg("spawn", {"cmd": ["xmessage", "-timeout", "120", "-title", "SharedShellXWaylandTask", "native and XWayland inventory"]})
            xrow = wait_for(lambda: next((row for row in msg("get_windows") if row["name"] == "SharedShellXWaylandTask"), None), "mixed XWayland task")
            xid = xrow["id"]
            msg("focus_window", {"id": xid})
            msg("maximize")
            wait_for(lambda: task(xid)["is_maximized"], "mixed XWayland maximize")
            def xgeometry_matches():
                actual = inner_xinfo("SharedShellXWaylandTask")
                expected = task(xid)
                result["mixed_x11_geometry"] = {"actual": actual, "wm": expected}
                # WM x/y describe the outer decoration box; configure sends the
                # client content origin after its explicit server-side border.
                border = expected["border_w"]
                target = [expected["x"] + border, expected["y"] + border, expected["w"], expected["h"]]
                return result["mixed_x11_geometry"] if all(abs(left - right) <= 2 for left, right in zip(actual["geometry"], target)) else None
            wait_for(xgeometry_matches, "actual XWayland geometry settles after WM maximize", timeout=15)
            msg("minimize")
            wait_for(lambda: task(xid)["is_minimized"], "mixed XWayland minimize")
            inner_xinfo("SharedShellXWaylandTask", refresh=True)
            assert task(xid)["is_minimized"], "an XWayland repaint remapped a minimized client"
            rect = wait_for(lambda: controls("bottom", "task", xid), "mixed task Dock allocation")
            pointer(rect[0] + rect[2] / 2, rect[1] + rect[3] / 2, True)
            wait_for(lambda: not task(xid)["is_minimized"] and task(xid)["is_focused"], "mixed XWayland Dock restore")
            msg("killclient")
            wait_for(lambda: all(row["id"] != xid for row in msg("get_windows")), "real XWayland close")
            record("mixed native/XWayland tasks: actual X geometry, maximize, hidden repaint, Dock restore and close")
            msg("focus_window", {"id": native_id})
            msg("minimize")
            wait_for(lambda: task(native_id)["is_minimized"], "native task minimized")
            rect = wait_for(lambda: controls("bottom", "task", native_id), "real task hit allocation")
            pointer(width / 2, height / 2)
            before_preview = capture("native-task-before-preview")
            pointer(rect[0] + rect[2] / 2, rect[1] + rect[3] / 2)
            time.sleep(0.6)
            preview = capture("native-task-preview")
            preview_pixels = changed_pixels(before_preview, preview, (100, max(50, height - 400), width - 100, height - 100))
            assert preview_pixels > 5000, ("native minimized preview did not render actual surface pixels", preview_pixels)
            result["preview_changed_pixels"] = preview_pixels
            run("xdotool", "click", "1")
            wait_for(lambda: not task(native_id)["is_minimized"] and task(native_id)["is_focused"], "native task restored through Dock")
            record("native xdg task preview and Dock restore use actual panel anchors")
            fullscreen = spawn(args.client_bin, "--fullscreen", extra_env={"WAYLAND_DISPLAY": wayland})
            wait_for(lambda: any(row["name"] == "SharedShellNativeFullscreen" and row["is_fullscreen"] for row in msg("get_windows")), "native xdg fullscreen request")
            wait_for(lambda: not state()["outputs"][0]["visible"], "native fullscreen hides shell input")
            capture("native-fullscreen")
            fullscreen.terminate()
            fullscreen.wait(timeout=5)
            wait_for(lambda: state()["outputs"][0]["visible"], "fullscreen exit restores shell")
            assert state()["host"]["pid"] == host_pid
            record("real xdg fullscreen hides and restores the same host")
            left = next(row for row in json.loads(run(args.shell_bin, "--inspect-controls"))
                        if row["output"] == 0 and row["edge"] == "left")
            # Apps, Files, Terminal are the first three generic controls in
            # the shared production Painter. The private PATH wrapper opens a
            # harmless real application through the native pointer action.
            terminal_hit = [row for row in left["controls"] if row["kind"] == "control"][2]["rect"]
            pointer(terminal_hit[0] + terminal_hit[2] / 2, terminal_hit[1] + terminal_hit[3] / 2, True)
            launched_pid = int(wait_for(lambda: (work / "launched-application.pid").read_text().strip()
                                       if (work / "launched-application.pid").is_file() else None,
                                       "native Terminal action launches the private application"))
            launched_application = process_identity(launched_pid)
            launch_error = work / "launched-application.pid.stderr"
            assert launched_application, ("shell-launched application exited", launch_error.read_text() if launch_error.exists() else "no stderr")
            launched_window = wait_for(lambda: next((row for row in msg("get_windows")
                                                     if row["name"] == "SharedNativeLaunchedTerminal"), None),
                                      "shell-launched application window")["id"]
            result["launched_application"] = {"pid": launched_pid, "window": launched_window}
            record("native panel Terminal control launches an independent real application")
            for edge in ("left", "right", "bottom"):
                set_config("desktop_shell." + edge, "hover")
            pointer(width / 2, height / 2)
            wait_for(lambda: msg("get_monitors")[0]["wx"] == 0 and msg("get_monitors")[0]["ww"] == width, "hover withdraws native exclusive zones")
            time.sleep(0.6)
            hidden = capture("native-hover-hidden")
            pointer(0, height / 2)
            time.sleep(0.3)
            shown = capture("native-hover-left")
            changed = changed_pixels(hidden, shown, (0, 40, 90, height - 40))
            assert changed > 5000, ("native edge sensor did not reveal panel pixels", changed)
            record("native hover sensors reveal panel pixels while exclusive zones remain empty")
            set_config("status_bar.name", args.second_bar)
            wait_for(lambda: any(row.get("pid") and Path(f'/proc/{row["pid"]}/exe').resolve() == second for row in state()["host"]["top_bars"]), "replacement top bar")
            assert state()["host"]["pid"] == host_pid
            record("top frontend replacement preserves native shared host PID")
            os.kill(host_pid, signal.SIGKILL)
            recovered = wait_for(lambda: (current := ready()) and current["host"]["pid"] != host_pid and current, "native host crash recovery", timeout=45)
            result["recovered_host_pid"] = recovered["host"]["pid"]
            assert native.poll() is None and any(row["id"] == native_id for row in msg("get_windows"))
            assert process_identity(launched_pid) == launched_application and task(launched_window)
            record("native host crash recovers exactly one shell and retains native and shell-launched applications")
            set_config("desktop_shell.enabled", False)
            wait_for(lambda: state()["host"]["pid"] is None, "disable reaps native host")
            wait_for(lambda: not any(row["name"].startswith("jwm-shell-") for row in msg("get_windows")), "disable removes native surfaces and sensors")
            wait_for(lambda: msg("get_monitors")[0]["wx"] == 0 and msg("get_monitors")[0]["ww"] == width, "disable releases native workspace")
            assert process_identity(launched_pid) == launched_application and task(launched_window)
            record("rollback removes native panels, trigger surfaces and reservations")
            result["status"] = "passed"
        except BaseException as error:
            result["status"] = "failed"
            result["error"] = repr(error)
            if isinstance(error, subprocess.CalledProcessError):
                result["command_output"] = str(error.output)[-4000:]
            raise
        finally:
            result["elapsed_seconds"] = round(time.monotonic() - started, 3)
            (evidence / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            if launched_application and process_identity(launched_application[0]) == launched_application:
                try:
                    os.kill(launched_application[0], signal.SIGTERM)
                except ProcessLookupError:
                    pass
            for child in reversed(children):
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                        child.wait(timeout=3)
                    except (ProcessLookupError, subprocess.TimeoutExpired):
                        try:
                            os.killpg(child.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        child.wait(timeout=3)
            log.close()
    print(json.dumps({"status": result["status"], "evidence": str(evidence)}))


if __name__ == "__main__":
    main()
