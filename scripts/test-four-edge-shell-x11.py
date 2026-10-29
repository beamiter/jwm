#!/usr/bin/env python3
"""Exercise the actual four-edge Tao shell in a private X11/JWM session.

Build JWM and tao_glow_bar first. JWM_TEST_BIN_DIR defaults to target/release;
JWM_TEST_BAR_BIN selects the bar executable and JWM_TEST_BACKEND selects x11rb
(default) or xcb. Requires Xvfb, xdpyinfo, xdotool, xprop, xmessage, libX11 and
dbus-run-session. No live desktop/socket, volume controls or launchers are used.
Set JWM_FOUR_EDGE_SCREENSHOT to save an optional compositor-captured PNG.
The fixture profile disables the independent hover-reveal companion shell.
"""

import ctypes
import ctypes.util
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
BIN = Path(os.environ.get("JWM_TEST_BIN_DIR", ROOT / "target/release")).resolve()
BAR = Path(os.environ.get(
    "JWM_TEST_BAR_BIN", ROOT / "bars/tao_glow_bar/target/release/tao_glow_bar")).resolve()
BACKEND = os.environ.get("JWM_TEST_BACKEND", "x11rb")
NAMES = {"top": "tao_glow_bar", **{
    edge: "tao_glow_bar-aux-" + edge for edge in ("left", "right", "bottom")}}
REFERENCE = {"top": (5, 5, 1270, 42), "left": (12, 72, 60, 656),
             "right": (1208, 72, 60, 656), "bottom": (128, 712, 1024, 76)}


