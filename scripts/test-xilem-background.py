#!/usr/bin/env python3
"""Verify Xilem's actual X11 alpha pixels and live JWM frost/clear switching.

Requires built jwm/jwm-tool and xilem_bar, Xvfb, xdotool, libX11 and Pillow.
Everything runs on a private display with private configuration and IPC.
"""

import argparse
import ctypes
import ctypes.util
import json
import os
from pathlib import Path
import re
import select
import signal
import statistics
import subprocess
import tempfile
import time

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]


class Pixels:
    def __init__(self, display):
        self.x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
        self.x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x11.XOpenDisplay.restype = ctypes.c_void_p
        self.x11.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                                      ctypes.c_int, ctypes.c_uint, ctypes.c_uint,
                                      ctypes.c_ulong, ctypes.c_int]
        self.x11.XGetImage.restype = ctypes.c_void_p
        self.x11.XGetPixel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        self.x11.XGetPixel.restype = ctypes.c_ulong
        self.x11.XDestroyImage.argtypes = [ctypes.c_void_p]
        self.x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.display = self.x11.XOpenDisplay(display.encode())
        assert self.display

    def sample(self, window, x, y):
        image = self.x11.XGetImage(self.display, int(window), x, y, 1, 1,
                                  ctypes.c_ulong(-1).value, 2)
        assert image, "XGetImage failed"
        try:
            pixel = self.x11.XGetPixel(image, 0, 0)
            return [(pixel >> shift) & 255 for shift in (16, 8, 0, 24)]
        finally:
            self.x11.XDestroyImage(image)

    def close(self):
        self.x11.XCloseDisplay(self.display)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--bar-bin", type=Path,
                        default=ROOT / "target/bar-install/xilem_bar/release/xilem_bar")
    parser.add_argument("--backend", choices=["xcb", "x11rb"], default="xcb")
    args = parser.parse_args()
    for path in (args.bin_dir / "jwm", args.bin_dir / "jwm-tool", args.bar_bin):
        assert path.is_file(), path
    children, bar_pids = [], set()
    with tempfile.TemporaryDirectory(prefix="jwm-xilem-background-") as directory:
        work = Path(directory)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", JWM_COMPOSITOR_API="glx",
                   XBAR_CONFIG=str(work / "xbar.toml"), RUST_LOG="warn,xilem_bar=info")
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                    "JWM_SOCKET", "JWM_BACKEND", "JWM_BAR_LAYOUT", "JWM_DAEMON_PID",
                    "JWM_BAR_POSITION", "JWM_BAR_THICKNESS", "JWM_DESKTOP_SHELL"):
            env.pop(key, None)
        for key, folder in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                            ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                            ("XDG_RUNTIME_DIR", "runtime"), ("XDG_PICTURES_DIR", "pictures")):
            (work / folder).mkdir(mode=0o700)
            env[key] = str(work / folder)
        (work / "bin").mkdir()
        (work / "bin/xilem_bar").symlink_to(args.bar_bin.resolve())
        env["PATH"] = str(work / "bin") + os.pathsep + env.get("PATH", "")
        xbar = work / "xbar.toml"
        xbar.write_text('theme="dark"\n')
        log = (work / "session.log").open("w+")
        pixels = None

        def run(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=15)

        def spawn(*argv, **kwargs):
            child = subprocess.Popen(list(map(str, argv)), env=env, stdout=log, stderr=log,
                                     start_new_session=True, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate, label, timeout=40):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, AssertionError, subprocess.SubprocessError) as error:
                    last = str(error)
                time.sleep(0.1)
            raise AssertionError(f"{label}; last={last}")

        def msg(command, values=None):
            argv = [args.bin_dir / "jwm-tool", "msg", command, "--raw"]
            if values is not None:
                argv += ["--args", json.dumps(values)]
            reply = json.loads(run(*argv))
            assert reply["success"], reply
            return reply.get("data")

        def window(pid=None):
            search = ["xdotool", "search", "--onlyvisible"]
            search += ["--pid", str(pid)] if pid else ["--class", "^xilem_bar$"]
            for xid in run(*search).split():
                info = run("xwininfo", "-id", xid)
                width = int(re.search(r"Width: (\d+)", info)[1])
                if width >= 600:
                    depth = int(re.search(r"Depth: (\d+)", info)[1])
                    owner = int(re.search(r"= (\d+)", run("xprop", "-id", xid, "_NET_WM_PID"))[1])
                    bar_pids.add(owner)
                    return xid, width, depth
            return None

        def alpha_is(surface, expected):
            xid, width, _ = surface
            rgba = pixels.sample(xid, width // 2, 5)
            return rgba if abs(rgba[3] - expected) <= 2 else None

        def screenshot():
            path = Path(msg("take_screenshot_fullscreen")["path"])
            assert path.is_relative_to(work / "pictures"), path
            wait_for(lambda: path.is_file() and path.stat().st_size > 32, "screenshot")
            with Image.open(path) as source:
                return source.convert("RGB")

        try:
            read_fd, write_fd = os.pipe()
            try:
                spawn("Xvfb", "-displayfd", write_fd, "-screen", "0", "1280x800x24",
                      "-nolisten", "tcp", "-noreset", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                assert select.select([read_fd], [], [], 10)[0], "Xvfb startup"
                env["DISPLAY"] = ":" + os.read(read_fd, 32).decode().strip()
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            pixels = Pixels(env["DISPLAY"])
            bare = spawn(args.bar_bin.resolve())
            surface = wait_for(lambda: window(bare.pid), "bar without compositor")
            assert surface[2] == 24, surface
            bare.terminate()
            bare.wait(timeout=10)
            print("PASS: no compositor retains the depth-24 solid fallback", flush=True)

            wallpaper = work / "checker.png"
            image = Image.new("RGB", (1280, 800))
            image.putdata([(235, 235, 235) if (x // 4 + y // 4) % 2 else (20, 20, 20)
                           for y in range(800) for x in range(1280)])
            image.save(wallpaper)
            run(args.bin_dir / "jwm", "--backend", args.backend, "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            text = config.read_text()
            for key, value in {"wallpaper": json.dumps(str(wallpaper)),
                               "wallpaper_effect": '"off"', "wallpaper_colors": "false",
                               "wallpaper_mode": '"stretch"', "blur_status_bar": "false",
                               "status_bar_height": "40", "status_bar_padding": "3"}.items():
                text, count = re.subn(r"(?m)^" + key + r" = .*$", f"{key} = {value}", text)
                assert count == 1, (key, count)
            text = re.sub(r"(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)",
                          '[status_bar]\nname="xilem_bar"\nshow_bar=true\nlayout="single"\nposition="top"\n\n', text)
            text = re.sub(r"(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)", "", text)
            text += "\n[desktop_shell]\nenabled=false\n"
            config.write_text(text)
            wm = spawn("dbus-run-session", "--", args.bin_dir / "jwm", "--backend", args.backend)
            surface = wait_for(window, "managed Xilem bar")
            assert surface[2] == 32, surface
            clear = wait_for(lambda: alpha_is(surface, 0), "clear framebuffer alpha")
            clear_image = screenshot()
            print(f"PASS: compositor creates depth-32 ARGB; clear pixel={clear}", flush=True)

            msg("set_config", {"key": "behavior.blur_status_bar", "value": True})
            frosted = wait_for(lambda: alpha_is(surface, 140), "live frosted framebuffer alpha")
            # The 4px wallpaper checker must be blurred, beyond mere tinting.
            def variation(image):
                return statistics.pstdev(image.getpixel((x, 8))[0] for x in range(460, 820))
            sharp = variation(clear_image)
            assert sharp > 50, sharp
            # Alpha updates before the compositor's temporal blur cache settles.
            blurred = wait_for(lambda: (value if (value := variation(screenshot())) < sharp * 0.25 else None),
                               "compositor blur settles")
            print(f"PASS: live frost pixel={frosted}; checker variation {sharp:.1f} -> {blurred:.1f}", flush=True)
            msg("set_config", {"key": "behavior.blur_status_bar", "value": False})
            wait_for(lambda: alpha_is(surface, 0), "live return to clear")
            print("PASS: disabling frost restores clear pixels without restarting", flush=True)

            # A second bar with the same WM_CLASS is intentionally hidden by
            # JWM. Restart the private session for the startup-only override.
            managed_pids = set(bar_pids)
            os.killpg(wm.pid, signal.SIGTERM)
            wm.wait(timeout=10)
            for pid in managed_pids:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            wait_for(lambda: all(not Path(f"/proc/{pid}").exists() for pid in managed_pids),
                     "old private frontends exited")
            xbar.write_text('theme="light"\nbackground_opacity=0.25\n')
            spawn("dbus-run-session", "--", args.bin_dir / "jwm", "--backend", args.backend)
            override_surface = wait_for(window, "explicit opacity bar")
            override = wait_for(lambda: alpha_is(override_surface, 64), "explicit opacity framebuffer")
            assert all(channel <= override[3] for channel in override[:3]), override
            print(f"PASS: light theme preserves explicit 25% opacity and premultiplied RGB: {override}", flush=True)
            log.flush()
            content = (work / "session.log").read_text()
            probe = re.findall(r"alpha probe: adapter=.*?backend: (\w+)", content)
            renderer = re.findall(r"renderer adapter: .*?backend: (\w+)", content)
            assert probe and renderer and probe[-1] == renderer[-1], (probe, renderer)
            print(f"PASS: probe and actual renderer use the same backend: {probe[-1]}", flush=True)
        except BaseException:
            log.flush()
            print((work / "session.log").read_text()[-8000:], flush=True)
            raise
        finally:
            if pixels:
                pixels.close()
            # Only terminate frontends proven to belong to this private display.
            for pid in bar_pids:
                try:
                    if f'DISPLAY={env["DISPLAY"]}'.encode() in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"):
                        os.kill(pid, signal.SIGTERM)
                except (ProcessLookupError, FileNotFoundError):
                    pass
            for child in reversed(children):
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            for child in reversed(children):
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    main()
