#!/usr/bin/env python3
"""Term dropdown exit overlap regression on owned Xvfb -> Xephyr software GL.

Only the supplied binary is exercised. No user display/config, terminal app,
password, screen locker or project command is used. Raw sampled PNGs and JSON
are retained on failure. This verifies compositor pixels, not physical GPU output.
"""
import argparse
import ctypes as C
import ctypes.util
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("private_fixture", ROOT / "eva/tests/desktop-ipc.py")
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)
REQUIRED = ("Xvfb", "Xephyr", "dbus-run-session", "xdotool")


class XImage(C.Structure):
    _fields_ = [("width", C.c_int), ("height", C.c_int), ("xoffset", C.c_int),
                ("format", C.c_int), ("data", C.c_void_p), ("byte_order", C.c_int),
                ("bitmap_unit", C.c_int), ("bitmap_bit_order", C.c_int), ("bitmap_pad", C.c_int),
                ("depth", C.c_int), ("bytes_per_line", C.c_int), ("bits_per_pixel", C.c_int),
                ("red_mask", C.c_ulong), ("green_mask", C.c_ulong), ("blue_mask", C.c_ulong),
                ("obdata", C.c_void_p), ("functions", C.c_void_p * 6)]


def ximage_format(metadata):
    """Accept explicit byte-aligned truecolor formats; reject unknown layouts."""
    width, height = metadata["width"], metadata["height"]
    bpp, stride = metadata["bits_per_pixel"], metadata["bytes_per_line"]
    order, pad = metadata["byte_order"], metadata["bitmap_pad"]
    if not (0 < width <= 8192 and 0 < height <= 8192):
        raise ValueError("invalid XImage dimensions")
    if metadata["format"] != 2 or metadata["xoffset"] != 0:
        raise ValueError("XImage must be unshifted ZPixmap")
    if bpp not in (24, 32) or metadata["depth"] not in (24, 32) or metadata["depth"] > bpp:
        raise ValueError("unsupported XImage depth/bits_per_pixel")
    if order not in (0, 1) or metadata["bitmap_bit_order"] not in (0, 1):
        raise ValueError("unknown XImage byte/bitmap order")
    if pad not in (8, 16, 32) or metadata["bitmap_unit"] not in (8, 16, 32):
        raise ValueError("unsupported XImage bitmap unit/pad")
    minimum = width * (bpp // 8)
    expected_stride = (minimum + pad // 8 - 1) // (pad // 8) * (pad // 8)
    if stride != expected_stride or stride * height > 64 * 1024 * 1024:
        raise ValueError("invalid XImage row stride/buffer size")
    layout = ["X"] * (bpp // 8)
    for channel, mask in (("R", metadata["red_mask"]), ("G", metadata["green_mask"]), ("B", metadata["blue_mask"])):
        shifts = [shift for shift in range(0, bpp, 8) if mask == 0xff << shift]
        if len(shifts) != 1:
            raise ValueError("unsupported XImage channel mask")
        position = shifts[0] // 8 if order == 0 else bpp // 8 - 1 - shifts[0] // 8
        if layout[position] != "X":
            raise ValueError("overlapping XImage channel masks")
        layout[position] = channel
    rawmode = "".join(layout)
    if rawmode not in ("RGB", "BGR", "RGBX", "BGRX", "XRGB", "XBGR"):
        raise ValueError("unsupported Pillow raw XImage channel ordering: " + rawmode)
    return rawmode, stride * height


def decode_ximage(data, metadata):
    from PIL import Image
    rawmode, size = ximage_format(metadata)
    if len(data) != size:
        raise ValueError("XImage buffer length does not match stride * height")
    return Image.frombytes("RGB", (metadata["width"], metadata["height"]), data,
                           "raw", rawmode, metadata["bytes_per_line"], 1)


class RootCapture:
    """One persistent X connection, XGetImage and Pillow's C raw decoder."""
    def __init__(self, display_name):
        self.lib = C.CDLL(C.util.find_library("X11"))
        ptr, win = C.c_void_p, C.c_ulong
        signatures = {
            "XOpenDisplay": ([C.c_char_p], ptr),
            "XDefaultRootWindow": ([ptr], win),
            "XGetGeometry": ([ptr, win, C.POINTER(win), C.POINTER(C.c_int), C.POINTER(C.c_int),
                              C.POINTER(C.c_uint), C.POINTER(C.c_uint), C.POINTER(C.c_uint), C.POINTER(C.c_uint)], C.c_int),
            "XGetImage": ([ptr, win, C.c_int, C.c_int, C.c_uint, C.c_uint, win, C.c_int], C.POINTER(XImage)),
            "XDestroyImage": ([C.POINTER(XImage)], C.c_int),
            "XCloseDisplay": ([ptr], C.c_int),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.lib, name)
            function.argtypes, function.restype = args, result
        self.display = self.lib.XOpenDisplay(display_name.encode())
        if not self.display:
            raise RuntimeError("cannot open owned X display for root capture")
        self.root = self.lib.XDefaultRootWindow(self.display)
        self.metadata = None

    def grab(self):
        root, x, y = C.c_ulong(), C.c_int(), C.c_int()
        width, height, border, depth = (C.c_uint() for _ in range(4))
        if not self.lib.XGetGeometry(self.display, self.root, C.byref(root), C.byref(x), C.byref(y),
                                    C.byref(width), C.byref(height), C.byref(border), C.byref(depth)):
            raise RuntimeError("cannot query owned root geometry")
        image = self.lib.XGetImage(self.display, self.root, 0, 0, width.value, height.value, C.c_ulong(-1), 2)
        if not image:
            raise RuntimeError("XGetImage failed for owned root")
        try:
            metadata = {name: getattr(image.contents, name) for name, _type in XImage._fields_
                        if name not in ("data", "obdata", "functions")}
            _, size = ximage_format(metadata)
            if (metadata["width"], metadata["height"]) != (width.value, height.value) or not image.contents.data:
                raise ValueError("XImage geometry/data mismatch")
            result = decode_ximage(C.string_at(image.contents.data, size), metadata)
            self.metadata = metadata
            return result
        finally:
            self.lib.XDestroyImage(image)

    def close(self):
        if self.display:
            self.lib.XCloseDisplay(self.display)
            self.display = None


def producer(kind, record, alive):
    """Own the actual mapped X window and publish this exact process's PID."""
    lib = C.CDLL(C.util.find_library("X11"))
    ptr, win = C.c_void_p, C.c_ulong
    signatures = {
        "XOpenDisplay": ([C.c_char_p], ptr),
        "XDefaultRootWindow": ([ptr], win),
        "XCreateSimpleWindow": ([ptr, win, C.c_int, C.c_int, C.c_uint, C.c_uint, C.c_uint, win, win], win),
        "XInternAtom": ([ptr, C.c_char_p, C.c_int], win),
        "XChangeProperty": ([ptr, win, win, win, C.c_int, C.c_int, ptr, C.c_int], C.c_int),
        "XStoreName": ([ptr, win, C.c_char_p], C.c_int),
        "XMapWindow": ([ptr, win], C.c_int),
        "XClearWindow": ([ptr, win], C.c_int),
        "XSync": ([ptr, C.c_int], C.c_int),
        "XCloseDisplay": ([ptr], C.c_int),
    }
    for name, (args, result) in signatures.items():
        function = getattr(lib, name)
        function.argtypes, function.restype = args, result
    display = lib.XOpenDisplay(os.environ["DISPLAY"].encode())
    if not display:
        raise RuntimeError("producer cannot open owned display")
    color = 0xE02020 if kind == "term" else 0x20D040 if kind == "or" else 0x2040E0
    window = lib.XCreateSimpleWindow(display, lib.XDefaultRootWindow(display), 0, 0, 1280, 800, 0, color, color)
    atom = lambda value: lib.XInternAtom(display, value.encode(), 0)
    def prop(name, type_name, values):
        data = (C.c_ulong * len(values))(*values)
        lib.XChangeProperty(display, window, atom(name), atom(type_name), 32, 0, C.cast(data, ptr), len(values))
    prop("_NET_WM_PID", "CARDINAL", [os.getpid()])
    if kind in ("above", "fullscreen"):
        prop("_NET_WM_STATE", "ATOM", [atom("_NET_WM_STATE_" + kind.upper())])
    if kind == "or":
        class Attributes(C.Structure):
            _fields_ = [("background_pixmap", win), ("background_pixel", win), ("border_pixmap", win),
                        ("border_pixel", win), ("bit_gravity", C.c_int), ("win_gravity", C.c_int),
                        ("backing_store", C.c_int), ("backing_planes", win), ("backing_pixel", win),
                        ("save_under", C.c_int), ("event_mask", C.c_long), ("do_not_propagate_mask", C.c_long),
                        ("override_redirect", C.c_int), ("colormap", win), ("cursor", win)]
        attributes = Attributes()
        attributes.override_redirect = 1
        lib.XChangeWindowAttributes.argtypes = [ptr, win, win, C.POINTER(Attributes)]
        lib.XChangeWindowAttributes(display, window, 1 << 9, C.byref(attributes))
    lib.XStoreName(display, window, ("JwmExitFixture-" + kind).encode())
    lib.XMapWindow(display, window)
    lib.XSync(display, 0)
    Path(record).write_text(json.dumps({"pid": os.getpid(), "xid": window}))
    try:
        while Path(alive).exists():
            lib.XClearWindow(display, window)
            lib.XSync(display, 0)
            time.sleep(0.03)
    finally:
        lib.XCloseDisplay(display)


def color_pixels(image, kind, rect=None):
    from PIL import ImageChops
    image = image.crop(rect) if rect else image
    red, green, blue = image.convert("RGB").split()
    if kind == "term":
        channels = ((red, lambda v: v > 140), (green, lambda v: v < 100), (blue, lambda v: v < 110))
    elif kind == "or":
        channels = ((green, lambda v: v > 140), (red, lambda v: v < 100), (blue, lambda v: v < 120))
    else:
        channels = ((blue, lambda v: v > 140), (red, lambda v: v < 100), (green, lambda v: v < 120))
    masks = [channel.point([255 if predicate(value) else 0 for value in range(256)]) for channel, predicate in channels]
    return ImageChops.multiply(ImageChops.multiply(masks[0], masks[1]), masks[2]).histogram()[255]


def overlap_during_capture(terminal, background, start, end, dispatch_seconds=0.):
    """Conservative interior shared throughout a sampled ease-in-out exit.

    Hide onset lies anywhere in the measured key-command dispatch interval.
    Capture spans additionally allow an 80ms presentation margin. Frames that
    naturally leave a small PiP ROI are not mistaken for occlusion.
    """
    def y_at(seconds):
        t = min(1., max(0., seconds))
        eased = 4 * t * t * t if t < .5 else 1 - (-2 * t + 2) ** 3 / 2
        hidden = -terminal["h"] - 2 * terminal["border_w"] - 1
        return terminal["y"] + (hidden - terminal["y"]) * eased
    rect = (max(0, background["x"], terminal["x"]) + 12,
            max(0, background["y"], int(y_at(max(0, start - dispatch_seconds - .08)))) + 12,
            min(1280, background["x"] + background["w"], terminal["x"] + terminal["w"]) - 12,
            min(800, background["y"] + background["h"], int(y_at(end + .08)) + terminal["h"]) - 12)
    return rect if rect[2] > rect[0] and rect[3] > rect[1] else None


def overlap_horizon(terminal, background, dispatch_seconds=0.):
    """Last conservatively visible ROI with enough interior to classify color."""
    horizon = 0.
    for index in range(501):
        seconds = index / 500
        rect = overlap_during_capture(terminal, background, seconds, seconds, dispatch_seconds)
        if rect and (rect[2] - rect[0]) * (rect[3] - rect[1]) >= 400:
            horizon = seconds
    return horizon


def assert_full_coverage(samples, horizon, label):
    checked = [sample for sample in samples if sample.get("checked")]
    assert horizon > .15, f"{label}: overlap lasts too briefly for meaningful coverage"
    assert len(checked) >= 3, f"{label}: fewer than 3 overlapping frames; sampling inadequate"
    times = [sample["seconds"] for sample in checked]
    assert min(times) <= horizon / 3, f"{label}: missing early overlap evidence"
    assert any(horizon / 3 < value < horizon * 2 / 3 for value in times), f"{label}: missing middle overlap evidence"
    assert max(times) >= horizon - .075, f"{label}: missing final visible overlap evidence"
    assert samples[-1]["seconds"] >= 1.05, f"{label}: capture ended before animation completion"


def producer_row(windows, producer):
    """Resolve one owned producer by PID; native XID is not JWM's WindowId."""
    matches = [item for item in windows if item.get("pid") == producer["pid"]]
    assert len(matches) <= 1, "fixture producer unexpectedly owns multiple managed windows"
    if not matches:
        return None
    item = matches[0]
    if "id" in producer:
        assert item["id"] == producer["id"], "producer changed its managed window identity"
    return item


def run_case(binary, backend, output, stacked=False):
    output.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="jwm-terminal-exit-"))
    children = FIXTURE.Children()
    capture = None
    pending_images = {}
    def flush_images():
        for label, image in list(pending_images.items()):
            image.save(output / (label + ".png"))
            del pending_images[label]
    marker = work / "producer-alive"
    marker.touch()
    with binary.open("rb") as executable:
        binary_sha = hashlib.file_digest(executable, "sha256").hexdigest()
    report = {"status": "failed", "backend": backend, "work": str(work), "samples": [],
              "binary": str(binary), "binary_sha256": binary_sha,
              "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "limits": ["private software-GL X11 only", "OR included in fixture; consult executed case records; password/session-lock not exercised"]}
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
                   DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"))
        (work / "config/jwm").mkdir()
        def run(*args, run_env=None):
            return subprocess.check_output(list(map(str, args)), env=run_env or env, stderr=subprocess.PIPE, timeout=15)
        run(binary, "--backend", backend, "--gen-config")
        for config in (work / "config/jwm").glob("config_*.toml"):
            text = config.read_text()
            for section, key, literal, required in (
                ("status_bar", "show_bar", "false", True), ("status_bar", "name", '"/bin/true"', True),
                ("desktop_shell", "enabled", "false", False), ("behavior", "compositor", "true", True),
                ("behavior", "wallpaper_effect", '"off"', False),
                ("animation", "enabled", "true", True), ("animation", "duration_ms", "1000", True),
                ("animation", "speed", '"normal"', True), ("animation", "easing", '"ease-out"', True)):
                text = FIXTURE.edit_setting(text, section, key, literal, required)
            config.write_text(text)
            generated = tomllib.loads(text)
            report.setdefault("fixture_config", {})[config.name] = {
                "animation": generated.get("animation"),
                "compositor": generated.get("behavior", {}).get("compositor"),
                "status_bar_enabled": generated.get("status_bar", {}).get("show_bar"),
            }
        run(binary, "--backend", backend, "--check-config")
        env["DISPLAY"] = children.display("Xvfb", env, work)
        host_env = env.copy()
        if stacked:
            connectors = [line.split()[0] for line in run("xrandr", "--query").decode().splitlines()
                          if len(line.split()) >= 2 and line.split()[1] == "connected"]
            assert len(connectors) == 1, connectors
            run("xrandr", "--setmonitor", "JWM-upper", "1280/340x400/106+0+0", connectors[0])
            run("xrandr", "--setmonitor", "JWM-lower", "1280/340x400/106+0+400", "none")
            report["randr_monitors"] = run("xrandr", "--listmonitors").decode()
        else:
            env["DISPLAY"] = children.display("Xephyr", env, work)
            host_window = run("xdotool", "search", "--class", "Xephyr", run_env=host_env).splitlines()[0]
            run("xdotool", "windowfocus", "--sync", host_window.decode(), run_env=host_env)
        wm = children.spawn(["dbus-run-session", "--", str(binary), "--backend", backend], env, work / "jwm.log")
        socket = work / "runtime/jwm-ipc.sock"
        def wait(predicate, label, timeout=8):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                if wm.poll() is not None:
                    raise RuntimeError("JWM exited; see " + str(work / "jwm.log"))
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
                    last = str(error)
                time.sleep(0.02)
            raise RuntimeError(f"{label}: timed out ({last})")
        query = lambda name: FIXTURE.query(socket, name)
        command = lambda name, args={}: FIXTURE.command(socket, name, args)
        wait(lambda: socket.exists() and query("get_execution_context"), "private compositor IPC")
        def row(producer):
            item = producer_row(query("get_windows"), producer)
            if item:
                required = {"id", "pid", "x", "y", "w", "h", "border_w", "tags", "is_focused",
                            "is_floating", "is_above", "is_pip", "is_fullscreen", "is_scratchpad"}
                assert required <= item.keys(), f"get_windows schema missing {required - item.keys()}"
            return item
        capture = RootCapture(env["DISPLAY"])
        def shot(label, defer=False):
            image = capture.grab()
            report["capture_format"] = capture.metadata
            report["capture_backend"] = "persistent Xlib XGetImage / Pillow raw decoder"
            if defer:
                pending_images[label] = image
            else:
                image.save(output / (label + ".png"))
            return image
        def hotkey():
            start = time.monotonic()
            run("xdotool", "key", "--clearmodifiers", "alt+shift+n", run_env=host_env)
            return start, time.monotonic()
        if stacked:
            monitors = query("get_monitors")
            report["monitors"] = monitors
            assert sorted((m["x"], m["y"], m["w"], m["h"]) for m in monitors) == [
                (0, 0, 1280, 400), (0, 400, 1280, 400)
            ], monitors
            assert query("get_status")["compositor_active"], "software GL compositor not active"
            run("xdotool", "mousemove", "640", "560")
            if not any(m["y"] == 400 and m["focused"] for m in query("get_monitors")):
                command("focusmon", {"value": 1})
            wait(lambda: any(m["y"] == 400 and m["focused"] for m in query("get_monitors")),
                 "selected lower output")
        term_record = work / "term.json"
        command("togglescratchpad", {"cmd": ["term", sys.executable, str(Path(__file__).resolve()),
                                             "--producer", "term", "--record", str(term_record), "--alive", str(marker)]})
        term = wait(lambda: json.loads(term_record.read_text()) if term_record.exists() else None, "spawned terminal PID")
        report["producer"] = term
        term["id"] = wait(lambda: row(term), "mapped terminal")["id"]
        assert row(term)["pid"] == term["pid"], "scratchpad did not match actual producer PID"
        assert row(term)["is_scratchpad"], "producer not registered as scratchpad"
        assert row(term).get("scratchpad") == "term", "wrong named scratchpad binding"
        assert query("get_scratchpads").get("term") == term["id"], "scratchpad registry identity mismatch"
        command("reload_config")  # Reinstall hotkeys after Xephyr XKB initialization.
        time.sleep(1.2)
        if stacked:
            ready = row(term)
            report["lower_terminal_before"] = ready
            assert ready["is_focused"] and ready["tags"] != 0 and ready["y"] >= 400, ready
            before = shot("stacked-before-key")
            upper, lower = (0, 0, 1280, 400), (0, 400, 1280, 800)
            assert color_pixels(before, "term", upper) == 0
            assert color_pixels(before, "term", lower) > 5000
            # The existing single-output cases exercise the physical hotkey
            # under Xephyr. The direct Xvfb key path did not show a dismissal in
            # this topology; drive the same production toggle via private IPC
            # to isolate the compositor behavior instead of input transport.
            started = time.monotonic()
            command("togglescratchpad", {"cmd": ["term"]})
            finished = time.monotonic()
            after_dispatch = row(term)
            report["stacked_terminal_after_ipc"] = after_dispatch
            samples = []
            report["samples"].append({"case": "stacked", "frames": samples,
                                      "trigger": "private-ipc", "toggle_monotonic_start": started,
                                      "toggle_monotonic_finish": finished})
            while time.monotonic() - started < 1.25 and len(samples) < 60:
                image = shot(f"stacked-hide-{len(samples)}", defer=True)
                samples.append({"seconds": time.monotonic() - started,
                                "upper": color_pixels(image, "term", upper),
                                "lower": color_pixels(image, "term", lower)})
                time.sleep(0.020)
            assert after_dispatch and not after_dispatch["is_focused"] and after_dispatch["tags"] == 0, (
                "terminal did not release managed focus and tags on dismissal"
            )
            assert samples and samples[-1]["seconds"] > 1.05, "capture missed the exit deadline"
            assert any(0.10 <= f["seconds"] <= 0.65 and f["lower"] > 1000 for f in samples), (
                "did not observe animation on the owning output"
            )
            final = shot("stacked-after-animation")
            assert color_pixels(final, "term", upper) == color_pixels(final, "term", lower) == 0
            assert all(frame["upper"] == 0 for frame in samples), "exit crossed onto the upper output"
            assert len({frame["lower"] for frame in samples}) > 2, "exit did not move on its own output"

            entered = time.monotonic()
            command("togglescratchpad", {"cmd": ["term"]})
            reveal = []
            report["samples"].append({"case": "stacked-reveal", "trigger": "private-ipc",
                                      "toggle_monotonic_start": entered, "frames": reveal})
            while time.monotonic() - entered < 1.25 and len(reveal) < 60:
                image = shot(f"stacked-reveal-{len(reveal)}", defer=True)
                reveal.append({"seconds": time.monotonic() - entered,
                               "upper": color_pixels(image, "term", upper),
                               "lower": color_pixels(image, "term", lower)})
                time.sleep(0.020)
            assert reveal and reveal[-1]["seconds"] > 1.05, "capture missed reveal completion"
            assert all(frame["upper"] == 0 for frame in reveal), "reveal crossed onto the upper output"
            assert len({frame["lower"] for frame in reveal}) > 2, "reveal did not move on its own output"
            restored = row(term)
            assert restored["pid"] == term["pid"] and restored["is_focused"] and restored["tags"] != 0
            restored_image = shot("stacked-after-reveal")
            assert color_pixels(restored_image, "term", upper) == 0
            assert color_pixels(restored_image, "term", lower) > 5000
            report["stacked_outcome"] = "clipped-exit-and-reveal"
            report["status"] = "passed"
            return report
        for kind in ("above", "pip", "fullscreen"):
            record = work / (kind + ".json")
            background = children.spawn([sys.executable, str(Path(__file__).resolve()), "--producer", kind,
                                         "--record", str(record), "--alive", str(marker)], env, work / (kind + ".log"))
            data = wait(lambda: json.loads(record.read_text()) if record.exists() else None, "background producer")
            data["id"] = wait(lambda: row(data), "managed background")["id"]
            command("focus_window", {"id": data["id"]})
            if kind == "pip":
                command("togglepip")
            elif kind == "above" and not row(data)["is_floating"]:
                command("togglefloating")
            if kind != "fullscreen":
                run("xdotool", "windowsize", data["xid"], 1280, 800)
                run("xdotool", "windowmove", data["xid"], 0, 0)
            wait(lambda: row(data)["is_" + kind], "requested " + kind + " layer")
            # Hide if still shown, then reveal over the prepared overlap background.
            if row(term)["tags"] != 0:
                hotkey(); time.sleep(1.2)
            hotkey()
            wait(lambda: row(term)["is_focused"], "terminal reveal focus")
            time.sleep(1.2)
            bg = row(data)
            terminal = row(term)
            rect = (max(0, bg["x"], terminal["x"]), max(0, bg["y"], terminal["y"]),
                    min(1280, bg["x"] + bg["w"], terminal["x"] + terminal["w"]),
                    min(800, bg["y"] + bg["h"], terminal["y"] + terminal["h"]))
            assert rect[2] - rect[0] > 80 and rect[3] - rect[1] > 80, f"{kind}: fixture lacks overlap: {rect}"
            before_start = time.monotonic()
            shot(kind + "-before-key")
            before_end = time.monotonic()
            started, key_finished = hotkey()
            dispatch_seconds = key_finished - started
            samples = []
            case_report = {"case": kind, "rect": rect, "frames": samples,
                           "key_monotonic_start": started, "key_monotonic_finish": key_finished,
                           "dispatch_seconds": dispatch_seconds,
                           "pre_key_capture_relative": [before_start - started, before_end - started]}
            report["samples"].append(case_report)
            wait(lambda: not row(term)["is_focused"], "focus released at hide start", timeout=0.2)
            focus_released = time.monotonic()
            case_report["focus_released_seconds"] = focus_released - started
            assert focus_released - started <= .2, "focus-release observation exceeded 200ms from key-command start"
            assert int(run("xdotool", "getwindowfocus")) != term["xid"], "native focus remained on hiding terminal"
            onset_latest = max(key_finished, focus_released) - started
            case_report["onset_latest_seconds"] = onset_latest
            horizon = overlap_horizon(terminal, bg, onset_latest)
            report["samples"][-1]["conservative_overlap_horizon_seconds"] = horizon
            index = 0
            # Sample the whole 1000ms exit, including natural disappearance.
            # The frame cap and elapsed budget prevent an unbounded recorder.
            while time.monotonic() - started < 1.15 and index < 60:
                capture_start = time.monotonic() - started
                image = shot(f"{kind}-hide-{index}", defer=True)
                elapsed = time.monotonic() - started
                visible = overlap_during_capture(terminal, bg, capture_start, elapsed, onset_latest)
                sample = {"start_seconds": capture_start, "seconds": elapsed, "conservative_overlap": visible,
                          "checked": False, "total_term_pixels": color_pixels(image, "term")}
                samples.append(sample)
                if visible:
                    area = (visible[2] - visible[0]) * (visible[3] - visible[1])
                    if area >= 400:
                        pixels = color_pixels(image, "term", visible)
                        sample.update(term_pixels_in_overlap=pixels, checked=True)
                        assert pixels > area * .9, f"{kind}: hiding terminal covered at {elapsed:.3f}s ({pixels}/{area})"
                index += 1
                time.sleep(0.025)
            assert_full_coverage(samples, horizon, kind)
            flush_images()
            time.sleep(max(0, 1.25 - (time.monotonic() - started)))
            end = shot(kind + "-complete")
            assert color_pixels(end, "term", rect) == 0, "terminal remained after hide completion"
            assert color_pixels(end, "background", rect) > 1000, "background not revealed after hide"
            # Reverse while still exiting; identity must be retained and input restored.
            hotkey(); time.sleep(1.2)
            hotkey(); time.sleep(0.12); hotkey()
            wait(lambda: row(term)["is_focused"], "reversal restores focus")
            time.sleep(1.2)
            assert row(term)["pid"] == term["pid"], "reversal respawned producer"
            assert row(term)["tags"] != 0, "reversal left terminal parked"
            shot(kind + "-reversed")  # Resting term returns to normal layer policy.
            background.terminate(); background.wait(timeout=3)
        # Override-redirect safety surfaces must stay above the transient term layer.
        record = work / "or.json"
        overlay = children.spawn([sys.executable, str(Path(__file__).resolve()), "--producer", "or",
                                  "--record", str(record), "--alive", str(marker)], env, work / "or.log")
        wait(record.exists, "OR surface")
        wait(lambda: color_pixels(shot("or-ready"), "or") > 900000, "OR covers private display")
        shot("or-before-key")
        started, key_finished = hotkey()
        index = 0
        or_samples = []
        report["samples"].append({"case": "or", "frames": or_samples,
                                  "key_monotonic_start": started, "key_monotonic_finish": key_finished})
        while time.monotonic() - started < 1.15 and index < 60:
            image = shot(f"or-hide-{index}", defer=True)
            red, green = color_pixels(image, "term"), color_pixels(image, "or")
            or_samples.append({"seconds": time.monotonic() - started, "term_pixels": red, "or_pixels": green})
            assert red == 0, "hiding term escaped above OR safety surface"
            assert green > 900000
            index += 1
            time.sleep(0.025)
        assert len(or_samples) >= 3 and or_samples[-1]["seconds"] >= 1.05, "OR capture missed hide completion"
        flush_images()
        overlay.terminate(); overlay.wait(timeout=3)
        report["status"] = "passed"
    finally:
        if capture:
            capture.close()
        marker.unlink(missing_ok=True)
        time.sleep(0.08)
        children.close()
        report_json = json.dumps(report, indent=2)
        (output / "report.json").write_text(report_json + "\n")
        print(report_json, flush=True)  # Failed CI keeps timing/pixel diagnostics in its job log.
        flush_images()
        for log in work.glob("*.log"):
            shutil.copyfile(log, output / log.name)
        for config in (work / "config/jwm").glob("config_*.toml"):
            shutil.copyfile(config, output / config.name)
        if report["status"] == "passed":
            shutil.rmtree(work)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path)
    parser.add_argument("--backend", choices=("x11rb", "xcb"), default="x11rb")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--stacked", action="store_true")
    parser.add_argument("--producer", choices=("term", "above", "pip", "fullscreen", "or"), help=argparse.SUPPRESS)
    parser.add_argument("--record", help=argparse.SUPPRESS)
    parser.add_argument("--alive", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.producer:
        producer(args.producer, args.record, args.alive)
        return
    required = ("Xvfb", "xrandr", "dbus-run-session", "xdotool") if args.stacked else REQUIRED
    missing = [tool for tool in required if not shutil.which(tool)]
    if not C.util.find_library("X11"):
        missing.append("libX11")
    try:
        import PIL.Image  # noqa: F401
    except ImportError:
        missing.append("Pillow")
    if missing:
        parser.error("required tools unavailable: " + ", ".join(missing))
    if not args.jwm or not args.jwm.is_file() or not args.output:
        parser.error("--jwm existing binary and --output are required")
    if args.preflight:
        print("terminal exit fixture dependencies: PASS")
        return
    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    run_case(args.jwm.resolve(), args.backend, args.output.resolve(), args.stacked)


if __name__ == "__main__":
    main()