def process_identity(pid):
    """Remember the process start time as well as the PID before cleanup."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        return pid, stat[stat.rfind(")") + 2:].split()[19]
    except FileNotFoundError:
        return None


def fullscreen(display_name, window, enabled):
    """Send the normal client request, rather than the fullscreen *layout*."""
    class Data(ctypes.Union):
        _fields_ = [("l", ctypes.c_long * 5)]

    class ClientMessage(ctypes.Structure):
        _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong),
                    ("send_event", ctypes.c_int), ("display", ctypes.c_void_p),
                    ("window", ctypes.c_ulong), ("message_type", ctypes.c_ulong),
                    ("format", ctypes.c_int), ("data", Data)]

    class Event(ctypes.Union):
        _fields_ = [("client", ClientMessage), ("padding", ctypes.c_long * 24)]

    library = ctypes.util.find_library("X11")
    if not library:
        raise RuntimeError("missing libX11 for the native fullscreen request")
    x11 = ctypes.CDLL(library)
    x11.XOpenDisplay.argtypes, x11.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x11.XInternAtom.restype = ctypes.c_ulong
    x11.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                              ctypes.c_long, ctypes.POINTER(Event)]
    x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    display = x11.XOpenDisplay(display_name.encode())
    assert display, "cannot open the private X11 display"
    try:
        event = Event()
        event.client.type, event.client.send_event = 33, 1  # ClientMessage
        event.client.display, event.client.window = display, window
        event.client.message_type = x11.XInternAtom(display, b"_NET_WM_STATE", 0)
        event.client.format = 32
        event.client.data.l[0] = int(enabled)  # ADD or REMOVE
        event.client.data.l[1] = x11.XInternAtom(display, b"_NET_WM_STATE_FULLSCREEN", 0)
        event.client.data.l[3] = 1  # application source
        assert x11.XSendEvent(display, x11.XDefaultRootWindow(display), 0,
                              (1 << 20) | (1 << 19), ctypes.byref(event))
        x11.XSync(display, 0)
    finally:
        x11.XCloseDisplay(display)


def main():
    assert BACKEND in ("x11rb", "xcb"), BACKEND
    for command in ("Xvfb", "xdpyinfo", "xdotool", "xprop", "xmessage", "dbus-run-session"):
        if not shutil.which(command):
            raise RuntimeError(f"missing required executable: {command}")
    for path in (BIN / "jwm", BIN / "jwm-tool", BAR):
        assert path.is_file() and os.access(path, os.X_OK), f"build the executable first: {path}"

    with tempfile.TemporaryDirectory(prefix="jwm-four-edge-") as directory:
        work = Path(directory)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", JWM_COMPOSITOR_API="glx",
                   GDK_BACKEND="x11", XBAR_CONFIG=str(work / "xbar.toml"))
        for variable, name in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                               ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                               ("XDG_RUNTIME_DIR", "runtime"), ("XDG_PICTURES_DIR", "pictures")):
            (work / name).mkdir(mode=0o700)
            env[variable] = str(work / name)
        # Keep HOME intact; every JWM-owned output directory is selected above.
        for variable in ("DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS",
                         "JWM_SOCKET", "JWM_BACKEND", "JWM_COMPOSITOR", "JWM_BAR_LAYOUT",
                         "JWM_BAR_POSITION", "JWM_BAR_THICKNESS", "JWM_BAR_PADDING"):
            env.pop(variable, None)
        bar_bin = work / "bin"
        bar_bin.mkdir()
        (bar_bin / "tao_glow_bar").symlink_to(BAR)
        env["PATH"] = str(bar_bin) + os.pathsep + env.get("PATH", "")
        shutil.copyfile(ROOT / "bars/tao_glow_bar/four-edge.toml", work / "xbar.toml")
        probe = socket.socket(socket.AF_UNIX)
        try:
            probe.bind(str(work / "socket-probe"))
        finally:
            probe.close()

        children, owners = [], set()
        log = (work / "session.log").open("w+")

        def run(*args):
            return subprocess.check_output(args, env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=10)

        def spawn(*args, **kwargs):
            child = subprocess.Popen(args, env=env, stdout=log, stderr=log,
                                     start_new_session=True, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate, label, timeout=15):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (subprocess.CalledProcessError, OSError, ValueError,
                        AssertionError, StopIteration) as error:
                    last = str(error)
                time.sleep(0.08)
            raise AssertionError(f"timed out: {label}; last={last}")

        def msg(command, args=None):
            argv = [str(BIN / "jwm-tool"), "msg", command, "--raw"]
            if args is not None:
                argv += ["--args", json.dumps(args)]
            response = json.loads(run(*argv))
            assert response.get("success", True), response
            return response.get("data", response)

        def panels():
            result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^tao_glow_bar"],
                                    env=env, capture_output=True, text=True, timeout=10)
            assert result.returncode in (0, 1), result.stderr
            visible = {}
            for token in result.stdout.split():
                title = run("xdotool", "getwindowname", token).strip()
                if title not in NAMES.values():
                    continue
                assert title not in visible, f"duplicate live panel: {title}"
                values = dict(re.findall(r"^(\w+)=(\S+)$",
                              run("xdotool", "getwindowgeometry", "--shell", token), re.M))
                geometry = tuple(int(values[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))
                pid = int(run("xprop", "-id", token, "_NET_WM_PID").split("=")[-1].strip())
                identity = process_identity(pid)
                assert identity, f"panel owner disappeared: {pid}"
                owners.add(identity)
                visible[title] = {"id": int(token), "rect": geometry, "owner": identity}
            return visible

        def four_panels(expected=REFERENCE):
            visible = panels()
            if set(visible) != set(NAMES.values()):
                return None
            assert len({item["owner"] for item in visible.values()}) == 1, visible
            for edge, rectangle in expected.items():
                if visible[NAMES[edge]]["rect"] != rectangle:
                    return None
            return visible

        def hidden_panels():
            visible = panels()
            if set(visible) != {NAMES["top"]}:
                return False
            x, y, width, height = visible[NAMES["top"]]["rect"]
            return x + width <= 0 or x >= 1280 or y + height <= 0 or y >= 800

        def workarea(expected, ewmh_required=True):
            monitor = msg("get_monitors")[0]
            actual = tuple(monitor[key] for key in ("wx", "wy", "ww", "wh"))
            if not ewmh_required:
                return actual == expected
            root = run("xprop", "-root", "_NET_WORKAREA").split("=")[-1]
            ewmh = tuple(map(int, re.findall(r"-?\d+", root)[:4]))
            return actual == expected and ewmh == expected

        def click(x, y, leave=True):
            # --sync can hang when the pointer is already at the destination.
            run("xdotool", "mousemove", str(x), str(y))
            # Native entry/hover queues the scene's redraw. Do not press in
            # the same XTest packet before that visible frame can be painted.
            time.sleep(0.2)
            run("xdotool", "mousedown", "1")
            time.sleep(0.06)
            run("xdotool", "mouseup", "1")
            time.sleep(0.12)  # Let GTK dispatch release before a synthetic leave.
            if leave:
                run("xdotool", "mousemove", "640", "400")

        def windows():
            return msg("get_windows")

        def current(window):
            return next(item for item in windows() if item["id"] == window)

        def owner(visible):
            return visible[NAMES["top"]]["owner"]

        def reaped(previous):
            return process_identity(previous[0]) != previous

        def capture(destination, move_pointer=True):
            if move_pointer:
                run("xdotool", "mousemove", "640", "400")
            time.sleep(0.8)
            queued = msg("take_screenshot_fullscreen")
            assert queued["status"] == "queued", queued
            source = Path(queued["path"])
            assert source.is_relative_to(work / "pictures"), source
            wait_for(lambda: source.is_file() and source.stat().st_size > 32, "compositor screenshot PNG")
            header = source.read_bytes()[:24]
            assert header[:8] == b"\x89PNG\r\n\x1a\n" and struct.unpack(">II", header[16:24]) == (1280, 800)
            destination = Path(destination).resolve()
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            print(f"Saved actual compositor screenshot: {destination}", flush=True)

        try:
            read_fd, write_fd = os.pipe()
            try:
                spawn("Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x800x24",
                      "-nolisten", "tcp", "-noreset", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                assert select.select([read_fd], [], [], 10)[0], "Xvfb readiness timeout"
                number = os.read(read_fd, 32).decode().strip()
                assert number.isdigit(), number
                env["DISPLAY"] = ":" + number
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            run("xdpyinfo")
            run(str(BIN / "jwm"), "--backend", BACKEND, "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            text = config.read_text()
            replacements = {"wallpaper": json.dumps(str(ROOT / "assets/wallpapers/08-cloud-sea.png")),
                            "wallpaper_effect": '"off"', "wallpaper_colors": "false",
                            "status_bar_height": "42", "status_bar_padding": "5", "blur_status_bar": "true"}
            for key, value in replacements.items():
                text, count = re.subn(r"(?m)^" + key + r" = .*$", f"{key} = {value}", text)
                assert count == 1, (key, count)
            text, count = re.subn(r"(?ms)(^\[status_bar\]\n).*?(?=^\[|\Z)",
                                 '[status_bar]\nname = "tao_glow_bar"\nshow_bar = true\nposition = "top"\nlayout = "four-edge"\n\n', text)
            assert count == 1, "missing status_bar table"
            config.write_text(text)
            run(str(BIN / "jwm"), "--backend", BACKEND, "--check-config")
            spawn("dbus-run-session", "--", str(BIN / "jwm"), "--backend", BACKEND)
            initial = wait_for(four_panels, "four native panels at reference rectangles")
            wait_for(lambda: workarea((84, 52, 1112, 648)), "IPC and EWMH four-edge work areas")
            print("PASS: one owner, four native rectangles; IPC/EWMH work areas", flush=True)

            click(103, 26)
            wait_for(lambda: msg("get_show_bar")["tag"] == 2, "top workspace 2 click")
            click(42, 488)
            wait_for(lambda: (state := msg("get_show_bar"))["tag"] == 3
                     and state["active_tags"] == 4, "left workspace 3 click")
            print("PASS: top and left workspace pointer actions", flush=True)

            targets = []
            for name in ("EdgeDockFirst", "EdgeDockSecond"):
                spawn("xmessage", "-name", name, "-title", name, "-geometry", "360x160+180+180", name)
                window = wait_for(lambda: next((item for item in windows() if item["name"] == name), None), name)
                targets.append(window["id"])
                msg("focus_window", {"id": window["id"]})
                wait_for(lambda: current(window["id"])["is_focused"], "focus " + name)
                msg("minimize")
                wait_for(lambda: current(window["id"])["is_minimized"], "minimize " + name)
            # Scene cards use two fixed, independently clickable title pills.
            restored_ids = set()
            for x, slot in ((603, 1), (449, 0)):
                candidates = sorted((current(window) for window in targets), key=lambda item: item["minimized_order"])
                target = candidates[slot]["id"]
                # Allow the minimized projection and its first native redraw
                # to catch up, including the software compositor's animation.
                time.sleep(1.1)
                click(x, 750, leave=False)
                wait_for(lambda: not current(target)["is_minimized"], "Dock restore acknowledgement")
                run("xdotool", "mousemove", "640", "400")
                wait_for(lambda: not current(target)["is_minimized"] and current(target)["is_focused"], "Dock restore/focus")
                restored_ids.add(target)
                msg("minimize")
                wait_for(lambda: current(target)["is_minimized"], "Dock reminimize")
            assert restored_ids == set(targets), "both distinct windows must be restored"
            print("PASS: both Dock title pills restore, focus and reminimize", flush=True)

            msg("togglebar")
            wait_for(hidden_panels, "three auxiliaries hidden and primary parked")
            msg("togglebar")
            wait_for(four_panels, "four panels restored after togglebar")
            assert owner(four_panels()) == owner(initial), "visibility toggle replaced the owner"
            print("PASS: togglebar hides/restores all four surfaces", flush=True)

            time.sleep(1.1)  # The re-shown panel must paint the new projection epoch.
            target = sorted((current(window) for window in targets), key=lambda item: item["minimized_order"])[0]
            click(449, 750, leave=False)
            wait_for(lambda: not current(target["id"])["is_minimized"], "restore fullscreen client")
            run("xdotool", "mousemove", "640", "400")
            native = int(run("xdotool", "search", "--onlyvisible", "--name", "^" + target["name"] + "$" ).split()[0])
            fullscreen(env["DISPLAY"], native, True)
            wait_for(lambda: current(target["id"])["is_fullscreen"], "true EWMH fullscreen client")
            wait_for(hidden_panels, "fullscreen hides every rail")
            fullscreen(env["DISPLAY"], native, False)
            wait_for(lambda: not current(target["id"])["is_fullscreen"], "leave fullscreen")
            wait_for(four_panels, "fullscreen exit restores four surfaces")
            msg("minimize")
            wait_for(lambda: current(target["id"])["is_minimized"], "minimize restored client")
            print("PASS: true client fullscreen hides/restores all rails", flush=True)

            previous = owner(four_panels())
            msg("set_config", {"key": "status_bar.layout", "value": "single"})
            single = wait_for(lambda: (visible if set(visible := panels()) == {NAMES["top"]}
                                      and owner(visible) != previous else None), "single-layout replacement")
            wait_for(lambda: reaped(previous), "four-panel owner reaped")
            # The legacy single-bar EWMH path predates this feature and may
            # expose the unreserved output. Its actual layout area must still
            # recover; the new four-edge EWMH contract remains strict below.
            wait_for(lambda: workarea((0, 52, 1280, 748), ewmh_required=False), "single-layout work area")
            previous = owner(single)
            msg("set_config", {"key": "status_bar.layout", "value": "four-edge"})
            replaced = wait_for(four_panels, "four-edge layout replacement")
            assert owner(replaced) != previous
            wait_for(lambda: reaped(previous), "single-panel owner reaped")
            print("PASS: single/four-edge mode changes replace and reap the whole owner", flush=True)

            previous = owner(replaced)
            msg("set_config_batch", {"values": {"appearance.status_bar_height": 48, "appearance.status_bar_padding": 8}})

            def resized():
                visible = panels()
                if set(visible) != set(NAMES.values()) or owner(visible) == previous:
                    return None
                assert len({item["owner"] for item in visible.values()}) == 1
                if visible[NAMES["top"]]["rect"] != (8, 8, 1264, 48):
                    return None
                for item in visible.values():
                    x, y, width, height = item["rect"]
                    assert 0 <= x < x + width <= 1280 and 0 <= y < y + height <= 800, item
                return visible

            resized_owner = owner(wait_for(resized, "size/padding replacement with four bounded surfaces"))
            wait_for(lambda: reaped(previous), "old size/padding owner reaped")
            msg("set_config_batch", {"values": {"appearance.status_bar_height": 42, "appearance.status_bar_padding": 5}})
            restored = wait_for(four_panels, "reference geometry restored")
            assert owner(restored) != resized_owner
            wait_for(lambda: reaped(resized_owner), "resized owner reaped")
            wait_for(lambda: workarea((84, 52, 1112, 648)), "restored EWMH work area")
            print("PASS: size/padding changes restart every surface, keep bounds and reap old owners", flush=True)

            if destination := os.environ.get("JWM_FOUR_EDGE_SCREENSHOT"):
                capture(destination)
            print(f"PASS: four-edge native integration ({BACKEND})", flush=True)
        except BaseException:
            try:
                print("Failure state:", json.dumps({"bar": msg("get_show_bar"),
                    "monitors": msg("get_monitors"),
                    "ewmh_workarea": run("xprop", "-root", "_NET_WORKAREA"), "windows": [
                    {key: window[key] for key in ("id", "name", "is_focused", "is_minimized", "minimized_order")}
                    for window in windows()]}), flush=True)
                if destination := os.environ.get("JWM_FOUR_EDGE_SCREENSHOT"):
                    capture(Path(destination).with_suffix(".failed.png"), move_pointer=False)
            except Exception as diagnostic_error:
                print("Could not collect failure screenshot:", diagnostic_error, flush=True)
            log.flush()
            log.seek(0)
            print(log.read()[-24000:])
            raise
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=5)
            # Only process identities observed on this private display qualify.
            for pid, start in owners:
                if process_identity(pid) == (pid, start):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            log.close()


if __name__ == "__main__":
    # The historical fixed four-surface ownership is now a compatibility
    # configuration for the session-owned shared shell. Keep helpers above
    # importable by the common fixture and exercise the migrated lifecycle.
    import sys
    command = [sys.executable, str(ROOT / "scripts/test-shared-sidebar-shell-x11.py"),
               "--bar", "tao_glow_bar", "--bar-bin", str(BAR), "--backend", BACKEND,
               "--bin-dir", str(BIN), "--shell-bin", os.environ.get("JWM_TEST_SHELL_BIN", str(ROOT / "crates/jwm_shell/target/debug/jwm-shell")),
               "--legacy-four-edge", "--deep"]
    second = ROOT / "bars/tao_pixels_bar/target/debug/tao_pixels_bar"
    if second.is_file():
        command.extend(["--second-bar-bin", str(second)])
    subprocess.run(command, check=True)
    if screenshot := os.environ.get("JWM_FOUR_EDGE_SCREENSHOT"):
        source = ROOT / "artifacts/shared-shell" / f"tao_glow_bar-{BACKEND}-1280x800-light-legacy-four-edge/desktop.png"
        shutil.copyfile(source, screenshot)
