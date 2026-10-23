#!/usr/bin/env python3
"""Real host/viewer fault tests on two private Xvfb servers; stdlib only."""
import argparse
import contextlib
import ctypes as C
import ctypes.util
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import subprocess
import threading
import time


def wait_for(predicate, description, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(f"timed out: {description}")


class Process:
    def __init__(self, args, log, **kwargs):
        self.log = Path(log)
        self.file = self.log.open("wb")
        self.p = subprocess.Popen(args, stdout=self.file, stderr=subprocess.STDOUT, **kwargs)

    def text(self):
        return self.log.read_text(errors="replace")

    def stop(self):
        if self.p.poll() is None:
            self.p.send_signal(signal.SIGCONT)
            self.p.terminate()
            try:
                self.p.wait(3)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait(3)
        self.file.close()


class XServer:
    def __init__(self, output, name):
        read_fd, write_fd = os.pipe()
        try:
            self.process = Process(
                ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1024x768x24",
                 "-nolisten", "tcp", "-ac"], output / f"{name}.log", pass_fds=(write_fd,))
            os.close(write_fd)
            write_fd = -1
            if not select.select([read_fd], [], [], 5)[0]:
                raise AssertionError("Xvfb did not publish a display")
            self.display = ":" + os.read(read_fd, 64).decode().strip()
            self.x = XConnection(self.display)
        except BaseException:
            if hasattr(self, "process"):
                self.process.stop()
            raise
        finally:
            os.close(read_fd)
            if write_fd >= 0:
                os.close(write_fd)

    def command(self, *args):
        return subprocess.check_output(args, env={**os.environ, "DISPLAY": self.display},
                                       stderr=subprocess.STDOUT, timeout=5).decode().strip()

    def close(self):
        self.x.close()
        self.process.stop()


