"""Read-only AT-SPI probe for Notes, with generated test notes only."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

CONTROL = "JWM A11y positive control"
INITIAL = "JWM A11y A initial"
TARGET = "JWM A11y Z target"
SEARCH = "搜索备忘录"
EDITOR = "备忘录正文"
ACTION_NAMES = {"click", "activate", "press", "open", "do default"}


class ProbeError(Exception):
    pass


def wait(get, seconds):
    end = time.monotonic() + seconds
    while True:
        value = get()
        if value is not None and value is not False:
            return value
        if time.monotonic() >= end:
            return None
        time.sleep(.1)


def children(node):
    count = node.get_child_count()
    if not 0 <= count <= 256:
        raise ProbeError("invalid-node-count")
    for index in range(count):
        child = node.get_child_at_index(index)
        if child is not None:
            yield child


def named(root, label, roles):
    todo = [] if root is None else [root]
    match = None
    seen = 0
    while todo:
        node = todo.pop()
        seen += 1
        if seen > 256:
            raise ProbeError("tree-too-large")
        if node.get_role() in roles and node.get_name() == label:
            if match is not None:
                raise ProbeError("ambiguous-control")
            match = node
        todo.extend(children(node))
    return match


def activate(node):
    api = node.get_action_iface()
    if api is None:
        return False
    count = api.get_n_actions()
    if not 0 <= count <= 16:
        raise ProbeError("invalid-action-count")
    indexes = [i for i in range(count)
               if api.get_action_name(i).casefold() in ACTION_NAMES]
    return len(indexes) == 1 and api.do_action(indexes[0])


def evaluate(atspi, gtk_pid, notes_pid, receipt, window, seconds, enable_reader=None,
             keyboard=None):
    def application(pid):
        desktop = atspi.get_desktop(0)
        if desktop is None:
            raise ProbeError("desktop-unavailable")
        desktop.clear_cache()
        found = [a for a in children(desktop) if a.get_process_id() == pid]
        if len(found) > 1:
            raise ProbeError("ambiguous-application")
        if found:
            found[0].clear_cache()
            return found[0]
        return None

    roles = atspi.Role
    if receipt.exists():
        raise ProbeError("stale-positive-control-receipt")
    gtk = wait(lambda: named(application(gtk_pid), CONTROL, {roles.PUSH_BUTTON}), seconds)
    if gtk is None or not activate(gtk):
        raise ProbeError("gtk-positive-control-not-actionable")
    if not wait(lambda: receipt.exists() and receipt.read_text() == "activated\n", seconds):
        raise ProbeError("gtk-positive-control-action-not-observed")
    if enable_reader is not None:
        enable_reader()
    if not window(INITIAL):
        raise ProbeError("initial-notes-window-not-ready")

    def current():
        if not window(INITIAL):
            raise ProbeError("initial-notes-window-lost")
        return application(notes_pid)

    if wait(current, seconds) is None:
        return "notes-no-atspi-application-for-pid"

    if keyboard is not None:
        return keyboard(atspi, lambda: application(notes_pid), window)

    def controls():
        root = current()
        if root is None:
            raise ProbeError("atspi-application-lost")
        search = named(root, SEARCH, {roles.ENTRY, roles.TEXT})
        row = named(root, TARGET, {roles.PUSH_BUTTON, roles.LIST_ITEM})
        return row if search is not None else None

    row = wait(controls, seconds)
    if row is None:
        return "notes-atspi-controls-missing"
    if not activate(row):
        return "notes-result-not-actionable"

    def focused():
        if not window(TARGET):
            return False
        editor = named(application(notes_pid), EDITOR,
                       {roles.TEXT, roles.ENTRY, roles.DOCUMENT_TEXT})
        if editor is None:
            return False
        editor.clear_cache()
        return editor.get_state_set().contains(atspi.StateType.FOCUSED)

    if not wait(focused, seconds):
        return "notes-result-selection-or-focus-not-observed"
    return "notes-named-controls-action-and-focus-observed"


def gtk_control(receipt):
    import gi
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gtk
    widget = Gtk.Window(title=CONTROL)
    button = Gtk.Button(label=CONTROL)
    button.connect("clicked", lambda _: receipt.write_text("activated\n"))
    widget.connect("destroy", Gtk.main_quit)
    widget.add(button)
    widget.show_all()
    Gtk.main()


def xdo(*args, env, timeout=3):
    return subprocess.run(["xdotool", *args], capture_output=True, text=True,
                          timeout=timeout, env=env)


def keyboard_results(atspi, current, window, send):
    """Drive real keys. Use AT-SPI only as an observation channel."""
    roles = atspi.Role
    fields = {roles.ENTRY, roles.TEXT, roles.DOCUMENT_TEXT}
    buttons = {roles.PUSH_BUTTON, roles.LIST_ITEM}

    def result(index):
        return f"JWM Keys target {index:02}"

    def require(predicate, failure):
        if not wait(predicate, 5):
            raise ProbeError(failure)

    def focus(label, accepted):
        node = named(current(), label, accepted)
        return node is not None and node.get_state_set().contains(atspi.StateType.FOCUSED)

    require(lambda: focus(EDITOR, fields), "initial-editor-not-focused")
    if named(current(), result(23), buttons) is not None:
        raise ProbeError("last-result-initially-visible")
    send("key", "ctrl+f")
    require(lambda: focus(SEARCH, fields), "keyboard-search-not-focused")
    send("type", "--", "JWM Keys")
    for key, index in (("Down", 0), ("Down", 1), ("Up", 0)):
        send("key", key)
        require(lambda: focus(result(index), buttons), f"keyboard-result-{index:02}-not-focused")
    if named(current(), result(23), buttons) is not None:
        raise ProbeError("last-result-visible-before-up-from-search")
    send("key", "Escape")
    require(lambda: window(INITIAL) and focus(EDITOR, fields), "escape-did-not-restore-editor")
    send("key", "ctrl+f")
    require(lambda: focus(SEARCH, fields), "keyboard-search-not-refocused")
    send("key", "Up")
    require(lambda: focus(result(23), buttons), "offscreen-last-result-not-revealed")
    send("key", "Return")
    require(lambda: window(result(23)) and focus(EDITOR, fields), "keyboard-result-not-opened")
    return "notes-keyboard-down-up-escape-reveal-and-enter-observed"


def run_isolated(notes_path, atspi, set_reader, keyboard=False):
    env = dict(os.environ)
    launched = []
    with tempfile.TemporaryDirectory(prefix="jwm-notes-atspi-") as t:
        temp = Path(t)
        data = temp / "data"
        n = data / "jwm" / "notes"
        n.mkdir(parents=True)
        (n / "a.txt").write_text(INITIAL + "\n")
        (n / "z.txt").write_text(TARGET + "\n")
        os.utime(n / "a.txt", (1700010000, 1700010000))
        os.utime(n / "z.txt", (1700000000, 1700000000))
        if keyboard:
            for index in range(24):
                path = n / f"key-{index:02}.txt"
                path.write_text(f"JWM Keys target {index:02}\n", encoding="utf8")
                timestamp = 1700009000 - 100 * index
                os.utime(path, (timestamp, timestamp))
        original = {path.name: path.read_bytes() for path in n.glob("*.txt")}
        for name in ("home", "config", "cache", "state", "run"):
            (temp / name).mkdir(mode=0o700)
        env.update(HOME=str(temp / "home"), XDG_DATA_HOME=str(data),
                   XDG_CONFIG_HOME=str(temp / "config"), XDG_CACHE_HOME=str(temp / "cache"),
                   XDG_STATE_HOME=str(temp / "state"), XDG_RUNTIME_DIR=str(temp / "run"),
                   LANG="C.UTF-8", LC_ALL="C.UTF-8", WINIT_UNIX_BACKEND="x11",
                   GDK_BACKEND="x11", GSETTINGS_BACKEND="memory", XDG_SESSION_TYPE="x11")
        for name in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "NO_AT_BRIDGE",
                     "AT_SPI_BUS_ADDRESS", "GTK_MODULES", "GTK_A11Y"):
            env.pop(name, None)
        receipt = temp / "gtk-action"
        # AccessKit listens for the actual ScreenReaderEnabled property.
        # Exercise both transitions only in this owned, disposable bus.
        set_reader(False)
        try:
            control = subprocess.Popen([sys.executable, __file__, "--gtk", str(receipt)], env=env)
            launched.append(control)
            notes = subprocess.Popen([str(notes_path)], env=env)
            launched.append(notes)

            def ready():
                if notes.poll() is not None:
                    raise ProbeError("notes-process-exited")
                listing = xdo("search", "--all", "--onlyvisible", "--pid", str(notes.pid),
                              "--name", "^" + re.escape(INITIAL) + " ", env=env)
                ids = listing.stdout.splitlines()
                return ids[0] if listing.returncode == 0 and len(ids) == 1 else None

            wid = wait(ready, 12)
            if wid is None:
                raise ProbeError("notes-x11-window-never-mapped")
            refocused = False

            def window(title):
                nonlocal refocused
                if notes.poll() is not None:
                    return False
                actual = xdo("getwindowname", wid, env=env)
                if actual.returncode or actual.stdout.rstrip("\n") != title + " — JWM 备忘录":
                    return False
                if not refocused and title == INITIAL:
                    if xdo("windowfocus", "--sync", wid, env=env).returncode:
                        raise ProbeError("x11-focus-request-failed")
                    refocused = True
                focus = xdo("getwindowfocus", "-f", env=env)
                return focus.returncode == 0 and focus.stdout.strip() == wid

            def send(*argv):
                # Without --window, xdotool uses real XTEST input to the focused
                # window, rather than XSendEvent. Never inject to a foreign window.
                focused = xdo("getwindowfocus", "-f", env=env)
                if focused.returncode or focused.stdout.strip() != wid:
                    raise ProbeError("notes-not-focused-before-key")
                if xdo(*argv, env=env).returncode:
                    raise ProbeError("x11-key-injection-failed")

            outcome = evaluate(atspi, control.pid, notes.pid, receipt, window, 7,
                               enable_reader=lambda: set_reader(True),
                               keyboard=(lambda *a: keyboard_results(*a, send)) if keyboard else None)
            if keyboard and {path.name: path.read_bytes() for path in n.glob("*.txt")} != original:
                raise ProbeError("keyboard-navigation-modified-notebook")
            return outcome
        finally:
            for child in reversed(launched):
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=3)


def main():
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("--gtk", type=Path, help=argparse.SUPPRESS)
    args.add_argument("--notes", type=Path)
    args.add_argument("--expect", choices=("baseline", "working", "keyboard"))
    opt = args.parse_args()
    if (os.getenv("JWM_ATSPI_ISOLATED") != "1" or not os.getenv("DISPLAY")
            or not os.getenv("DBUS_SESSION_BUS_ADDRESS") or os.getenv("AT_SPI_BUS_ADDRESS")):
        args.error("use a fresh private D-Bus session and Xvfb test display")
    if opt.gtk:
        gtk_control(opt.gtk)
        return 0
    if not opt.notes or not opt.expect:
        args.error("--notes and --expect are required")
    import gi
    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi, Gio, GLib

    def set_reader(enabled):
        conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        address = ("org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties")
        status = "org.a11y.Status"
        prop = "ScreenReaderEnabled"
        conn.call_sync(*address, "Set",
                       GLib.Variant("(ssv)", (status, prop, GLib.Variant("b", enabled))),
                       None, Gio.DBusCallFlags.NONE, 3000, None)
        value = conn.call_sync(*address, "Get", GLib.Variant("(ss)", (status, prop)),
                               GLib.VariantType("(v)"), Gio.DBusCallFlags.NONE, 3000, None)
        if value.get_child_value(0).get_variant().get_boolean() != enabled:
            raise ProbeError("screen-reader-state-not-confirmed")

    try:
        if Atspi.init() != 0:
            raise ProbeError("atspi-init-failed")
        Atspi.set_timeout(1500, 3000)
        outcome = run_isolated(opt.notes.resolve(), Atspi, set_reader, keyboard=opt.expect == "keyboard")
    except (ProbeError, GLib.Error, OSError, AttributeError, TypeError,
            subprocess.SubprocessError) as exc:
        outcome = str(exc) if isinstance(exc, ProbeError) else "probe-transport-or-fixture-error"
        print(json.dumps({"outcome": outcome}))
        return 2
    print(json.dumps({"backend": "x11", "outcome": outcome,
                      "gtk_positive_control": "action-observed", "screen_reader_enabled": True,
                      "speech": "not-tested"}))
    expected = {
        "baseline": "notes-no-atspi-application-for-pid",
        "working": "notes-named-controls-action-and-focus-observed",
        "keyboard": "notes-keyboard-down-up-escape-reveal-and-enter-observed",
    }[opt.expect]
    return 0 if outcome == expected else 1


if __name__ == "__main__":
    raise SystemExit(main())
