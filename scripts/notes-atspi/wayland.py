"""Real AT-SPI probe under an owned, headless Sway compositor without XWayland."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import probe


def descendants(node):
    yield node
    for child in node["nodes"] + node["floating_nodes"]:
        yield from descendants(child)


def run(notes_path, atspi, gio, glib):
    with tempfile.TemporaryDirectory(prefix="notes-atspi-wayland-") as t:
        work = Path(t)
        runtime = work / "run"
        runtime.mkdir(mode=0o700)
        env = dict(os.environ, HOME=str(work), XDG_RUNTIME_DIR=str(runtime),
                   XDG_DATA_HOME=str(work / "data"), XDG_CONFIG_HOME=str(work / "config"),
                   XDG_CACHE_HOME=str(work / "cache"), XDG_STATE_HOME=str(work / "state"),
                   GSETTINGS_BACKEND="memory", XDG_SESSION_TYPE="wayland", LC_ALL="C.UTF-8",
                   WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_LIBINPUT_NO_DEVICES="1",
                   GDK_BACKEND="wayland", JWM_ATSPI_CHILD="gtk")
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "SWAYSOCK",
                    "WINIT_UNIX_BACKEND", "AT_SPI_BUS_ADDRESS", "ATK_BRIDGE",
                    "NO_AT_BRIDGE", "GTK_MODULES", "GTK_A11Y"):
            env.pop(key, None)
        notes_dir = work / "data/jwm/notes"
        notes_dir.mkdir(parents=True)
        for name, title, timestamp in (("a", probe.INITIAL, 1700010000),
                                       ("z", probe.TARGET, 1700000000)):
            path = notes_dir / (name + ".txt")
            path.write_text(title + "\n", encoding="utf8")
            os.utime(path, (timestamp, timestamp))
        config = work / "sway.conf"
        config.write_text("xwayland disable\noutput HEADLESS-1 resolution 1280x900\n"
                          "default_border none\nfocus_follows_mouse no\n")
        children = []
        receipt = work / "gtk-action"
        bus = gio.bus_get_sync(gio.BusType.SESSION, None)
        address = ("org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties")
        status, prop = "org.a11y.Status", "ScreenReaderEnabled"

        def set_reader(enabled):
            bus.call_sync(*address, "Set",
                          glib.Variant("(ssv)", (status, prop, glib.Variant("b", enabled))),
                          None, gio.DBusCallFlags.NONE, 3000, None)
            value = bus.call_sync(*address, "Get", glib.Variant("(ss)", (status, prop)),
                                  glib.VariantType("(v)"), gio.DBusCallFlags.NONE, 3000, None)
            if value.get_child_value(0).get_variant().get_boolean() != enabled:
                raise probe.ProbeError("screen-reader-state-not-confirmed")
            print(json.dumps({"screen_reader_enabled": enabled}), flush=True)

        def launch(*argv):
            child = subprocess.Popen(list(map(str, argv)), env=env)
            children.append(child)
            return child

        def socket(pattern, sway):
            if sway.poll() is not None:
                raise probe.ProbeError("sway-exited")
            sockets = [path for path in runtime.glob(pattern) if path.is_socket()]
            if len(sockets) > 1:
                raise probe.ProbeError("ambiguous-sway-socket")
            return sockets[0] if sockets else None

        def ipc(*args):
            result = subprocess.run(["swaymsg", "-s", env["SWAYSOCK"], "-r", *args],
                                    env=env, capture_output=True, text=True, timeout=3,
                                    check=True)
            return json.loads(result.stdout)

        try:
            sway = launch("sway", "--unsupported-gpu", "-c", config)
            ipc_path = probe.wait(lambda: socket("sway-ipc.*.sock", sway), 10)
            display = probe.wait(lambda: socket("wayland-*", sway), 10)
            if ipc_path is None or display is None:
                raise probe.ProbeError("sway-sockets-not-ready")
            env.update(SWAYSOCK=str(ipc_path), WAYLAND_DISPLAY=display.name)
            # Winit's Wayland focus event is driven by wl_keyboard enter.
            # Keep a virtual keyboard on this device-less seat for the run;
            # no keys are sent after either application starts.
            keyboard = launch("wtype", "-k", "Shift_L", "-s", "75000")
            def keyboard_ready():
                if keyboard.poll() is not None:
                    raise probe.ProbeError("virtual-keyboard-exited")
                return any(node.get("type") == "keyboard"
                           for node in ipc("-t", "get_inputs"))
            if not probe.wait(keyboard_ready, 5):
                raise probe.ProbeError("virtual-keyboard-not-ready")
            set_reader(False)
            gtk = launch(sys.executable, __file__, "--gtk", receipt)
            notes = launch(notes_path)

            def owned_window():
                if notes.poll() is not None:
                    raise probe.ProbeError("notes-process-exited")
                nodes = list(descendants(ipc("-t", "get_tree")))
                matches = [n for n in nodes if n.get("pid") == notes.pid
                           and n.get("app_id") == "org.jwm.Notes"
                           and n.get("shell") == "xdg_shell"]
                if len(matches) > 1:
                    raise probe.ProbeError("ambiguous-native-notes-window")
                return matches[0] if matches else None

            initial = probe.wait(owned_window, 12)
            if initial is None:
                raise probe.ProbeError("native-notes-window-not-mapped")
            con_id = initial["id"]
            refocused = False
            observed = set()

            def window(title):
                nonlocal refocused
                current = owned_window()
                if current is None or current["id"] != con_id:
                    return False
                expected = title + " — JWM 备忘录"
                if current.get("name") != expected:
                    return False
                if not refocused and title == probe.INITIAL:
                    reply = ipc(f"[con_id={con_id}] focus")
                    if not reply or not all(item.get("success") for item in reply):
                        raise probe.ProbeError("sway-focus-request-failed")
                    refocused = True
                    return bool(probe.wait(lambda: window(title), 3))
                if current.get("focused") is not True:
                    return False
                if title not in observed:
                    observed.add(title)
                    print(json.dumps({"con_id": con_id, "pid": notes.pid, "app_id": current["app_id"],
                                      "shell": current["shell"], "focused": True, "title": expected},
                                     ensure_ascii=False), flush=True)
                return True

            return probe.evaluate(atspi, gtk.pid, notes.pid, receipt, window, 7,
                                  enable_reader=lambda: set_reader(True))
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gtk", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--notes", type=Path)
    parser.add_argument("--expect", choices=("baseline", "working"))
    args = parser.parse_args()
    if (os.getenv("JWM_ATSPI_ISOLATED") != "1" or os.getenv("GSETTINGS_BACKEND") != "memory"
            or not os.getenv("DBUS_SESSION_BUS_ADDRESS") or os.getenv("DISPLAY")
            or os.getenv("AT_SPI_BUS_ADDRESS") or os.getenv("WAYLAND_SOCKET")
            or (not args.gtk and (os.getenv("WAYLAND_DISPLAY") or os.getenv("SWAYSOCK")))):
        parser.error("requires a fresh private D-Bus session and an owned compositor")
    if args.gtk:
        if not (os.getenv("JWM_ATSPI_CHILD") == "gtk" and os.getenv("WAYLAND_DISPLAY")
                and os.getenv("GDK_BACKEND") == "wayland"):
            parser.error("invalid GTK control environment")
        probe.gtk_control(args.gtk)
        return 0
    if not args.notes or not args.expect:
        parser.error("--notes and --expect are required")
    try:
        import gi
        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi, Gio, GLib
    except (ImportError, ValueError):
        print(json.dumps({"outcome": "probe-atspi-runtime-unavailable"}))
        return 2
    try:
        if Atspi.init() != 0:
            raise probe.ProbeError("atspi-init-failed")
        Atspi.set_timeout(1500, 3000)
        outcome = run(args.notes.resolve(), Atspi, Gio, GLib)
    except (probe.ProbeError, GLib.Error, OSError, AttributeError, TypeError, ValueError,
            subprocess.SubprocessError) as error:
        result = str(error) if isinstance(error, probe.ProbeError) else "probe-transport-or-fixture-error"
        print(json.dumps({"outcome": result}))
        return 2
    print(json.dumps({"backend": "native-wayland-sway-headless", "xwayland": False,
                      "outcome": outcome, "gtk_positive_control": "action-observed",
                      "screen_reader_enabled": True, "speech": "not-tested"}))
    expected = ("notes-no-atspi-application-for-pid" if args.expect == "baseline"
                else "notes-named-controls-action-and-focus-observed")
    return 0 if outcome == expected else 1


if __name__ == "__main__":
    raise SystemExit(main())
