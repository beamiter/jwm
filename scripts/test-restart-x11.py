#!/usr/bin/env python3
"""Real hotkey/IPC restart regression on hidden, owned X11 desktops.

No cargo, user display, audio, daemon control, or user configuration is used.
Every case owns Xvfb -> Xephyr, private XDG/HOME and a private D-Bus session.
The PNG is generated locally to exercise native root-pixmap initialization.
"""
import argparse
import ctypes
import ctypes.util
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("private_desktop_fixture", ROOT / "eva/tests/desktop-ipc.py")
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)
REQUIRED = ("Xvfb", "Xephyr", "dbus-run-session", "xdotool", "xmessage", "xprop", "xdpyinfo")


def png(path):
    width, height = 32, 24
    pixels = b"".join(b"\0" + bytes(channel for x in range(width)
                                   for channel in (40 + x * 5, 30 + y * 7, 110))
                      for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    path.write_bytes(b"\x89PNG\r\n\x1a\n"
                     + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))


def identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
        if fields[0] == "Z":
            return None
        return {"pid": pid, "start_ticks": int(fields[19]), "parent": int(fields[1])}
    except (OSError, ValueError, IndexError):
        return None


def native_pixmap_geometry(display, pixmap):
    """Read an X drawable on this private server; tolerate stale properties."""
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
    pointer = ctypes.c_void_p
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XOpenDisplay.restype = pointer
    xlib.XCloseDisplay.argtypes = [pointer]
    xlib.XSync.argtypes = [pointer, ctypes.c_int]
    callback_type = ctypes.CFUNCTYPE(ctypes.c_int, pointer, pointer)
    errors = []
    callback = callback_type(lambda _display, _event: errors.append(True) or 0)
    xlib.XSetErrorHandler.argtypes = [pointer]
    xlib.XSetErrorHandler.restype = pointer
    xlib.XGetGeometry.argtypes = [pointer, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong),
                                 ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
                                 ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint),
                                 ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint)]
    connection = xlib.XOpenDisplay(display.encode())
    if not connection:
        raise RuntimeError("private X display became unavailable")
    old_handler = xlib.XSetErrorHandler(ctypes.cast(callback, pointer))
    try:
        root = ctypes.c_ulong()
        x, y = ctypes.c_int(), ctypes.c_int()
        width, height, border, depth = (ctypes.c_uint() for _ in range(4))
        ok = xlib.XGetGeometry(connection, pixmap, ctypes.byref(root), ctypes.byref(x), ctypes.byref(y),
                              ctypes.byref(width), ctypes.byref(height), ctypes.byref(border), ctypes.byref(depth))
        xlib.XSync(connection, 0)
        if not ok or errors:
            raise RuntimeError("published root pixmap is no longer a live X drawable")
        return {"pixmap": pixmap, "width": width.value, "height": height.value, "depth": depth.value}
    finally:
        xlib.XCloseDisplay(connection)
        xlib.XSetErrorHandler(old_handler)



def publish_foreign_owner(display, resource):
    """Install a stale typed PIXMAP hint naming a live, foreign X client."""
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
    pointer = ctypes.c_void_p
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XOpenDisplay.restype = pointer
    xlib.XDefaultRootWindow.argtypes = [pointer]
    xlib.XDefaultRootWindow.restype = ctypes.c_ulong
    xlib.XInternAtom.argtypes = [pointer, ctypes.c_char_p, ctypes.c_int]
    xlib.XInternAtom.restype = ctypes.c_ulong
    xlib.XChangeProperty.argtypes = [pointer, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong,
                                    ctypes.c_int, ctypes.c_int, pointer, ctypes.c_int]
    xlib.XSync.argtypes = [pointer, ctypes.c_int]
    xlib.XCloseDisplay.argtypes = [pointer]
    connection = xlib.XOpenDisplay(display.encode())
    if not connection:
        raise RuntimeError("private display unavailable while installing stale owner")
    try:
        root = xlib.XDefaultRootWindow(connection)
        kind = xlib.XInternAtom(connection, b"PIXMAP", 0)
        value = ctypes.c_ulong(resource)
        for name in (b"_XROOTPMAP_ID", b"ESETROOT_PMAP_ID"):
            atom = xlib.XInternAtom(connection, name, 0)
            xlib.XChangeProperty(connection, root, atom, kind, 32, 0, ctypes.byref(value), 1)
        xlib.XSync(connection, 0)
    finally:
        xlib.XCloseDisplay(connection)