class XConnection:
    def __init__(self, display):
        self.lib = C.CDLL(ctypes.util.find_library("X11"))
        signatures = {
            "XOpenDisplay": (C.c_void_p, [C.c_char_p]),
            "XDefaultRootWindow": (C.c_ulong, [C.c_void_p]),
            "XCloseDisplay": (C.c_int, [C.c_void_p]),
            "XFlush": (C.c_int, [C.c_void_p]),
            "XSync": (C.c_int, [C.c_void_p, C.c_int]),
            "XStringToKeysym": (C.c_ulong, [C.c_char_p]),
            "XKeysymToKeycode": (C.c_ubyte, [C.c_void_p, C.c_ulong]),
            "XQueryKeymap": (C.c_int, [C.c_void_p, C.c_void_p]),
            "XQueryPointer": (C.c_int, [C.c_void_p, C.c_ulong, C.c_void_p, C.c_void_p,
                                         C.c_void_p, C.c_void_p, C.c_void_p, C.c_void_p, C.c_void_p]),
            "XSetWindowBackground": (C.c_int, [C.c_void_p, C.c_ulong, C.c_ulong]),
            "XClearWindow": (C.c_int, [C.c_void_p, C.c_ulong]),
            "XGrabKeyboard": (C.c_int, [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_int, C.c_ulong]),
            "XUngrabKeyboard": (C.c_int, [C.c_void_p, C.c_ulong]),
            "XInternAtom": (C.c_ulong, [C.c_void_p, C.c_char_p, C.c_int]),
            "XCreateSimpleWindow": (C.c_ulong, [C.c_void_p, C.c_ulong, C.c_int, C.c_int,
                                               C.c_uint, C.c_uint, C.c_uint, C.c_ulong, C.c_ulong]),
            "XSetSelectionOwner": (C.c_int, [C.c_void_p, C.c_ulong, C.c_ulong, C.c_ulong]),
            "XDestroyWindow": (C.c_int, [C.c_void_p, C.c_ulong]),
            "XGetImage": (C.c_void_p, [C.c_void_p, C.c_ulong, C.c_int, C.c_int,
                                      C.c_uint, C.c_uint, C.c_ulong, C.c_int]),
            "XGetPixel": (C.c_ulong, [C.c_void_p, C.c_int, C.c_int]),
            "XDestroyImage": (C.c_int, [C.c_void_p]),
        }
        for name, (restype, argtypes) in signatures.items():
            function = getattr(self.lib, name)
            function.restype, function.argtypes = restype, argtypes
        self.d = self.lib.XOpenDisplay(display.encode())
        if not self.d:
            raise AssertionError(f"cannot connect to private display {display}")
        self.root = self.lib.XDefaultRootWindow(self.d)

    def down(self, name="Shift_L"):
        code = self.lib.XKeysymToKeycode(self.d, self.lib.XStringToKeysym(name.encode()))
        if not code:
            raise AssertionError(f"missing test keysym {name}")
        keys = C.create_string_buffer(32)
        self.lib.XQueryKeymap(self.d, keys)
        return bool(keys.raw[code // 8] & (1 << (code % 8)))

    def background(self, color):
        self.lib.XSetWindowBackground(self.d, self.root, color)
        self.lib.XClearWindow(self.d, self.root)
        self.lib.XSync(self.d, 0)

    def pointer(self):
        root, child = C.c_ulong(), C.c_ulong()
        rx, ry, wx, wy, mask = C.c_int(), C.c_int(), C.c_int(), C.c_int(), C.c_uint()
        assert self.lib.XQueryPointer(self.d, self.root, C.byref(root), C.byref(child),
                                      C.byref(rx), C.byref(ry), C.byref(wx), C.byref(wy), C.byref(mask))
        return rx.value, ry.value, mask.value

    def pixel(self, window, x, y):
        image = self.lib.XGetImage(self.d, window, x, y, 1, 1, C.c_ulong(-1).value, 2)
        if not image:
            return None
        try:
            value = self.lib.XGetPixel(image, 0, 0)
            return ((value >> 16) & 255, (value >> 8) & 255, value & 255)
        finally:
            self.lib.XDestroyImage(image)

    @contextlib.contextmanager
    def blocked_clipboard(self):
        atom = self.lib.XInternAtom(self.d, b"CLIPBOARD", 0)
        window = self.lib.XCreateSimpleWindow(self.d, self.root, 0, 0, 1, 1, 0, 0, 0)
        self.lib.XSetSelectionOwner(self.d, atom, window, 0)
        self.lib.XSync(self.d, 0)
        try:
            # Intentionally do not service SelectionRequest events.
            yield
        finally:
            self.lib.XSetSelectionOwner(self.d, atom, 0, 0)
            self.lib.XDestroyWindow(self.d, window)
            self.lib.XSync(self.d, 0)

    def close(self):
        self.lib.XCloseDisplay(self.d)


class Proxy:
    def __init__(self, address):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.listener.settimeout(5)
        self.address = "127.0.0.1:" + str(self.listener.getsockname()[1])
        self.upstream = address
        self.sockets = []
        self.stop_event = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            client, _ = self.listener.accept()
            server = socket.create_connection(self.upstream, timeout=5)
            self.sockets = [client, server]
            for sock in self.sockets:
                sock.settimeout(2)
            while not self.stop_event.is_set():
                ready, _, _ = select.select(self.sockets, [], [], 0.1)
                for sock in ready:
                    data = sock.recv(65536)
                    if not data:
                        return
                    self.sockets[1 if sock is client else 0].sendall(data)
        except (OSError, ValueError) as error:
            if not self.stop_event.is_set():
                self.error = str(error)
        finally:
            for sock in self.sockets:
                sock.close()

    def close(self):
        self.stop_event.set()
        for sock in self.sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.listener.close()
        self.thread.join(6)
        assert not self.thread.is_alive(), "fault proxy leaked a thread"


class Session:
    def __init__(self, binary, output, key, host_display, viewer_display, *, view_only=False, scope=()):
        self.host_display, self.viewer_display = host_display, viewer_display
        self.host = self.viewer = self.proxy = None
        try:
            self.host = Process([binary, "host", "--listen", "127.0.0.1:0", "--key-file", str(key),
                                 "--display", host_display.display, "--capture-source", "root",
                                 "--allow-input", "--allow-clipboard", *scope], output / "host.log")
            match = wait_for(lambda: re.search(r"listening on 127\.0\.0\.1:(\d+)", self.host.text()),
                             "host listener")
            self.baseline_threads = self.threads()
            self.proxy = Proxy(("127.0.0.1", int(match[1])))
            self.viewer = Process([binary, "connect", self.proxy.address, "--key-file", str(key),
                                   "--display", viewer_display.display, "--clipboard",
                                   *(["--view-only"] if view_only else [])], output / "viewer.log")
            wait_for(lambda: "connected;" in self.viewer.text(), "first frame/ACK")
            self.window = int(viewer_display.command("xdotool", "search", "--onlyvisible", "--class", "^JwmRemote$").splitlines()[0])
            viewer_display.command("xdotool", "windowfocus", str(self.window))
            self.peak_threads = self.threads()
        except BaseException:
            self.close()
            raise

    def threads(self):
        return len(list(Path(f"/proc/{self.host.p.pid}/task").iterdir()))

    def key(self, down):
        self.viewer_display.command("xdotool", "keydown" if down else "keyup", "Shift_L")

    def press(self):
        self.key(True)
        wait_for(self.host_display.x.down, "authenticated key press reaches host")

    def color(self, expected, x=160, y=120):
        def matches():
            pixel = self.viewer_display.x.pixel(self.window, x, y)
            return pixel and all(abs(a - b) <= 14 for a, b in zip(pixel, expected))
        wait_for(matches, f"viewer pixel converges to {expected}")

    def disconnect(self):
        self.proxy.close()
        wait_for(lambda: self.viewer.p.poll() is not None, "viewer exits after lost transport", timeout=10)
        wait_for(lambda: self.threads() <= self.baseline_threads, "host reaps all session workers", timeout=10)

    def close(self):
        if self.viewer:
            self.viewer.stop()
        if self.proxy:
            self.proxy.close()
        if self.host:
            self.host.stop()


def exercise(name, session):
    host, viewer = session.host_display, session.viewer_display
    metrics = {}
    if name == "disconnect_held_key":
        session.press()
        started = time.monotonic()
        session.proxy.close()
        wait_for(lambda: not host.x.down(), "disconnect releases held key", timeout=2)
        assert time.monotonic() - started < 2
        viewer.command("xdotool", "keyup", "Shift_L")
    elif name == "silent_slow_viewer":
        session.press()
        def rss_kib():
            return int(re.search(r"VmRSS:\s+(\d+)", Path(f"/proc/{session.host.p.pid}/status").read_text())[1])
        rss_before = rss_kib()
        os.kill(session.viewer.p.pid, signal.SIGSTOP)
        try:
            started = time.monotonic()
            wait_for(lambda: not host.x.down(), "600ms silence policy releases key", timeout=2)
            assert time.monotonic() - started < 2
            assert session.threads() <= session.peak_threads + 1, "slow peer grows thread count"
            peak_rss = rss_kib()
            for index in range(24):
                host.x.background(0x110000 * (index % 8))
                time.sleep(0.04)
                peak_rss = max(peak_rss, rss_kib())
            assert peak_rss - rss_before < 48 * 1024, "slow viewer caused unbounded RSS growth"
            metrics["slow_viewer_rss_growth_kib"] = peak_rss - rss_before
            host.x.background(0x2244CC)
        finally:
            os.kill(session.viewer.p.pid, signal.SIGCONT)
        session.key(False)
        session.color((34, 68, 204))
    elif name == "view_only":
        pointer_before = host.x.pointer()
        session.key(True)
        viewer.command("xdotool", "mousemove", "--window", str(session.window), "40", "30")
        time.sleep(0.3)
        assert not host.x.down(), "view-only injected a key"
        assert host.x.pointer() == pointer_before, "view-only injected pointer input"
        session.key(False)
    elif name == "external_keyboard_grab":
        session.press()
        assert viewer.x.lib.XGrabKeyboard(viewer.x.d, viewer.x.root, 0, 1, 1, 0) == 0
        viewer.x.lib.XSync(viewer.x.d, 0)
        try:
            wait_for(lambda: not host.x.down(), "external grab releases remote key", timeout=2)
        finally:
            viewer.x.lib.XUngrabKeyboard(viewer.x.d, 0)
            viewer.x.lib.XSync(viewer.x.d, 0)
            session.key(False)
    elif name == "static_viewport_resize":
        host.x.background(0xCC2244)
        session.color((204, 34, 68))
        viewer.command("xdotool", "windowsize", str(session.window), "640", "480")
        session.color((204, 34, 68), 80, 80)
        wait_for(lambda: "encoding to fit 640x480" in session.host.text(), "static scene recaptured at resized viewport")
    elif name == "blocked_clipboard":
        with viewer.x.blocked_clipboard():
            time.sleep(0.2)
            session.press()
            session.key(False)
            wait_for(lambda: not host.x.down(), "key release while clipboard owner stalls")
            host.x.background(0x33CC55)
            session.color((51, 204, 85))
    elif name == "scoped_monitor_removal":
        host.command("xrandr", "--delmonitor", "e2e-scope")
        wait_for(lambda: session.viewer.p.poll() is not None, "removed share scope terminates session", timeout=10)
        wait_for(lambda: 'no RandR monitor named "e2e-scope"' in session.host.text(),
                 "host reports the removed scope without falling back to root")
    elif name == "worker_lifecycle":
        session.press()
        session.key(False)
        wait_for(lambda: not host.x.down(), "ordinary key release")
    else:
        raise AssertionError(name)
    session.disconnect()
    if name == "silent_slow_viewer":
        maxima = [int(value) for value in re.findall(r"outstanding current/max \d+/(\d+)", session.host.text())]
        assert maxima and max(maxima) <= 2, "host exceeded the two-frame credit bound"
        assert "controller silent for 600 ms" in session.host.text()
        metrics["max_outstanding_frames"] = max(maxima)
    return {"host_threads_before": session.baseline_threads,
            "host_threads_after": session.threads(), "session_threads": session.peak_threads, **metrics}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    for command in ["Xvfb", "xdotool", "xrandr"]:
        if not shutil.which(command):
            parser.error(f"required test dependency unavailable: {command}")
    binary = str(args.binary.resolve(strict=True))
    args.output.mkdir(parents=True, exist_ok=True)
    key = args.output / f"test-key-{os.getpid()}"
    subprocess.run([binary, "keygen", "--output", str(key)], check=True, stdout=subprocess.DEVNULL)
    results = []
    try:
        for name in ["disconnect_held_key", "silent_slow_viewer", "view_only",
                     "external_keyboard_grab", "static_viewport_resize", "blocked_clipboard",
                     "scoped_monitor_removal", "worker_lifecycle"]:
            case = args.output / name
            case.mkdir(exist_ok=True)
            host = viewer = session = None
            started = time.monotonic()
            try:
                host = XServer(case, "host-xvfb")
                viewer = XServer(case, "viewer-xvfb")
                scope = ()
                if name == "scoped_monitor_removal":
                    host.command("xrandr", "--setmonitor", "e2e-scope", "640/160x480/120+0+0", "none")
                    scope = ("--monitor", "e2e-scope")
                session = Session(binary, case, key, host, viewer, view_only=name == "view_only", scope=scope)
                metrics = exercise(name, session)
                result = {"case": name, "status": "passed", **metrics}
            except Exception as error:
                result = {"case": name, "status": "failed", "error": str(error)}
            finally:
                if session:
                    session.close()
                if viewer:
                    viewer.close()
                if host:
                    host.close()
            result["seconds"] = round(time.monotonic() - started, 3)
            results.append(result)
            print(json.dumps(result), flush=True)
    finally:
        key.unlink(missing_ok=True)
    report = {"schema_version": 1, "capture": "Xvfb root (not real compositor overlay)", "cases": results}
    (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    return int(any(result["status"] != "passed" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
