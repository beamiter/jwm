#!/usr/bin/env python3
"""Drive the real Files shortcut and foot PTY inside private nested Wayland."""
import argparse
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def replace_setting(text, section, key, value, required=True):
    group = re.compile(r"(?ms)^\[" + re.escape(section) + r"\]\n(.*?)(?=^\[|\Z)")
    match = group.search(text)
    if match is None:
        if required:
            raise AssertionError(f"missing fixture section {section}")
        return text
    field = re.compile(r"(?m)^" + re.escape(key) + r"\s*=.*$")
    body, count = field.subn(lambda _: f"{key} = {value}", match[1])
    if count != 1 and required:
        raise AssertionError(f"expected one fixture field {section}.{key}, found {count}")
    return text[:match.start(1)] + body + text[match.end(1):]


def bounded(predicate, what, seconds=14):
    deadline, latest = time.monotonic() + seconds, None
    while time.monotonic() < deadline:
        try:
            if value := predicate():
                return value
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            latest = repr(error)
        time.sleep(0.03)
    raise AssertionError(f"{what}: timeout, last error {latest}")


def identity(path):
    observed = path.stat()
    return f"{observed.st_dev}:{observed.st_ino}"


def matching_owned_window(rows, pid, name):
    # JWM's get_windows serializes WindowInfo.name, not an xdg-shell "title" key.
    found = [row for row in rows if row.get("pid") == pid]
    assert len(found) <= 1, found
    return found[0] if found and found[0].get("name") == name else None


def prepare_terminal_fixture(root, chosen, env, foot):
    shell = root / "proof-shell"
    shell.write_text('''#!/bin/sh
set -eu
test -t 0
test -z "${DISPLAY:-}"
test -n "$WAYLAND_DISPLAY"
/usr/bin/stat -c '%d:%i' . > "$JWM_WL_FILES_FIXTURE/shell.tmp"
mv -- "$JWM_WL_FILES_FIXTURE/shell.tmp" "$JWM_WL_FILES_FIXTURE/shell.receipt"
sleep 2
''')
    adapter = root / "terminal-adapter"
    adapter.write_text('''#!/bin/sh
set -eu
root=$JWM_WL_FILES_FIXTURE
exec 2>"$root/foot.log"
test "$1" = -e
shift
mv -- "$JWM_WL_FILES_CHOSEN" "$root/original"
mkdir -- "$JWM_WL_FILES_CHOSEN"
printf '%s\n' "$$" > "$root/adapter.receipt"
exec "$JWM_WL_FILES_FOOT" --title='JWM Files Native Foot Fixture' \
  --app-id=org.jwm.FilesFootFixture /usr/bin/env "SHELL=$root/proof-shell" "$@"
''')
    shell.chmod(0o700)
    adapter.chmod(0o700)
    env.update(JWM_WL_FILES_FIXTURE=str(root), JWM_WL_FILES_CHOSEN=str(chosen),
               JWM_WL_FILES_FOOT=foot)
    descriptor = "org.jwm.FilesFootFixture.desktop"
    data = Path(env["XDG_DATA_HOME"])
    for leaf in ["applications", "xdg-terminals"]:
        (data / leaf).mkdir(mode=0o700)
        (data / leaf / descriptor).write_text(
            "[Desktop Entry]\nType=Application\nName=Native Foot Fixture\n"
            f"Exec={adapter}\nTryExec={adapter}\nCategories=System;TerminalEmulator;\n"
            "Terminal=false\nX-TerminalArgExec=-e\nX-ExecArg=-e\n"
        )
    (Path(env["XDG_CONFIG_HOME"]) / "xdg-terminals.list").write_text(descriptor + "\n")


