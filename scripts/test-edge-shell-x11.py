#!/usr/bin/env python3
"""Exercise real edge sensors, independent reveal, hide cancellation and pinning.

Build bars/tao_glow_bar first. Requires Xvfb, xdotool and libX11. A D-Bus session enables desktop providers.
No existing desktop or user configuration is touched.
JWM_TEST_BIN_DIR selects JWM/tool binaries (default target/debug).
"""
import os
import json
import ctypes
from contextlib import contextmanager
from pathlib import Path
import select
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BAR = Path(os.environ.get("JWM_EDGE_BAR_BIN", ROOT / "bars/tao_glow_bar/target/debug/tao_glow_bar"))
BIN = Path(os.environ.get("JWM_TEST_BIN_DIR", ROOT / "target/debug"))


def main():
    with tempfile.TemporaryDirectory(prefix="jwm-edge-test-") as directory:
        work = Path(directory)
        width,height=map(int,os.environ.get("JWM_EDGE_TEST_SIZE","1280x800").split("x"))
        assert width>=1000 and height>=700, "interaction test needs at least 1000x700"
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"), XDG_CACHE_HOME=str(work / "cache"),
                   XDG_RUNTIME_DIR=str(work / "runtime"), LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11", NO_AT_BRIDGE="1")
        (work / "runtime").mkdir(mode=0o700)
        config = work / "xbar.toml"
        env["XBAR_CONFIG"] = str(config)
        env.pop("EGL_PLATFORM", None)
        if os.environ.get("JWM_EDGE_TEST_WAYLAND_TRIGGER") == "1":
            # Exercise the mapped transparent trigger representation while
            # keeping the harness itself on its isolated X11 server.
            env["WAYLAND_DISPLAY"]="wayland-edge-trigger-test"
        env["PATH"] = str(BAR.parent) + os.pathsep + env["PATH"]
        children = []
        log = (work / "session.log").open("w+")

        event_files = []
        event_displays = []

        def spawn(*args, **kwargs):
            kwargs.setdefault("stdout", log)
            kwargs.setdefault("stderr", log)
            child = subprocess.Popen(args, env=env, **kwargs)
            children.append(child)
            return child

        def run(*args):
            return subprocess.check_output(args, env=env, text=True, stderr=subprocess.STDOUT, timeout=10)

        def wait_for(predicate, label="edge shell readiness/interaction timeout", timeout=10):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, subprocess.CalledProcessError):
                    pass
                time.sleep(0.03)
            raise AssertionError(label)

        def find(edge):
            return run("xdotool", "search", "--name", "^jwm-edge-" + edge + "$").strip().splitlines()[0]

        def geometry(window):
            values = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", window).splitlines())
            return {key: int(values[key]) for key in ["X", "Y", "WIDTH", "HEIGHT"]}

        def move(x, y):
            # --sync waits for a motion event even if already at this point;
            # repeated task restore clicks intentionally reuse their position.
            run("xdotool", "mousemove", str(x), str(y))

        def struts(window):
            raw = run("xprop","-id",window,"_NET_WM_STRUT_PARTIAL").split("=",1)[1]
            return [int(value.strip()) for value in raw.split(",")]

        def click_control(window, kind, **identity):
            def ready_control():
                raw=run("xprop","-id",window,"-notype","_JWM_EDGE_CONTROLS")
                if "=" not in raw:
                    return None
                controls=json.loads(json.loads(raw.split("=",1)[1].strip()))
                return next((c for c in controls if c["kind"]==kind and all(c.get(k)==v for k,v in identity.items())),None)
            hit=wait_for(ready_control)
            x,y,w,h=hit["rect"]
            origin=geometry(window)
            move(origin["X"]+int(x+w/2),origin["Y"]+int(y+h/2))
            run("xdotool","click","1")

        @contextmanager
        def pointer_grab(owner_events):
            # A compositor popup or application menu can temporarily grab the
            # pointer without moving it. X11 reports Grab/Ungrab crossing events;
            # they are not a physical exit from the edge panel.
            xlib = ctypes.CDLL("libX11.so.6")
            xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
            xlib.XOpenDisplay.restype = ctypes.c_void_p
            xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
            xlib.XDefaultRootWindow.restype = ctypes.c_ulong
            xlib.XGrabPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                                         ctypes.c_uint, ctypes.c_int, ctypes.c_int,
                                         ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
            xlib.XUngrabPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
            xlib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
            xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
            display = xlib.XOpenDisplay(env["DISPLAY"].encode())
            assert display, "cannot open isolated X display for grab regression"
            try:
                root = xlib.XDefaultRootWindow(display)
                assert xlib.XGrabPointer(display, root, int(owner_events), 0, 1, 1, 0, 0, 0) == 0, "external pointer grab failed"
                xlib.XSync(display, 0)
                yield
            finally:
                xlib.XUngrabPointer(display, 0)
                xlib.XSync(display, 0)
                xlib.XCloseDisplay(display)

        def observe_structure(window, label):
            # Observe on our own synchronized X connection. An external xev
            # process can be scheduled after the baseline sample under parallel
            # build load, falsely attributing an intentional earlier hide to a
            # later handoff. XSync orders every sample against server events.
            import ctypes.util
            xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
            xlib.XOpenDisplay.argtypes, xlib.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
            xlib.XSelectInput.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_long]
            xlib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
            xlib.XPending.argtypes = [ctypes.c_void_p]
            xlib.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
            xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
            display = xlib.XOpenDisplay(env["DISPLAY"].encode())
            assert display, "structure observer could not open private display"
            event_displays.append((xlib, display))
            xlib.XSelectInput(display, int(window, 0), 1 << 17)  # StructureNotifyMask
            xlib.XSync(display, 0)
            trace = work / f"structure-{label}.log"
            stream = trace.open("w", buffering=1)
            event_files.append(stream)
            count = 0
            def unmaps():
                nonlocal count
                xlib.XSync(display, 0)
                while xlib.XPending(display):
                    event = (ctypes.c_long * 24)()
                    xlib.XNextEvent(display, ctypes.byref(event))
                    kind = ctypes.cast(ctypes.byref(event), ctypes.POINTER(ctypes.c_int))[0]
                    if kind == 18:
                        count += 1
                        stream.write("UnmapNotify\n")
                    elif kind == 19:
                        stream.write("MapNotify\n")
                return count
            return unmaps

        def stay_visible(window, unmaps, baseline, label, duration):
            deadline = time.monotonic() + duration
            while time.monotonic() < deadline:
                assert mapped(window), f"{label}: panel closed while pointer remained inside"
                assert unmaps() == baseline, f"{label}: transient unmap/remap flicker"
                time.sleep(0.01)

        def cross_point(edge, inset, panel):
            if edge == "left":
                return inset, panel["Y"] + panel["HEIGHT"] // 2
            if edge == "right":
                return width - 1 - inset, panel["Y"] + panel["HEIGHT"] // 2
            return panel["X"] + panel["WIDTH"] // 2, height - 1 - inset

        def exercise_handoffs(panels, sensors):
            traces = {edge: observe_structure(window, edge) for edge, window in panels.items()}
            # Signal each live configuration update by changing the mode too;
            # a sleep alone would not establish which hide timeout was tested.
            for hide_ms in [0, 40]:
                move(width//2, height//2)
                config.write_text('[edge_shell]\nleft="always"\nright="disabled"\nbottom="disabled"\n')
                wait_for(lambda: mapped(panels["left"]) and not mapped(panels["right"]) and not mapped(panels["bottom"]))
                config.write_text(f'[edge_shell]\nreveal_delay_ms=0\nhide_delay_ms={hide_ms}\n')
                wait_for(lambda: all(not mapped(window) for window in panels.values()) and all(mapped(sensor) for sensor in sensors))
                hold = hide_ms/1000 + 0.08
                for edge, window in panels.items():
                    for attempt in range(2):
                        # Enter the physical sensor before using the actual
                        # expanded geometry for the slow crossing path.
                        approach = (0, height//2) if edge == "left" else (width-1,height//2) if edge == "right" else (width//2,height-1)
                        move(*approach)
                        wait_for(lambda: mapped(window))
                        panel = geometry(window)
                        time.sleep(0.04)  # Let the observer consume the preceding intentional hide/map.
                        baseline = traces[edge]()
                        for inset in [1, 2, 3, 6, 10, 14, 22, 36]:
                            move(*cross_point(edge, inset, panel))
                            stay_visible(window, traces[edge], baseline, f"{edge}/{hide_ms}ms/entry-{attempt}/inset-{inset}", hold)
                        for owner_events in [False, True]:
                            with pointer_grab(owner_events):
                                stay_visible(window, traces[edge], baseline, f"{edge}/{hide_ms}ms/external-grab-{owner_events}", hold)
                            stay_visible(window, traces[edge], baseline, f"{edge}/{hide_ms}ms/ungrab-{owner_events}", hold)
                        for owner_events in [False, True]:
                            # Conversely, physical motion while another client
                            # owns the grab must be reconciled when it releases
                            # the pointer. Ignoring every Ungrab would leave a
                            # stale inside flag and make the panel stay open.
                            with pointer_grab(owner_events):
                                move(width//2, height//2)
                                time.sleep(hold)
                            wait_for(lambda: not mapped(window), f"{edge}/{hide_ms}ms/grab-move-out-{owner_events}: panel did not hide after Ungrab")
                            move(*approach)
                            wait_for(lambda: mapped(window))
                            panel = geometry(window)
                            move(*cross_point(edge,36,panel))
                            time.sleep(0.04)
                            baseline = traces[edge]()
                            stay_visible(window, traces[edge], baseline, f"{edge}/{hide_ms}ms/reenter-after-grab-{owner_events}", hold)
                        # Implicit button grabs must hold the panel open while
                        # dragging out; release outside must then permit hiding.
                        controls_raw = run("xprop", "-id", window, "-notype", "_JWM_EDGE_CONTROLS")
                        controls = json.loads(json.loads(controls_raw.split("=",1)[1].strip()))
                        assert controls, f"{edge}: no real controls were rendered"
                        x,y,w,h = controls[0]["rect"]
                        move(panel["X"]+int(x+w/2), panel["Y"]+int(y+h/2))
                        run("xdotool", "mousedown", "1")
                        try:
                            move(width//2, height//2)
                            stay_visible(window, traces[edge], baseline, f"{edge}/{hide_ms}ms/button-grab", hold)
                        finally:
                            run("xdotool", "mouseup", "1")
                        wait_for(lambda: not mapped(window), f"{edge}/{hide_ms}ms/button-grab: panel did not hide after release outside")
                        assert all(not mapped(other) for other in panels.values())
                    print(f"PASS: {edge} sensor/passage/panel handoff, external and button grabs, {hide_ms}ms hide delay")

        try:
            read_fd, write_fd = os.pipe()
            spawn("Xvfb", "-displayfd", str(write_fd), "-screen", "0", f"{width}x{height}x24", "-nolisten", "tcp", pass_fds=(write_fd,))
            os.close(write_fd)
            # Xvfb can write the display number and newline separately. Keep
            # the read end alive until both arrive, or its final write can
            # receive EPIPE and terminate the freshly started X server.
            display_bytes = bytearray()
            deadline = time.monotonic() + 10
            while b"\n" not in display_bytes:
                assert select.select([read_fd], [], [], max(0, deadline-time.monotonic()))[0], "Xvfb readiness timeout"
                part = os.read(read_fd, 32)
                assert part, "Xvfb exited before reporting its display"
                display_bytes.extend(part)
            display = display_bytes.decode().strip()
            assert display.isdigit(), display
            env["DISPLAY"] = ":" + display
            os.close(read_fd)
            def mapped(window):
                return "Map State: IsViewable" in run("xwininfo", "-id", window)

            config.write_text('[edge_shell]\nreveal_delay_ms = 40\nhide_delay_ms = 300\n')
            bar = spawn(str(BAR))
            # GTK may synchronously wait for a freshly activated desktop portal
            # during application registration. Interaction deadlines stay short.
            left, right, bottom = [wait_for(lambda edge=edge: find(edge), timeout=45) for edge in ["left", "right", "bottom"]]
            triggers = [find("trigger-" + edge) for edge in ["left", "right", "bottom"]]
            if os.environ.get("JWM_EDGE_TEST_WAYLAND_TRIGGER") == "1":
                assert "Depth: 32" in run("xwininfo","-id",triggers[0])
            assert all(not mapped(window) for window in [left,right,bottom])
            assert geometry(triggers[0])["HEIGHT"] >= height-10
            assert geometry(triggers[2])["WIDTH"] >= width-10
            # Approach outside the old center-only sensor, then cross into
            # the floating capsule. Each physical edge owns its own state.
            move(0, 30)
            wait_for(lambda: mapped(left))
            assert not mapped(right) and not mapped(bottom)
            move(40,height//2)
            time.sleep(0.4)
            assert mapped(left)
            move(width//2, height//2)
            time.sleep(0.10)
            assert mapped(left)
            move(40,height//2)
            time.sleep(0.35)
            assert mapped(left)
            move(width//2, height//2)
            wait_for(lambda: not mapped(left))
            move(width-1,height-35)
            wait_for(lambda: mapped(right))
            assert not mapped(left)
            move(30,height-1)
            wait_for(lambda: mapped(bottom))
            wait_for(lambda: not mapped(right))
            move(width//2,height-50)
            time.sleep(0.4)
            assert mapped(bottom)
            move(width//2, height//2)
            wait_for(lambda: not mapped(bottom))
            exercise_handoffs({"left":left,"right":right,"bottom":bottom}, triggers)
            # Later application windows must not bury the input-only trigger.
            blocker = spawn("xmessage", "-geometry", f"{width}x{height}+0+0", "edge stacking regression")
            time.sleep(0.3)
            move(0, 30)
            wait_for(lambda: mapped(left))
            move(width//2,height//2)
            wait_for(lambda: not mapped(left))
            blocker.terminate()
            blocker.wait(timeout=5)
            # Same process: pinned/disabled settings must apply without a restart.
            config.write_text('[edge_shell]\nleft = "always"\nright = "disabled"\nbottom = "always"\n')
            wait_for(lambda: mapped(left) and mapped(bottom) and not mapped(right))
            time.sleep(0.6)
            assert mapped(left) and mapped(bottom)
            assert all(not mapped(trigger) for trigger in triggers)
            assert struts(left)[0] == 90 and struts(bottom)[3] == 76
            config.write_text('[edge_shell]\nleft = "hover"\nright = "always"\nbottom = "disabled"\n')
            wait_for(lambda: not mapped(left) and mapped(right) and not mapped(bottom))
            assert struts(left)[0] == 0 and struts(bottom)[3] == 0
            assert struts(right)[1] == 90
            # A malformed edit must retain the last valid live modes.
            config.write_text('[edge_shell]\nleft = "invalid"\n')
            time.sleep(1.2)
            assert mapped(right) and not mapped(bottom)
            assert bar.poll() is None
            capture = os.environ.get("JWM_EDGE_SCREENSHOT")
            bar.terminate()
            bar.wait(timeout=10)
            jwm = BIN / "jwm"
            tool = BIN / "jwm-tool"
            run(str(jwm), "--backend", "x11rb", "--gen-config")
            jwm_config = work / "config/jwm/config_x11.toml"
            # Keep native rendering deterministic and use a real bundled
            # wallpaper for the screenshot rather than an invented UI mock.
            import re
            wallpaper = ROOT / "assets/wallpapers/08-cloud-sea.png"
            content = jwm_config.read_text()
            content = re.sub(r'^wallpaper = .*$', 'wallpaper = ' + json.dumps(str(wallpaper)), content, flags=re.M)
            content = re.sub(r'^wallpaper_effect = .*$', 'wallpaper_effect = "off"', content, flags=re.M)
            if os.environ.get("JWM_EDGE_TEST_REFERENCE") == "1":
                content = re.sub(r'^status_bar_height = .*$', 'status_bar_height = 52', content, flags=re.M)
                content = re.sub(r'^status_bar_padding = .*$', 'status_bar_padding = 12', content, flags=re.M)
            content = re.sub(r'^name = ".*_bar"$', 'name = "tao_glow_bar"', content, flags=re.M)
            # This fixture retains the explicit independent legacy rollback path.
            # Shared-host lifecycle and all frontend switches have their own
            # parameterized test-shared-sidebar-shell-x11.py fixture.
            content += '\n[desktop_shell]\nenabled = false\n'
            env["XBAR_LEGACY_PRIVATE_SHELL"] = "1"
            jwm_config.write_text(content)
            config.write_text('[edge_shell]\nleft = "always"\nright = "always"\nbottom = "always"\n[glass]\nwallpaper = ' + json.dumps(str(wallpaper)) + '\n')
            spawn(str(jwm), "--backend", "x11rb")

            def msg(command, args=None):
                cmd = [str(tool), "msg", command, "--raw"]
                if args is not None:
                    cmd += ["--args", json.dumps(args)]
                response = json.loads(run(*cmd))
                assert response.get("success", True), response
                return response.get("data", response)

            left = wait_for(lambda: find("left"))
            bottom = wait_for(lambda: find("bottom"))
            wait_for(lambda: mapped(left))
            before = wait_for(lambda: next((w for w in msg("get_workspaces") if w["focused"]), None))["tag_index"]
            click_control(left,"workspace",index=1)
            wait_for(lambda: next(w["tag_index"] for w in msg("get_workspaces") if w["focused"]) != before)
            spawn("xmessage", "-name", "EdgeDockFirst", "-geometry", "320x180+200+180", "JWM four-edge dock")
            spawn("xmessage", "-name", "EdgeDockSecond", "-geometry", "320x180+600+180", "Live window focus")
            tasks = wait_for(lambda: [w for w in msg("get_windows") if "EdgeDock" in str(w)] if len([w for w in msg("get_windows") if "EdgeDock" in str(w)]) == 2 else None)
            msg("focus_window", {"id": tasks[1]["id"]})
            time.sleep(1.2)  # Existing aligned provider tick publishes the model.
            tasks = [w for w in msg("get_windows") if "EdgeDock" in str(w)]
            print("Task inventory:", [(w["id"], w["name"], w["is_focused"]) for w in tasks])
            rect = geometry(bottom)
            if capture:
                run("import", "-window", "root", capture)
            click_control(bottom,"task",id=tasks[0]["id"])
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_focused"] for w in msg("get_windows")))
            msg("minimize")
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_minimized"] for w in msg("get_windows")))
            time.sleep(1.2)
            click_control(bottom,"task",id=tasks[0]["id"])
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_focused"] and not w["is_minimized"] for w in msg("get_windows")))
            msg("toggle_bar")
            wait_for(lambda: not mapped(left) and not mapped(bottom))
            msg("toggle_bar")
            wait_for(lambda: mapped(left) and mapped(bottom))
            if capture:
                for task in tasks:
                    msg("focus_window", {"id": task["id"]})
                    msg("minimize")
                move(width//2, height//2)
                time.sleep(1.2)
                run("import", "-window", "root", capture)
                if os.environ.get("JWM_EDGE_CAPTURE_WINDOWS") == "1":
                    for name,window in [("left",left),("right",find("right")),("bottom",bottom)]:
                        run("import","-window",window,str(Path(capture).with_suffix("."+name+".png")))
            print("PASS: independent hover, delayed hide/cancellation, pinned/disabled edges, workspace switching, task focus/restore and bar visibility through JWM IPC")
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read())
            for trace in work.glob("structure-*.log"):
                print(trace.name, trace.read_text()[-2000:])
            for command in [("xwininfo","-root","-tree"),("xdotool","getmouselocation","--shell")]:
                try:
                    print(run(*command))
                except (OSError,subprocess.SubprocessError):
                    pass
            raise
        finally:
            for xlib, display in event_displays:
                xlib.XCloseDisplay(display)
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            log.close()
            for stream in event_files:
                stream.close()


if __name__ == "__main__":
    # Keep the original standalone/explicit rollback hover handoff, grab and
    # flicker regression, then exercise migration to the shared process.
    main()
    # Legacy per-edge settings now migrate to the session-owned shared host;
    # this entry point continues to exercise file hot edits and rollback.
    import sys
    command = [sys.executable, str(ROOT / "scripts/test-shared-sidebar-shell-x11.py"),
               "--bar", "tao_glow_bar", "--bar-bin", str(BAR),
               "--backend", os.environ.get("JWM_TEST_BACKEND", "x11rb"),
               "--bin-dir", str(BIN), "--shell-bin", os.environ.get("JWM_TEST_SHELL_BIN", str(ROOT / "crates/jwm_shell/target/debug/jwm-shell")),
               "--legacy-edge-shell", "--deep"]
    second = ROOT / "bars/tao_pixels_bar/target/debug/tao_pixels_bar"
    if second.is_file():
        command.extend(["--second-bar-bin", str(second)])
    subprocess.run(command, check=True)
