#!/usr/bin/env python3
"""Native remote resize E2E on two owned disposable Xvfb servers.

Supply prebuilt binaries. No builds, dependency installs, user display or user
IPC are used. Host uses real JWM x11rb plus xcompmgr and a private generated
configuration with appearance.border_px=0. This explicit initial-backend test
restriction is NOT proof of default/decorated window support. --self-test opens
no sockets and starts no processes. Actual E2E belongs in permitted CI only.
"""
import argparse
import ctypes as C
import ctypes.util
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("owned_desktop", ROOT / "eva/tests/desktop-ipc.py")
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)
NONCE = "_JWM_REMOTE_RESIZE_CHALLENGE"


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def wait(predicate, label, timeout=15):
    deadline, last = time.monotonic() + timeout, None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, RuntimeError) as error:
            last = str(error)
        time.sleep(0.05)
    raise RuntimeError(f"{label} timed out: {last}")


def private_environment(work):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("JWM_", "OPENAI_", "PULSE_", "PIPEWIRE_"))
           and k not in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                         "DBUS_SYSTEM_BUS_ADDRESS", "NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC")}
    for key, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                      ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
        (work / leaf).mkdir(mode=0o700)
        env[key] = str(work / leaf)
    env.update(LIBGL_ALWAYS_SOFTWARE="1", RUST_LOG="info",
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"))
    return env


class IPC:
    """A persistent stream owns a grant; never silently reconnect."""
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(4)
        self.sock.connect(str(path))
        self.reader = self.sock.makefile("rb")

    def send(self, command, args):
        self.sock.sendall(json.dumps({"command": command, "args": args}).encode() + b"\n")

    def response(self, command, success=True):
        line = self.reader.readline(1024 * 1024 + 1)
        require(line.endswith(b"\n") and len(line) <= 1024 * 1024, "invalid IPC response frame")
        result = json.loads(line)
        require(result.get("success") is success, f"{command}: {result}")
        return result.get("data") if success else result

    def command(self, command, args, success=True):
        self.send(command, args)
        return self.response(command, success)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.reader.close()
        self.sock.close()


class WindowAttributes(C.Structure):
    _fields_ = [(name, kind) for kind, names in (
        (C.c_int, "x y width height border_width depth"), (C.c_void_p, "visual"),
        (C.c_ulong, "root"), (C.c_int, "class_ bit_gravity win_gravity backing_store"),
        (C.c_ulong, "backing_planes backing_pixel"), (C.c_int, "save_under"),
        (C.c_ulong, "colormap"), (C.c_int, "map_installed map_state"),
        (C.c_long, "all_event_masks your_event_mask do_not_propagate_mask"),
        (C.c_int, "override_redirect"), (C.c_void_p, "screen")) for name in names.split()]


class XWindows:
    def __init__(self, display):
        self.lib = C.CDLL(ctypes.util.find_library("X11"))
        p, u, i = C.c_void_p, C.c_ulong, C.c_int
        signatures = {
            "XOpenDisplay": ([C.c_char_p], p), "XDefaultRootWindow": ([p], u),
            "XCreateSimpleWindow": ([p, u, i, i, C.c_uint, C.c_uint, C.c_uint, u, u], u),
            "XInternAtom": ([p, C.c_char_p, i], u),
            "XChangeProperty": ([p, u, u, u, i, i, p, i], i),
            "XStoreName": ([p, u, C.c_char_p], i), "XMapWindow": ([p, u], i),
            "XUnmapWindow": ([p, u], i), "XSync": ([p, i], i), "XCloseDisplay": ([p], i),
            "XGetWindowAttributes": ([p, u, C.POINTER(WindowAttributes)], i),
            "XGetGeometry": ([p, u, C.POINTER(u), C.POINTER(i), C.POINTER(i), C.POINTER(C.c_uint),
                              C.POINTER(C.c_uint), C.POINTER(C.c_uint), C.POINTER(C.c_uint)], i),
            "XGetInputFocus": ([p, C.POINTER(u), C.POINTER(i)], i),
            "XGetSelectionOwner": ([p, u], u), "XGrabServer": ([p], i), "XUngrabServer": ([p], i),
            "XDeleteProperty": ([p, u, u], i), "XFree": ([p], i),
            "XGetWindowProperty": ([p, u, u, C.c_long, C.c_long, i, u, C.POINTER(u),
                                    C.POINTER(i), C.POINTER(u), C.POINTER(u), C.POINTER(p)], i),
        }
        for name, (arguments, result) in signatures.items():
            fn = getattr(self.lib, name)
            fn.argtypes, fn.restype = arguments, result
        self.conn = self.lib.XOpenDisplay(display.encode())
        require(bool(self.conn), "owned X display unavailable")
        self.root = self.lib.XDefaultRootWindow(self.conn)

    def atom(self, name):
        return self.lib.XInternAtom(self.conn, name.encode(), 0)

    def create(self, name, x):
        window = self.lib.XCreateSimpleWindow(self.conn, self.root, x, 70, 320, 240, 0, 0, 0x2288BB)
        # Floating DIALOG, deliberately no WM_TRANSIENT_FOR or fixed-size hints.
        kind = C.c_ulong(self.atom("_NET_WM_WINDOW_TYPE_DIALOG"))
        self.lib.XChangeProperty(self.conn, window, self.atom("_NET_WM_WINDOW_TYPE"),
                                 self.atom("ATOM"), 32, 0, C.byref(kind), 1)
        self.lib.XStoreName(self.conn, window, name.encode())
        self.lib.XSync(self.conn, 0)
        return window

    def map(self, window):
        self.lib.XMapWindow(self.conn, window)
        self.lib.XSync(self.conn, 0)

    def unmap_remap(self, window):
        self.lib.XUnmapWindow(self.conn, window)
        self.map(window)

    def viewable(self, window):
        attrs = WindowAttributes()
        return bool(self.lib.XGetWindowAttributes(self.conn, window, C.byref(attrs))) and attrs.map_state == 2

    def nonce(self, window, value):
        require(len(value) == 32, "invalid test challenge length")
        data = (C.c_ubyte * 32).from_buffer_copy(value)
        self.lib.XChangeProperty(self.conn, window, self.atom(NONCE), self.atom("STRING"),
                                 8, 0, C.cast(data, C.c_void_p), 32)
        self.lib.XSync(self.conn, 0)

    def clear_nonce(self, window, expected):
        atom, string = self.atom(NONCE), self.atom("STRING")
        actual, format_, count, remaining = C.c_ulong(), C.c_int(), C.c_ulong(), C.c_ulong()
        data = C.c_void_p()
        self.lib.XGrabServer(self.conn)
        try:
            status = self.lib.XGetWindowProperty(self.conn, window, atom, 0, 9, 0, string,
                C.byref(actual), C.byref(format_), C.byref(count), C.byref(remaining), C.byref(data))
            require(status == 0 and actual.value == string and format_.value == 8
                    and count.value == 32 and remaining.value == 0 and data.value
                    and C.string_at(data, 32) == expected, "refusing to delete non-owned test nonce")
            self.lib.XDeleteProperty(self.conn, window, atom)
        finally:
            if data.value:
                self.lib.XFree(data)
            self.lib.XUngrabServer(self.conn)
            self.lib.XSync(self.conn, 0)

    def geometry(self, window):
        root, x, y = C.c_ulong(), C.c_int(), C.c_int()
        width, height, border, depth = (C.c_uint() for _ in range(4))
        require(self.lib.XGetGeometry(self.conn, window, C.byref(root), C.byref(x), C.byref(y),
            C.byref(width), C.byref(height), C.byref(border), C.byref(depth)), "native window disappeared")
        return dict(x=x.value, y=y.value, width=width.value, height=height.value,
                    border=border.value, depth=depth.value)

    def focus(self):
        window, revert = C.c_ulong(), C.c_int()
        self.lib.XGetInputFocus(self.conn, C.byref(window), C.byref(revert))
        return window.value

    def compositor_owner(self):
        return self.lib.XGetSelectionOwner(self.conn, self.atom("_NET_WM_CM_S0"))

    def close(self):
        self.lib.XCloseDisplay(self.conn)


def size(geometry):
    return geometry["width"], geometry["height"]


def apply_args(grant, width, height):
    return dict(grant=grant["grant"], expected_width=grant["width"], expected_height=grant["height"],
                width=width, height=height)


def stop(child):
    if child.poll() is None:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)