def run(args, root, log):
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
           "LIBGL_ALWAYS_SOFTWARE": "1", "LP_NUM_THREADS": "2", "WINIT_X11_SCALE_FACTOR": "1",
           "DBUS_SYSTEM_BUS_ADDRESS": f"unix:path={root}/absent-system-bus"}
    for var, leaf in (("HOME", "home"), ("XDG_RUNTIME_DIR", "runtime"),
                      ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                      ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
        folder = root / leaf
        folder.mkdir(mode=0o700)
        env[var] = str(folder)
    env["XDG_CONFIG_DIRS"] = str(root / "empty")
    env["XDG_DATA_DIRS"] = str(root / "empty")
    (root / "empty").mkdir()
    for tool in ("Xvfb", "xdotool", "dbus-daemon", "xdg-terminal-exec", "foot"):
        assert shutil.which(tool, path=env["PATH"]), tool
    foot = shutil.which("foot", path=env["PATH"])
    chosen = root / os.fsdecode(b"held-%20-\x80")
    chosen.mkdir()
    expected = identity(chosen)
    if not args.drag_test:
        prepare_terminal_fixture(root, chosen, env, foot)
    owned = []

    def spawn(argv, child_env=None, **kw):
        child = subprocess.Popen(list(map(str, argv)), env=child_env or env, stdout=log,
                                 stderr=log, start_new_session=True, **kw)
        owned.append(child)
        return child

    def command(*argv):
        return subprocess.check_output(list(map(str, argv)), env=env, text=True,
                                       stderr=log, timeout=10)

    def jwm_query(name):
        answer = json.loads(command(args.jwm_tool, "msg", name, "--raw"))
        assert answer.get("success", True), answer
        return answer.get("data", answer)

    def exact_window(pid, name):
        return matching_owned_window(jwm_query("get_windows"), pid, name)

    try:
        # Give both JWM and the separate GTK client an explicit private bus;
        # neither may block on nor access an ambient desktop session.
        read, write = os.pipe()
        try:
            spawn(["dbus-daemon", "--session", "--nofork", f"--print-address={write}"],
                  pass_fds=(write,))
            os.close(write)
            write = None
            assert select.select([read], [], [], 10)[0], "private D-Bus not ready"
            address = os.read(read, 1024).decode().strip()
            assert address.startswith("unix:"), address
            env["DBUS_SESSION_BUS_ADDRESS"] = address
        finally:
            os.close(read)
            if write is not None:
                os.close(write)
        width, height = (1920, 1000) if args.drag_test else (1280, 800)
        read, write = os.pipe()
        try:
            spawn(["Xvfb", "-displayfd", write, "-screen", "0", f"{width}x{height}x24",
                   "-nolisten", "tcp", "-noreset"], pass_fds=(write,))
            os.close(write)
            write = None
            assert select.select([read], [], [], 10)[0], "private Xvfb not ready"
            identifier = os.read(read, 32).decode().strip()
            assert identifier.isdecimal(), identifier
            env["DISPLAY"] = ":" + identifier
        finally:
            os.close(read)
            if write is not None:
                os.close(write)
        command(args.jwm, "--backend", "wayland-winit", "--gen-config")
        cfg = root / "config/jwm/config_wayland.toml"
        text = cfg.read_text()
        text = replace_setting(text, "status_bar", "show_bar", "false")
        text = replace_setting(text, "status_bar", "name", '"/bin/true"')
        text = replace_setting(text, "desktop_shell", "enabled", "false", required=False)
        cfg.write_text(text)
        command(args.jwm, "--backend", "wayland-winit", "--check-config")
        spawn([args.jwm, "--backend", "wayland-winit"])
        outer = bounded(lambda: command("xdotool", "search", "--name", "JWM ").strip(),
                        "outer JWM output").splitlines()[0]
        command("xdotool", "windowsize", outer, width, height)
        command("xdotool", "windowfocus", outer)
        bounded(lambda: jwm_query("get_monitors"), "real nested output")
        runtime = Path(env["XDG_RUNTIME_DIR"])
        socket = bounded(lambda: next((p.name for p in runtime.glob("wayland-*")
                                       if p.is_socket()), None), "private native Wayland socket")
        native = dict(env, WAYLAND_DISPLAY=socket, GDK_BACKEND="wayland",
                      GSK_RENDERER="cairo", GTK_A11Y="none")
        del native["DISPLAY"]
        if args.drag_test:
            native.update(
                JWM_FILE_DND_FIXTURE=str(root),
                JWM_FILE_DND_POINTER_DRIVER=str(Path(__file__).resolve()),
                JWM_FILE_DND_X11_HOST_DISPLAY=env["DISPLAY"],
                JWM_FILE_DND_OUTER=outer,
                JWM_FILE_DND_JWM_TOOL=str(args.jwm_tool),
            )
            fixture = spawn([args.drag_test,
                             "wayland_acceptance::native_jwm_wayland_drop_copies_one_exact_file",
                             "--exact", "--ignored", "--nocapture"], native)
            assert fixture.wait(timeout=60) == 0, "native Files pointer fixture failed"
            output = Path(log.name).read_text(errors="replace").splitlines()
            for marker in (
                "GTK4 Wayland drag copy: hover and outside release wrote nothing; conflict retained exact target",
                "GTK4 Wayland drag copy: real inter-window pointer copied exact native bytes and kept source and clipboard",
            ):
                assert output.count(marker) == 1, marker
                print(marker)
            return
        browser = spawn([args.files, "--", chosen], native)
        displayed = os.fsencode(chosen).decode("utf-8", "replace")
        native_browser = bounded(lambda: exact_window(browser.pid, "JWM Files · " + displayed),
                                 "native GTK Files view and committed raw directory")
        assert native_browser.get("is_on_view"), native_browser
        print("GTK4 Wayland terminal: native Files PID and exact raw directory matched", flush=True)
        command("xdotool", "windowfocus", outer)
        command("xdotool", "key", "--clearmodifiers", "ctrl+shift+Return")
        adapter_receipt = root / "adapter.receipt"
        pid = int(bounded(lambda: adapter_receipt.read_text().strip(), "packaged selector receipt"))
        bounded(lambda: exact_window(pid, "JWM Files Native Foot Fixture"),
                "real foot Wayland toplevel")
        receipt = root / "shell.receipt"
        observed = bounded(lambda: receipt.read_text().strip(), "actual PTY directory identity")
        assert observed == expected
        assert identity(root / "original") == expected and identity(chosen) != expected
        print("GTK4 Wayland terminal: real nested JWM key launched native foot PTY in exact held directory")
    except BaseException:
        try:
            print("private compositor window rows:", jwm_query("get_windows"), flush=True)
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        for file in (root / "foot.log", Path(log.name)):
            if file.is_file():
                print(f"--- {file.name} ---")
                print("\n".join(file.read_text(errors="replace").splitlines()[-60:]))
        raise
    finally:
        for child in reversed(owned):
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        for child in reversed(owned):
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=2)


