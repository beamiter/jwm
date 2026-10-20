#!/usr/bin/env python3
"""Isolated Xvfb regression for the default restore shortcut (no live desktop).

Build jwm and jwm-tool with backend-x11rb first. Requires Xvfb, xdpyinfo,
xdotool and xmessage. Exits nonzero on any missing dependency or assertion.
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


def main():
    for command in ["Xvfb", "xdpyinfo", "xdotool", "xmessage"]:
        if not shutil.which(command):
            raise RuntimeError(f"missing required executable: {command}")
    with tempfile.TemporaryDirectory(prefix="jwm-restore-x11-") as directory:
        work = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"),
                   XDG_RUNTIME_DIR=str(work / "runtime"),
                   XDG_CACHE_HOME=str(work / "cache"), LIBGL_ALWAYS_SOFTWARE="1")
        (work / "runtime").mkdir(mode=0o700)
        # Fail clearly in sandboxes that prohibit even AF_UNIX creation,
        # before Xvfb scans tens of thousands of unavailable display numbers.
        probe = socket.socket(socket.AF_UNIX)
        try:
            probe.bind(str(work / "socket-probe"))
        finally:
            probe.close()
        children = []
        log = (work / "session.log").open("w+")

        def run(*args):
            return subprocess.check_output(args, env=env, text=True, stderr=subprocess.STDOUT, timeout=10)

        def spawn(*args, **kwargs):
            child = subprocess.Popen(args, env=env, stdout=log, stderr=log, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    result = predicate()
                    if result:
                        return result
                except (subprocess.CalledProcessError, OSError, ValueError):
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

        def windows():
            return msg("get_windows")

        def key(chord):
            run("xdotool", "key", "--clearmodifiers", chord)
            time.sleep(0.15)

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
            run(str(BIN / "jwm"), "--backend", "x11rb", "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            text = config.read_text().replace('show_bar = true', 'show_bar = false')
            # Exercise native preview rendering with software GL; no bar child.
            text = text.replace('wallpaper_effect = "daily"', 'wallpaper_effect = "none"')
            config.write_text(text)
            spawn(str(BIN / "jwm"), "--backend", "x11rb")
            wait_for(lambda: msg("get_monitors"))
            spawn("xmessage", "-name", "RestoreRegression", "-geometry", "400x260+120+100", "restore regression")
            target = wait_for(lambda: next((w for w in windows() if "RestoreRegression" in str(w)), None))
            target_id = target["id"]
            current = lambda: next(w for w in windows() if w["id"] == target_id)
            wait_for(lambda: current().get("is_focused"))
            if not current()["is_floating"]:
                msg("togglefloating")
            before = wait_for(lambda: (w if (w := current())["is_floating"] else None))
            key("alt+n")
            wait_for(lambda: current()["is_minimized"])
            hidden = current()
            key("alt+ctrl+n")
            key("Escape")
            after_cancel = current()
            for field in ["is_minimized", "tags", "x", "y", "w", "h", "is_focused"]:
                assert after_cancel[field] == hidden[field], (field, hidden, after_cancel)
            msg("view", {"tag": 2})
            key("alt+ctrl+n")
            key("Return")
            restored = wait_for(lambda: w if not (w := current())["is_minimized"] and w["is_focused"] else None)
            for field in ["tags", "x", "y", "w", "h"]:
                assert restored[field] == before[field], (field, before, restored)
            print("PASS: Alt+N; Alt+Ctrl+N/Escape cancellation; cross-tag floating restore, geometry and focus")
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read()[-12000:])
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
            log.close()


if __name__ == "__main__":
    main()