def run(args):
    work = Path(tempfile.mkdtemp(prefix="jwm-window-resize-e2e-"))
    work.chmod(0o700)
    children, connections = FIXTURE.Children(), []
    report = dict(status="failed", directory=str(work), checks=[], backend="x11rb", test_config_border_px=0,
                  limitations=["borderless 24-bit floating clients only", "not default-config acceptance"])
    try:
        env = private_environment(work)
        (work / "config/jwm").mkdir(mode=0o700)
        def command(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, stderr=subprocess.STDOUT, text=True, timeout=30)
        command(args.jwm, "--backend", "x11rb", "--gen-config")
        configs = list((work / "config/jwm").glob("config_*.toml"))
        require(configs, "no generated private configuration")
        for config in configs:
            text = config.read_text()
            for section, key, literal, required in (
                ("appearance", "border_px", "0", True), ("status_bar", "show_bar", "false", True),
                ("status_bar", "name", '"/bin/true"', True), ("desktop_shell", "enabled", "false", False),
                ("behavior", "compositor", "false", True), ("behavior", "wallpaper_effect", '"off"', False)):
                text = FIXTURE.edit_setting(text, section, key, literal, required)
            config.write_text(text)
        command(args.jwm, "--backend", "x11rb", "--check-config")
        host_dir, viewer_dir = work / "host", work / "viewer"
        host_dir.mkdir(); viewer_dir.mkdir()
        env["DISPLAY"] = children.display("Xvfb", env, host_dir)
        view_env = private_environment(viewer_dir)
        view_env["DISPLAY"] = children.display("Xvfb", view_env, viewer_dir)
        # Open matching first native clients before either WM/compositor. Fail
        # rather than pretend to test cross-server aliasing if XIDs differ.
        host_x = XWindows(env["DISPLAY"]); connections.append(host_x)
        other_x = XWindows(view_env["DISPLAY"]); connections.append(other_x)
        target = host_x.create("Resize target", 40)
        collision = other_x.create("Foreign same-XID target", 40)
        require(target == collision, "exact-XID collision precondition failed")
        other = host_x.create("Unrelated client", 700)
        wm = children.spawn(["dbus-run-session", "--", args.jwm, "--backend", "x11rb"], env, work / "jwm.log")
        ipc = work / "runtime/jwm-ipc.sock"
        wait(lambda: ipc.is_socket() and wm.poll() is None, "private JWM IPC")
        compositor = children.spawn(["xcompmgr", "-a"], env, work / "compositor.log")
        wait(lambda: compositor.poll() is None and host_x.compositor_owner(), "private compositor selection")
        host_x.map(target); host_x.map(other); other_x.map(collision)
        challenge = secrets.token_bytes(32)
        host_x.nonce(target, challenge)
        def managed():
            with IPC(ipc) as client:
                grant = client.command("remote_resize_grant", dict(window=target, challenge=list(challenge)))
                client.command("remote_resize_revoke", dict(grant=grant["grant"]))
                return grant
        wait(managed, "managed floating target")
        geometry = host_x.geometry(target)
        require(geometry["border"] == 0 and geometry["depth"] == 24, "unsupported fixture border/depth")
        foreign = secrets.token_bytes(32)
        other_x.nonce(collision, foreign)
        with IPC(ipc) as client:
            result = client.command("remote_resize_grant", dict(window=collision, challenge=list(foreign)), False)
            require("challenge" in result.get("error", "").lower(), "cross-server rejection did not reach nonce proof")
        report["checks"].append("cross_server_exact_xid_nonce_rejected")
        with IPC(ipc) as owner, IPC(ipc) as stranger:
            grant = owner.command("remote_resize_grant", dict(window=target, challenge=list(challenge)))
            baseline, focus = host_x.geometry(other), host_x.focus()
            stranger.command("remote_resize_apply", apply_args(grant, 400, 300), False)
            require(size(host_x.geometry(target)) == (grant["width"], grant["height"]), "wrong owner resized target")
            owner.command("remote_resize_apply", apply_args(grant, 400, 300))
            wait(lambda: size(host_x.geometry(target)) == (400, 300), "native IPC resize")
            require(host_x.geometry(other) == baseline and host_x.focus() == focus, "native resize changed unrelated geometry/focus")
            owner.command("remote_resize_revoke", dict(grant=grant["grant"]))
            owner.command("remote_resize_apply", dict(apply_args(grant, 420, 310), expected_width=400, expected_height=300), False)
        report["checks"].append("native_grant_apply_revoke_and_wrong_owner")
        with IPC(ipc) as owner:
            expired = owner.command("remote_resize_grant", dict(window=target, challenge=list(challenge)))
        with IPC(ipc) as stranger:
            stranger.command("remote_resize_apply", apply_args(expired, 420, 310), False)
        wait(managed, "disconnect released owner grant")
        require(size(host_x.geometry(target)) == (400, 300), "disconnected owner changed geometry")
        report["checks"].append("disconnected_owner_revoked")
        with IPC(ipc) as owner:
            grant = owner.command("remote_resize_grant", dict(window=target, challenge=list(challenge)))
            before = size(host_x.geometry(target))
            require(wm.poll() is None, "owned WM exited before negative control")
            try:
                # Pause ONLY our child process group, never Xvfb. Remap can
                # await MapRequest handling. No deterministic dispatch-order
                # claim is made about IPC vs raw X events after resuming.
                os.killpg(wm.pid, signal.SIGSTOP)
                host_x.unmap_remap(target)
                owner.send("remote_resize_apply", apply_args(grant, 460, 330))
            finally:
                os.killpg(wm.pid, signal.SIGCONT)
            owner.response("remote_resize_apply", False)
            wait(lambda: host_x.viewable(target), "remapped target viewable")
            require(size(host_x.geometry(target)) == before, "expired lifecycle grant resized target")
        report["checks"].append("unmap_remap_old_grant_rejected_after_wm_resume")
        host_x.nonce(target, challenge)
        wait(managed, "fresh authorization after lifecycle negative control")
        host_x.clear_nonce(target, challenge)
        key = work / "remote.key"
        command(args.remote, "keygen", "--output", key)
        def start_host(label, allow):
            log = work / f"{label}-host.log"
            argv = [args.remote, "host", "--listen", "127.0.0.1:0", "--key-file", str(key),
                    "--window", str(target), "--once", "--fps", "10"]
            if allow:
                argv.append("--allow-window-resize")
            child = children.spawn(argv, env, log)
            def ready():
                text = log.read_text(errors="replace")
                require(child.poll() is None, f"{label} host exited: {text}")
                match = re.search(r"listening on (127\.0\.0\.1:\d+)", text)
                return match.group(1) if match else None
            return child, wait(ready, label + " host readiness")
        require(compositor.poll() is None, "compositor exited")
        host, address = start_host("permitted", True)
        baseline, focus = host_x.geometry(other), host_x.focus()
        log = work / "permitted-client.log"
        viewer = children.spawn([args.remote, "connect", address, "--key-file", str(key),
            "--view-only", "--resize-source", "480x320"], view_env, log)
        wait(lambda: size(host_x.geometry(target)) == (480, 320), "real host/connect resize")
        wait(lambda: re.search(r"source resize Applied: 480x320 \(capture geometry [1-9][0-9]*\)",
                              log.read_text(errors="replace")), "viewer applied acknowledgment")
        require(viewer.poll() is None and host.poll() is None, "resize session failed")
        require(host_x.geometry(other) == baseline and host_x.focus() == focus, "remote resize changed unrelated geometry/focus")
        report["checks"].append("real_host_connect_resize_native_geometry")
        stop(viewer); host.wait(timeout=15)
        # Host owns/replaces/removes its nonce, so recreate our test nonce.
        host_x.nonce(target, challenge)
        wait(managed, "remote disconnect released native grant")
        host_x.clear_nonce(target, challenge)
        host, address = start_host("denied", False)
        log = work / "denied-client.log"
        viewer = children.spawn([args.remote, "connect", address, "--key-file", str(key),
            "--view-only", "--resize-source", "500x340"], view_env, log)
        require(viewer.wait(timeout=15) != 0 and "has not authorized native window resizing" in log.read_text(errors="replace"),
                "no-permission client failed to reject absent capability")
        host.wait(timeout=15)
        require(size(host_x.geometry(target)) == (480, 320), "unpermitted resize changed geometry")
        require(host_x.geometry(other) == baseline and host_x.focus() == focus, "denied request changed unrelated geometry/focus")
        report["checks"].append("no_permission_rejected_without_resize")
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
    finally:
        for connection in reversed(connections):
            connection.close()
        children.close()
        if args.report:
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
        if report["status"] == "passed" and not args.keep:
            shutil.rmtree(work)
    return 0 if report["status"] == "passed" else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=lambda p: str(Path(p).resolve()))
    parser.add_argument("--remote", type=lambda p: str(Path(p).resolve()))
    parser.add_argument("--report", type=Path)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        require(apply_args(dict(grant=7, width=320, height=240), 480, 320) == dict(
            grant=7, expected_width=320, expected_height=240, width=480, height=320), "apply schema")
        with tempfile.TemporaryDirectory() as path:
            env = private_environment(Path(path))
            require("DISPLAY" not in env and "XAUTHORITY" not in env, "inherited display leaked")
            require(env["XDG_RUNTIME_DIR"] == str(Path(path) / "runtime"), "private IPC discovery")
        print("self-test passed (no sockets/processes)")
        return 0
    require(args.jwm and args.remote, "--jwm and --remote are required prebuilt executable paths")
    for binary in (args.jwm, args.remote):
        require(Path(binary).is_file() and os.access(binary, os.X_OK), f"missing executable {binary}")
    for program in ("Xvfb", "xcompmgr", "dbus-run-session"):
        require(shutil.which(program), f"missing CI prerequisite {program}")
    require(ctypes.util.find_library("X11"), "missing libX11")
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
