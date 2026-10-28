#!/usr/bin/env python3
"""Exercise real edge sensors, independent reveal, hide cancellation and pinning.

Build bars/tao_glow_bar first. Requires Xvfb, xdotool and a D-Bus session.
No existing desktop or user configuration is touched.
"""
import os
import json
from pathlib import Path
import select
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BAR = Path(os.environ.get("JWM_EDGE_BAR_BIN", ROOT / "bars/tao_glow_bar/target/debug/tao_glow_bar"))


def main():
    with tempfile.TemporaryDirectory(prefix="jwm-edge-test-") as directory:
        work = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"), XDG_CACHE_HOME=str(work / "cache"),
                   XDG_RUNTIME_DIR=str(work / "runtime"), LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11")
        (work / "runtime").mkdir(mode=0o700)
        config = work / "xbar.toml"
        env["XBAR_CONFIG"] = str(config)
        env.pop("EGL_PLATFORM", None)
        env["PATH"] = str(BAR.parent) + os.pathsep + env["PATH"]
        children = []
        log = (work / "session.log").open("w+")

        def spawn(*args, **kwargs):
            child = subprocess.Popen(args, env=env, stdout=log, stderr=log, **kwargs)
            children.append(child)
            return child

        def run(*args):
            return subprocess.check_output(args, env=env, text=True, stderr=subprocess.STDOUT, timeout=10)

        def wait_for(predicate):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, subprocess.CalledProcessError):
                    pass
                time.sleep(0.03)
            raise AssertionError("edge shell readiness/interaction timeout")

        def find(edge):
            return run("xdotool", "search", "--name", "^jwm-edge-" + edge + "$").strip().splitlines()[0]

        def geometry(window):
            values = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", window).splitlines())
            return {key: int(values[key]) for key in ["X", "Y", "WIDTH", "HEIGHT"]}

        def move(x, y):
            run("xdotool", "mousemove", "--sync", str(x), str(y))

        try:
            read_fd, write_fd = os.pipe()
            spawn("Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x800x24", "-nolisten", "tcp", pass_fds=(write_fd,))
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
            config.write_text('[edge_shell]\nreveal_delay_ms = 40\nhide_delay_ms = 300\n')
            bar = spawn(str(BAR))
            left, right, bottom = [wait_for(lambda edge=edge: find(edge)) for edge in ["left", "right", "bottom"]]
            assert geometry(left)["WIDTH"] == geometry(right)["WIDTH"] == 2
            assert geometry(bottom)["HEIGHT"] == 2
            move(0, 400)
            wait_for(lambda: geometry(left)["WIDTH"] == 84)
            assert geometry(right)["WIDTH"] == 2 and geometry(bottom)["HEIGHT"] == 2
            move(40, 400)  # Passage into the floating capsule must not close it.
            time.sleep(0.4)
            assert geometry(left)["WIDTH"] == 84
            move(640, 400)
            time.sleep(0.10)
            assert geometry(left)["WIDTH"] == 84
            move(40, 400)  # Re-entry cancels the pending hide.
            time.sleep(0.35)
            assert geometry(left)["WIDTH"] == 84
            move(640, 400)
            wait_for(lambda: geometry(left)["WIDTH"] == 2)
            move(1279, 400)
            wait_for(lambda: geometry(right)["WIDTH"] == 84)
            assert geometry(left)["WIDTH"] == 2
            move(640, 799)
            wait_for(lambda: geometry(bottom)["HEIGHT"] == 88)
            wait_for(lambda: geometry(right)["WIDTH"] == 2)
            move(640, 400)
            wait_for(lambda: geometry(bottom)["HEIGHT"] == 2)
            bar.terminate()
            bar.wait(timeout=10)
            config.write_text('[edge_shell]\nleft = "always"\nright = "disabled"\nbottom = "always"\n')
            bar = spawn(str(BAR))
            left = wait_for(lambda: find("left"))
            bottom = wait_for(lambda: find("bottom"))
            wait_for(lambda: geometry(left)["WIDTH"] == 84 and geometry(bottom)["HEIGHT"] == 88)
            time.sleep(0.6)
            assert geometry(left)["WIDTH"] == 84 and geometry(bottom)["HEIGHT"] == 88
            try:
                find("right")
                raise AssertionError("disabled right panel was mapped")
            except subprocess.CalledProcessError:
                pass
            capture = os.environ.get("JWM_EDGE_SCREENSHOT")
            if capture:
                run("import", "-window", "root", capture)
            assert bar.poll() is None
            bar.terminate()
            bar.wait(timeout=10)
            jwm = ROOT / "target/debug/jwm"
            tool = ROOT / "target/debug/jwm-tool"
            run(str(jwm), "--backend", "x11rb", "--gen-config")
            jwm_config = work / "config/jwm/config_x11.toml"
            # Keep native rendering deterministic and use a real bundled
            # wallpaper for the screenshot rather than an invented UI mock.
            import re
            wallpaper = ROOT / "assets/wallpapers/08-cloud-sea.png"
            content = jwm_config.read_text()
            content = re.sub(r'^wallpaper = .*$', 'wallpaper = ' + json.dumps(str(wallpaper)), content, flags=re.M)
            content = re.sub(r'^wallpaper_effect = .*$', 'wallpaper_effect = "off"', content, flags=re.M)
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
            wait_for(lambda: geometry(left)["WIDTH"] == 84)
            before = wait_for(lambda: next((w for w in msg("get_workspaces") if w["focused"]), None))["tag_index"]
            rect = geometry(left)
            move(rect["X"] + 42, rect["Y"] + 14 + 5 * 52 + 16 + 52 + 22)
            run("xdotool", "click", "1")
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
            move(rect["X"] + 18 + 172 + 60 + 71, rect["Y"] + 38)
            run("xdotool", "click", "1")
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_focused"] for w in msg("get_windows")))
            msg("minimize")
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_minimized"] for w in msg("get_windows")))
            time.sleep(1.2)
            run("xdotool", "click", "1")
            wait_for(lambda: any(w["id"] == tasks[0]["id"] and w["is_focused"] and not w["is_minimized"] for w in msg("get_windows")))
            def mapped(window):
                return "Map State: IsViewable" in run("xwininfo", "-id", window)
            msg("toggle_bar")
            wait_for(lambda: not mapped(left) and not mapped(bottom))
            msg("toggle_bar")
            wait_for(lambda: mapped(left) and mapped(bottom))
            if capture:
                for task in tasks:
                    msg("focus_window", {"id": task["id"]})
                    msg("minimize")
                move(640, 400)
                time.sleep(1.2)
                run("import", "-window", "root", capture)
            print("PASS: independent hover, delayed hide/cancellation, pinned/disabled edges, workspace switching, task focus/restore and bar visibility through JWM IPC")
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read())
            raise
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            log.close()


if __name__ == "__main__":
    main()