def drag_pointer(root, tool, outer, host_display, window_pid, role, x, y):
    """Translate a fixture's native client point and inject into its private host."""
    assert host_display.startswith(":") and host_display[1:].isdecimal()
    assert role in ("source", "target")
    assert int(window_pid) > 1 and int(outer) > 1
    x, y = int(x), int(y)
    assert 0 <= x < 2400 and 0 <= y < 1200

    def command(argv, env):
        return subprocess.check_output(argv, env=env, text=True, timeout=5)

    client_env = dict(os.environ)
    assert "DISPLAY" not in client_env
    runtime = Path(client_env["XDG_RUNTIME_DIR"])
    assert runtime == root / "runtime" and runtime.is_dir()
    assert (runtime / client_env["WAYLAND_DISPLAY"]).is_socket()
    rows = json.loads(command([tool, "msg", "get_windows", "--raw"], client_env))
    assert rows.get("success", True), rows
    rows = [row for row in rows["data"] if row.get("pid") == int(window_pid)]
    assert len(rows) == 2, rows

    def rect(kind):
        title = f"JWM Files · {root / ('files-dnd-' + kind)}"
        found = [row for row in rows if row.get("title") == title]
        assert len(found) == 1, rows
        row = found[0]
        return tuple(int(row[key]) for key in ("x", "y", "w", "h"))

    a, b = rect("source"), rect("target")
    assert all(w >= 250 and h >= 250 for _, _, w, h in (a, b)), (a, b)
    assert (a[0] + a[2] <= b[0] or b[0] + b[2] <= a[0]
            or a[1] + a[3] <= b[1] or b[1] + b[3] <= a[1]), (a, b)
    wx, wy, ww, wh = a if role == "source" else b
    assert x < ww and y < wh, (role, x, y, a, b)
    gx, gy = wx + x, wy + y
    assert 0 <= gx < 1920 and 0 <= gy < 1000, (gx, gy)
    host_env = dict(client_env, DISPLAY=host_display)
    del host_env["WAYLAND_DISPLAY"]
    command(["xdotool", "mousemove", "--window", outer, str(gx), str(gy)], host_env)


def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--fixture-pointer":
        drag_pointer(
            Path(os.environ["JWM_FILE_DND_FIXTURE"]),
            os.environ["JWM_FILE_DND_JWM_TOOL"],
            os.environ["JWM_FILE_DND_OUTER"],
            os.environ["JWM_FILE_DND_X11_HOST_DISPLAY"],
            os.getppid(), *sys.argv[2:],
        )
        return
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("jwm", "jwm-tool"):
        parser.add_argument("--" + option, type=lambda p: Path(p).resolve(), required=True)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--files", type=lambda p: Path(p).resolve())
    group.add_argument("--drag-test", type=lambda p: Path(p).resolve())
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="jwm-files-wayland-") as directory:
        root = Path(directory)
        with (root / "session.log").open("w") as log:
            run(args, root, log)


if __name__ == "__main__":
    main()
