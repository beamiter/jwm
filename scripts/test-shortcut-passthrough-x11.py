#!/usr/bin/env python3
"""Test actual keyboard delivery in an isolated Xvfb JWM session.

Build jwm and jwm-tool first. Set JWM_TEST_BIN_DIR and optionally
JWM_TEST_BACKEND=xcb (default x11rb). Requires Xvfb, xdpyinfo, xdotool,
xev and stdbuf. Does not connect to the user's display or IPC socket.
"""
import json
import os
from pathlib import Path
import select
import shutil
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("JWM_TEST_BIN_DIR", ROOT / "target/debug"))
BACKEND = os.environ.get("JWM_TEST_BACKEND", "x11rb")


def main():
    for executable in ["Xvfb", "xdpyinfo", "xdotool", "xmodmap", "xev", "stdbuf"]:
        if not shutil.which(executable):
            raise RuntimeError(f"missing executable: {executable}")
    with tempfile.TemporaryDirectory(prefix="jwm-shortcuts-x11-") as directory:
        work = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"),
                   XDG_RUNTIME_DIR=str(work / "runtime"), XDG_STATE_HOME=str(work / "state"),
                   XDG_CACHE_HOME=str(work / "cache"), JWM_COMPOSITOR="0", JWM_DEBUG_KEYS="1")
        (work / "runtime").mkdir(mode=0o700)
        probe = socket.socket(socket.AF_UNIX)
        try:
            probe.bind(str(work / "probe"))
        finally:
            probe.close()
        children, logs = [], []
        session_log = (work / "session.log").open("w+")
        logs.append(session_log)

        def run(*args):
            return subprocess.check_output(args, env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=10)

        def spawn(*args, output=session_log, **kwargs):
            child = subprocess.Popen(args, env=env, stdout=output, stderr=output, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    result = predicate()
                    if result:
                        return result
                except (subprocess.CalledProcessError, OSError, ValueError, StopIteration):
                    pass
                time.sleep(0.05)
            raise AssertionError("timed out waiting for X11/JWM state")

        def msg(name, args=None):
            command = [str(BIN / "jwm-tool"), "msg", name, "--raw"]
            if args is not None:
                command += ["--args", json.dumps(args)]
            result = json.loads(run(*command))
            assert result.get("success", True), result
            return result.get("data", result)

        def focused(window):
            return any(w["id"] == window and w["is_focused"] for w in msg("get_windows"))

        def focus(window):
            msg("focus_window", {"id": window})
            wait_for(lambda: focused(window))

        def key(chord):
            # Xvfb/Xlib may publish MappingNotify when XTest opens a client.
            # Leave the WM time to reinstall grabs before the chord's key.
            run("xdotool", "key", "--clearmodifiers", "--delay", "120", chord)
            time.sleep(0.15)

        def received(path, sym):
            return path.read_text().count(f", {sym}),")

        try:
            read_fd, write_fd = os.pipe()
            try:
                spawn("Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x800x24",
                      "-nolisten", "tcp", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                assert select.select([read_fd], [], [], 10)[0], "Xvfb readiness timeout"
                display = os.read(read_fd, 32).decode().strip()
                assert display.isdigit(), display
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            env["DISPLAY"] = ":" + display
            run("xdpyinfo")
            run(str(BIN / "jwm"), "--backend", BACKEND, "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            text = config.read_text().replace("show_bar = true", "show_bar = false")
            text = text.replace('name = "tao_glow_bar"', 'name = "/bin/true"')
            text = text.replace('wallpaper_effect = "daily"', 'wallpaper_effect = "none"')
            text = text.replace('leader_key = ""', 'leader_key = "space"')
            text = text.replace('leader_modifier = []', 'leader_modifier = ["Mod1"]')
            config.write_text(text)
            spawn(str(BIN / "jwm"), "--backend", BACKEND)
            wait_for(lambda: msg("get_monitors"))
            windows = []
            for name in ["RemotePassthrough", "LocalPassthrough"]:
                path = work / f"{name}.log"
                output = path.open("w+")
                logs.append(output)
                child = spawn("stdbuf", "-oL", "xev", "-event", "keyboard", "-name", name, output=output)
                window = wait_for(lambda: next(w for w in msg("get_windows") if w["name"] == name))
                windows.append((window["id"], path, child))
            remote, remote_log, remote_process = windows[0]
            local, local_log, _ = windows[1]
            # Xlib clients can initialize Xvfb's XKB map lazily. Install grabs
            # against that final map before asserting baseline key delivery.
            msg("reload_config")
            focus(remote)
            key("alt+j")
            wait_for(lambda: focused(local))
            assert received(remote_log, "j") == 0

            focus(remote)
            key("ctrl+super+F12")
            for chord, sym in [("alt+j", "j"), ("alt+2", "2"), ("alt+space", "space")]:
                key(chord)
                wait_for(lambda: received(remote_log, sym) >= 2)  # press AND release
                assert focused(remote), chord
            assert received(remote_log, "F12") == 0
            # Reload and MappingNotify must not bring the old grabs back.
            config.write_text(text + "\n# reload during passthrough\n")
            time.sleep(0.7)
            run("xmodmap", "-e", "keycode 200 = F35")
            time.sleep(0.2)
            before = received(remote_log, "j")
            key("alt+j")
            wait_for(lambda: received(remote_log, "j") > before)
            # A real held key generates X11 autorepeat, not repeated taps.
            run("xdotool", "keydown", "--delay", "120", "ctrl", "super", "F12")
            time.sleep(1)
            run("xdotool", "keyup", "F12", "super", "ctrl")
            key("alt+j")
            wait_for(lambda: focused(local))  # exited exactly once
            assert received(local_log, "j") == 0

            focus(remote)
            key("ctrl+super+F12")
            focus(local)
            key("alt+j")
            wait_for(lambda: focused(remote))
            before = received(remote_log, "j")
            key("alt+j")
            wait_for(lambda: focused(local))  # returning did not re-enable
            assert received(remote_log, "j") == before

            focus(remote)
            key("ctrl+super+F12")
            remote_process.terminate()
            remote_process.wait(timeout=5)
            wait_for(lambda: focused(local))
            key("alt+n")
            wait_for(lambda: next(w for w in msg("get_windows") if w["id"] == local)["is_minimized"])
            print(f"PASS ({BACKEND}): actual press/release forwarding, chord, reload, held toggle, focus, close")
        except BaseException:
            for output in logs:
                output.flush()
                output.seek(0)
                print(output.read()[-20000:])
            raise
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
            for output in logs:
                output.close()


if __name__ == "__main__":
    main()