def run_case(binary, backend, compositor, triggers, windows, keep, case_timeout, stale_owner):
    work = Path(tempfile.mkdtemp(prefix=f"jwm-restart-{backend}-"))
    work.chmod(0o700)
    children = FIXTURE.Children()
    report = {"backend": backend, "compositor": compositor, "directory": str(work),
              "status": "failed", "steps": [], "stale_published_owner": stale_owner}
    wm_pid = None
    try:
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("JWM_", "OPENAI_", "PULSE_", "PIPEWIRE_"))
               and key not in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                               "DBUS_SYSTEM_BUS_ADDRESS", "NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC")}
        for key, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
            (work / leaf).mkdir(mode=0o700)
            env[key] = str(work / leaf)
        env.update(LIBGL_ALWAYS_SOFTWARE="1", JWM_COMPOSITOR_API="glx", RUST_LOG="info",
                   DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"), JWM_DEBUG_KEYS="1")
        (work / "config/jwm").mkdir()

        def run(*argv, timeout=10):
            assert env["XDG_RUNTIME_DIR"] == str(work / "runtime")
            return subprocess.check_output(list(map(str, argv)), env=env, stderr=subprocess.STDOUT,
                                           text=True, timeout=timeout)

        run(binary, "--backend", backend, "--gen-config", timeout=60)
        wallpaper = work / "wallpaper.png"
        png(wallpaper)
        for config in (work / "config/jwm").glob("config_*.toml"):
            text = config.read_text()
            for section, key, literal, required in (
                    ("status_bar", "show_bar", "false", True),
                    ("status_bar", "name", json.dumps("/bin/true"), True),
                    ("desktop_shell", "enabled", "false", False),
                    ("behavior", "compositor", str(compositor).lower(), True),
                    ("behavior", "wallpaper", json.dumps(str(wallpaper)), True),
                    ("behavior", "wallpaper_effect", '"off"', False)):
                text = FIXTURE.edit_setting(text, section, key, literal, required)
            config.write_text(text)
        run(binary, "--backend", backend, "--check-config", timeout=30)
        env["DISPLAY"] = children.display("Xvfb", env, work)
        host_env = env.copy()
        env["DISPLAY"] = children.display("Xephyr", env, work)
        # Xephyr only delivers its nested keyboard while its own host window
        # has focus. The host here is our hidden Xvfb, never the user display.
        host_window = subprocess.check_output(["xdotool", "search", "--class", "Xephyr"],
                                              env=host_env, text=True, timeout=5).splitlines()[0]
        subprocess.run(["xdotool", "windowfocus", "--sync", host_window], env=host_env,
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=5)
        private_display = env["DISPLAY"]
        pid_file = work / "wm.pid"
        wrapper = "import os,sys;open(sys.argv[1],'x').write(str(os.getpid()));os.execv(sys.argv[2],sys.argv[2:])"
        wm = children.spawn(["dbus-run-session", "--", sys.executable, "-c", wrapper,
                             str(pid_file), str(binary), "--backend", backend], env, work / "jwm.log")
        socket = work / "runtime/jwm-ipc.sock"

        def alive():
            if wm.poll() is not None or (wm_pid is not None and identity(wm_pid) is None):
                raise RuntimeError(f"JWM exited during restart; supervisor status={wm.poll()}; see jwm.log")

        def wait(predicate, label, timeout=case_timeout):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                alive()
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                    last = str(error)
                time.sleep(0.05)
            raise RuntimeError(f"{label} timed out: {last}")

        def context():
            return FIXTURE.query(socket, "get_execution_context")

        def wallpaper_ready(min_uploads=1):
            if compositor:
                # A running compositor paints its own texture; by design it
                # does not install a native root pixmap. Each fresh backend
                # must upload the generated PNG again after exec.
                uploads = (work / "jwm.log").read_text(errors="replace").count(
                    "compositor: uploaded wallpaper texture (32x24)")
                return {"kind": "compositor_texture", "uploads": uploads, "width": 32, "height": 24} if uploads >= min_uploads else None
            raw = run("xprop", "-root", "_XROOTPMAP_ID", "ESETROOT_PMAP_ID")
            ids = re.findall(r"(?:_XROOTPMAP_ID|ESETROOT_PMAP_ID).*?(0x[0-9a-fA-F]+)", raw)
            if len(ids) != 2 or len(set(ids)) != 1:
                return None
            geometry = native_pixmap_geometry(private_display, int(ids[0], 16))
            return geometry if geometry["width"] == 1280 and geometry["height"] == 800 else None

        before = wait(context, "initial IPC")
        wm_pid = int(pid_file.read_text())
        wm_identity = identity(wm_pid)
        assert wm_identity is not None
        version = FIXTURE.query(socket, "get_version")
        assert version["backend"] == backend, version
        report.update(wm_identity=wm_identity, display=private_display,
                      initial_instance=before["wm_instance_id"], initial_wallpaper=wait(wallpaper_ready, "initial native wallpaper"))
        FIXTURE.command(socket, "view", {"value": 4})
        FIXTURE.command(socket, "setlayout", {"layout": "scrolling"})
        applications = []
        for index in range(windows):
            name = f"JwmRestartFixture{index}"
            child = children.spawn(["xmessage", "-name", name, "-title", name, "isolated restart fixture"],
                                   env, work / f"application-{index}.log")
            row = wait(lambda: next((item for item in FIXTURE.query(socket, "get_windows")
                                     if item["name"] == name), None), "managed fixture application")
            xid = int(run("xdotool", "search", "--name", "^" + re.escape(name) + "$").splitlines()[0])
            applications.append({"name": name, "identity": identity(child.pid), "xid": xid, "window": row["id"]})
        report["applications"] = applications
        # Xlib lazily initializes Xephyr's XKB map when the first client opens;
        # reinstall against that final map, as the native shortcut fixture does.
        FIXTURE.command(socket, "reload_config", {})

        for trigger in triggers:
            previous = context()
            prior_wallpaper = wallpaper_ready()
            assert prior_wallpaper is not None
            prior_layout = FIXTURE.query(socket, "get_layout")
            started = time.monotonic()
            if stale_owner == "foreign":
                publish_foreign_owner(private_display, applications[0]["xid"])
                raw = run("xprop", "-root", "_XROOTPMAP_ID", "ESETROOT_PMAP_ID")
                assert raw.count("(PIXMAP)") == 2, raw
                assert raw.lower().count(hex(applications[0]["xid"])) == 2, raw
            if trigger == "hotkey":
                assert env["DISPLAY"] == private_display
                # Inject at Xephyr's host window, exactly where physical keys
                # enter it. Both displays belong to this fixture. --window
                # would use SendEvent and bypass the native keyboard path.
                run("xdotool", "windowfocus", "--sync", applications[0]["xid"])
                subprocess.run(["xdotool", "windowfocus", "--sync", host_window], env=host_env,
                               check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=5)
                subprocess.run(["xdotool", "key", "--clearmodifiers", "--delay", "120", "Alt_L+Shift_L+r"],
                               env=host_env, check=True, stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE, timeout=5)
            else:
                try:
                    FIXTURE.command(socket, "restart", {})
                except (OSError, RuntimeError):
                    # Teardown may close IPC after accepting the request.
                    pass

            def restored():
                current = context()
                if current["wm_instance_id"] == previous["wm_instance_id"]:
                    return None
                if identity(wm_pid) != wm_identity:
                    raise RuntimeError("restart changed the JWM PID/start time")
                if current["monitors"][0]["visible_tag_mask"] != 4:
                    return None
                if FIXTURE.query(socket, "get_layout") != prior_layout:
                    return None
                rows = {item["name"]: item for item in FIXTURE.query(socket, "get_windows")}
                for application in applications:
                    if identity(application["identity"]["pid"]) != application["identity"]:
                        raise AssertionError("foreign application process did not survive restart")
                    if application["name"] not in rows:
                        return None
                    xids = [int(item) for item in run("xdotool", "search", "--name", "^" + re.escape(application["name"]) + "$").splitlines()]
                    if application["xid"] not in xids:
                        raise AssertionError("application XID did not survive restart")
                geometry = wallpaper_ready(prior_wallpaper.get("uploads", 0) + 1)
                if not geometry:
                    return None
                run("xdpyinfo")
                return {"trigger": trigger, "status": "passed", "elapsed_ms": round((time.monotonic() - started) * 1000, 2),
                        "old_instance": previous["wm_instance_id"], "new_instance": current["wm_instance_id"],
                        "wallpaper": geometry}

            report["steps"].append(wait(restored, f"{trigger} restart with native wallpaper"))
            # A response immediately before a delayed BadPixmap/connection
            # failure is insufficient. Observe this same owner after settling.
            time.sleep(0.25)
            alive()
            assert identity(wm_pid) == wm_identity
            assert context()["wm_instance_id"] == report["steps"][-1]["new_instance"]
        report["status"] = "passed"
    except BaseException as error:
        report["error"] = f"{type(error).__name__}: {error}"
        if isinstance(error, subprocess.CalledProcessError):
            report["command_output"] = str(error.output)[-6000:]
        if wm_pid is not None:
            report["final_wm_identity"] = identity(wm_pid)
        if isinstance(error, KeyboardInterrupt):
            raise
    finally:
        children.close()
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if report["status"] == "passed" and not keep:
            shutil.rmtree(work)
            report.pop("directory", None)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, default=ROOT / "target/debug/jwm")
    parser.add_argument("--backends", default="x11rb,xcb")
    parser.add_argument("--compositor", choices=("on", "off", "both"), default="both")
    parser.add_argument("--triggers", default="hotkey,ipc")
    parser.add_argument("--stale-published-owner", choices=("none", "foreign"), default="none",
                        help="publish a live fixture application's XID as stale typed PIXMAP hints before restart")
    parser.add_argument("--windows", type=int, choices=range(1, 6), default=1)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--timeout", type=float, default=90, help="per-stage deadline; cold software GL compiles shaders")
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    backends, triggers = args.backends.split(","), args.triggers.split(",")
    if not backends or any(name not in ("x11rb", "xcb") for name in backends):
        parser.error("only the two private X11 transports are supported")
    if args.timeout <= 0 or args.timeout > 300:
        parser.error("timeout must be within (0, 300] seconds")
    if not triggers or any(name not in ("hotkey", "ipc") for name in triggers):
        parser.error("triggers must be hotkey and/or ipc")
    dependencies = {name: shutil.which(name) for name in REQUIRED}
    ready = all(dependencies.values()) and args.jwm.is_file() and bool(ctypes.util.find_library("X11"))
    if args.preflight:
        print(json.dumps({"dependencies": dependencies, "binary": str(args.jwm), "ready": ready,
                          "uses_user_desktop": False, "runs_cargo": False}, indent=2))
        return 0 if ready else 1
    if not ready:
        parser.error("supply an existing JWM binary and the listed X11 test dependencies")
    # Resolve only now; preserve the caller-selected executable's bytes in evidence.
    binary = args.jwm.resolve()
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    states = (True, False) if args.compositor == "both" else (args.compositor == "on",)
    report = {"schema_version": 1, "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "uses_user_desktop": False, "runs_cargo": False,
              "runs": [run_case(binary, backend, state, triggers, args.windows, args.keep, args.timeout, args.stale_published_owner)
                       for backend in backends for state in states]}
    text = json.dumps(report, indent=2) + "\n"
    if args.report:
        args.report.write_text(text)
    print(text, end="")
    return 0 if all(run["status"] == "passed" for run in report["runs"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
