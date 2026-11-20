#!/usr/bin/env python3
"""Owned Xvfb host -> JWM wayland-winit -> real Fcitx/Pinyin -> native Notes."""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import struct
import subprocess
import tempfile
import time
import tomllib
import traceback

PIN = "a81a08d59cf13c559c612ccecc23988296fb16ae"
EXPECTED = "你好"
KEYS = [49, 23, 35, 30, 24]  # Linux evdev n i h a o, US layout.
PROFILE = """[Groups/0]
Name=Default
Default Layout=us
DefaultIM=pinyin

[Groups/0/Items/0]
Name=keyboard-us
Layout=

[Groups/0/Items/1]
Name=pinyin
Layout=

[GroupOrder]
0=Default
"""


def read(path):
    return path.read_text(errors="replace") if path.exists() else ""


def require(value, label):
    if not value:
        raise AssertionError(label)
    return value


def wait(get, label, seconds=10):
    end = time.monotonic() + seconds
    while True:
        value = get()
        if value:
            return value
        require(time.monotonic() < end, label)
        time.sleep(0.05)


def config_flags(path):
    text = path.read_text()
    for section, key in (("status_bar", "show_bar"),
                         ("behavior", "eva_companion_enabled"),
                         ("animation", "enabled"),
                         ("desktop_shell", "enabled")):
        table = re.search(r"(?ms)^\[" + section + r"\]\n(.*?)(?=^\[|\Z)", text)
        if not table:
            require(section == "desktop_shell", f"missing [{section}]")
            text += f"\n[{section}]\n{key} = false\n"
        else:
            body, count = re.subn(r"(?m)^\s*" + key + r"\s*=.*$", f"{key} = false", table[1])
            require(count <= 1, f"duplicate {section}.{key}")
            if not count:
                body = f"{key} = false\n" + body
            text = text[:table.start(1)] + body + text[table.end(1):]
        require(tomllib.loads(text)[section][key] is False, f"cannot disable {section}.{key}")
    path.write_text(text)


def grabbed_presses(text):
    return [int(k) for k in re.findall(
        r"zwp_input_method_keyboard_grab_v2@\d+\.key\(\s*\d+\s*,\s*\d+\s*,\s*(\d+)\s*,\s*1\s*\)",
        text)]


def events(text, object_name, event):
    return re.findall(rf'{object_name}@\d+\.{event}\("([^"]*)"', text)


def after_done(text, event):
    markers = re.findall(r"zwp_text_input_v3@\d+\.(preedit_string|commit_string|done)\(", text)
    return event in markers and "done" in markers[markers.index(event) + 1:]


def timeout_signal(*_):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise TimeoutError("hosted fixture deadline interrupted")


def run(args, out, report):
    with tempfile.TemporaryDirectory(prefix="jwm-fcitx-") as temp:
        work = Path(temp)
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
               "DBUS_SESSION_BUS_ADDRESS": os.environ["DBUS_SESSION_BUS_ADDRESS"],
               "DBUS_SYSTEM_BUS_ADDRESS": f"unix:path={work}/no-system-bus",
               "LIBGL_ALWAYS_SOFTWARE": "1", "WINIT_X11_SCALE_FACTOR": "1",
               "GSETTINGS_BACKEND": "memory", "RUST_LOG": "info"}
        for key, leaf in (("HOME", "home"), ("XDG_RUNTIME_DIR", "run"),
                          ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                          ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state")):
            (work / leaf).mkdir(mode=0o700)
            env[key] = str(work / leaf)
        procs = []
        xvfb = None

        def verify_outer():
            require(xvfb is not None and xvfb.poll() is None, "owned Xvfb not live")
            require(Path(f"/proc/{xvfb.pid}/exe").resolve() == Path("/usr/bin/Xvfb").resolve(),
                    "wrong Xvfb executable")
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(2)
                conn.connect("/tmp/.X11-unix/X" + env["DISPLAY"][1:])
                peer = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(peer[:2] == (xvfb.pid, os.getuid()), "wrong outer Xvfb peer")

        def cmd(*argv, e=env):
            if argv[0] in ("xdotool", "import", "xdpyinfo"):
                verify_outer()
            completed = subprocess.run(list(map(str, argv)), env=e, text=True, capture_output=True,
                                       timeout=8)
            with (out / "commands.log").open("a") as log:
                log.write(f"{argv!r} rc={completed.returncode}\n{completed.stdout}{completed.stderr}\n")
            require(completed.returncode == 0, f"command failed: {argv[0]}")
            return completed.stdout.strip()

        def launch(label, *argv, e=env):
            with (out / (label + ".log")).open("wb") as log:
                p = subprocess.Popen(list(map(str, argv)), env=e, stdout=log, stderr=log,
                                     stdin=subprocess.DEVNULL, start_new_session=True)
            procs.append(p)
            return p

        def save():
            (out / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")

        def ipc(key, name, args_=None):
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(2)
                conn.connect(str(work / "run/jwm-ipc.sock"))
                peer = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                require(peer[:2] == (jwm.pid, os.getuid()), "wrong inner IPC peer")
                conn.sendall(json.dumps({key: name, "args": args_ or {}}).encode() + b"\n")
                with conn.makefile("rb") as stream:
                    message = json.loads(stream.readline(1024 * 1024))
                require(message.get("success") is True, f"IPC {name} failed")
                return message.get("data")

        def inner_ready():
            require(jwm.poll() is None, "JWM exited at startup")
            try:
                version = ipc("query", "get_version")
            except (OSError, ValueError):
                return None
            require(version["backend"] == "wayland-winit", "unexpected JWM backend")
            paths = [p for p in (work / "run").glob("wayland-*") if p.is_socket()]
            require(len(paths) <= 1, "ambiguous inner Wayland display")
            if paths:
                with socket.socket(socket.AF_UNIX) as check:
                    check.settimeout(2)
                    check.connect(str(paths[0]))
                    peer = struct.unpack("3i", check.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                require(peer[:2] == (jwm.pid, os.getuid()), "wrong inner Wayland peer")
                return paths[0].name

        try:
            report["phase"] = "outer-and-nested-setup"
            save()
            auth = work / "authority"
            wrong = work / "wrong-authority"
            for path in (auth, wrong):
                parts = (b"", b"", b"MIT-MAGIC-COOKIE-1", secrets.token_bytes(16))
                with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as f:
                    f.write(struct.pack("!H", 65535)
                            + b"".join(struct.pack("!H", len(part)) + part for part in parts))
            env["XAUTHORITY"] = str(auth)
            # Explicit display gets a real native lock file. Xvfb -displayfd
            # can omit that lock, enabling nested XWayland to collide with it.
            display_number = 199
            outer_path = Path(f"/tmp/.X11-unix/X{display_number}")
            lock = Path(f"/tmp/.X{display_number}-lock")
            require(not os.path.lexists(outer_path) and not os.path.lexists(lock)
                    and not any(line.split()[-1:] in ([str(outer_path)], ["@" + str(outer_path)])
                                for line in Path("/proc/net/unix").read_text().splitlines()),
                    "fixed outer display/lock/socket is not free")
            env["DISPLAY"] = f":{display_number}"
            xvfb = launch("xvfb", "/usr/bin/Xvfb", env["DISPLAY"], "-auth", auth,
                           "-screen", "0", "1280x900x24", "-nolisten", "tcp", "-noreset")

            def x_ready():
                require(xvfb.poll() is None, "owned Xvfb exited")
                return lock.is_file() and outer_path.is_socket()

            wait(x_ready, "owned outer Xvfb socket/lock not ready", 8)
            require(lock.read_text().strip() == str(xvfb.pid), "wrong outer Xvfb lock owner")
            verify_outer()
            cmd("xdpyinfo")
            rejected = subprocess.run(["/usr/bin/xdpyinfo"], env={**env, "XAUTHORITY": str(wrong)},
                                      capture_output=True, timeout=5)
            require(rejected.returncode != 0, "wrong outer Xauthority admitted")
            cmd("xdpyinfo")
            cmd(args.jwm, "--backend", "wayland-winit", "--gen-config")
            cfg = work / "config/jwm/config_wayland.toml"
            config_flags(cfg)
            cmd(args.jwm, "--backend", "wayland-winit", "--check-config")
            jwm = launch("jwm", args.jwm, "--backend", "wayland-winit")
            display = wait(inner_ready, "nested IPC/Wayland did not start", 20)
            ids = cmd("xdotool", "search", "--sync", "--all", "--onlyvisible", "--pid",
                      jwm.pid, "--name", r"^JWM \(wayland-winit\)$").splitlines()
            require(len(ids) == 1 and ids[0].isdigit(), "no unique owned JWM outer XID")
            xid = ids[0]
            cmd("xdotool", "windowfocus", "--sync", xid)
            require(cmd("xdotool", "getwindowfocus", "-f") == xid, "outer JWM window not focused")
            report["setup"] = {"backend": "wayland-winit", "outer_xid": xid,
                               "nested_peer_pid": jwm.pid, "socket": display,
                               "xvfb_peer_pid": xvfb.pid, "wrong_cookie_denied": True}
            report["phase"] = "native-input-method-setup"
            save()

            native = {k: v for k, v in env.items() if k not in ("DISPLAY", "XAUTHORITY")}
            native.update(WAYLAND_DISPLAY=display, XDG_SESSION_TYPE="wayland", WAYLAND_DEBUG="client")
            profile = work / "config/fcitx5/profile"
            profile.parent.mkdir()
            profile.write_text(PROFILE)
            note = work / "ime-fixture.txt"
            note.write_text("")
            # No DISPLAY reaches either inner client. Disable cloud dictionary.
            fcitx = launch("fcitx", "fcitx5", "--disable=cloudpinyin", e=native)
            notes = launch("notes", args.notes, note, e=native)
            fcitx_log, notes_log = out / "fcitx.log", out / "notes.log"

            def id_of_notes():
                require(notes.poll() is None and fcitx.poll() is None, "native app or IME exited")
                rows = [r for r in ipc("query", "get_windows") if r.get("pid") == notes.pid]
                require(len(rows) <= 1, "ambiguous Notes inner window")
                return rows[0]["id"] if rows else None

            app_id = wait(id_of_notes, "no owned Notes Wayland window", 12)
            ipc("command", "focus_window", {"id": app_id})
            wait(lambda: any(r["id"] == app_id and r["is_focused"]
                             for r in ipc("query", "get_windows")), "Notes not focused natively")

            def frontend():
                ln, lf = read(notes_log), read(fcitx_log)
                return (re.search(r"zwp_text_input_v3@\d+\.enable\(", ln)
                        and re.search(r"zwp_input_method_v2@\d+\.activate\(", lf)
                        and re.search(r"zwp_input_method_v2@\d+\.grab_keyboard\(", lf))

            wait(frontend, "no native text-input activation / Fcitx keyboard grab")
            cmd("fcitx5-remote", "-s", "pinyin", e=native)
            cmd("fcitx5-remote", "-o", e=native)
            require(cmd("fcitx5-remote", "-n", e=native) == "pinyin", "Pinyin not active")
            require(cmd("fcitx5-remote", e=native) == "2", "Fcitx engine not active")
            report["frontend"] = {"notes_pid": notes.pid, "inner_window": app_id,
                                  "ime_pid": fcitx.pid, "pinyin_active": True}
            report["phase"] = "real-key-preedit"
            save()

            def key(*keys):
                require(cmd("xdotool", "getwindowfocus", "-f") == xid, "outer input focus changed")
                cmd("xdotool", "key", "--delay", "80", *keys)

            fl = len(read(fcitx_log))
            nl = len(read(notes_log))
            key("n", "i", "h", "a", "o")
            wait(lambda: grabbed_presses(read(fcitx_log)[fl:]) == KEYS,
                 "real outer keys did not reach Fcitx grab exactly once")
            composing = wait(lambda: [s for s in events(read(notes_log)[nl:], "zwp_text_input_v3",
                                                       "preedit_string") if s],
                             "Notes did not receive nonempty preedit")
            require(not events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string"),
                    "text committed before selecting any candidate")
            wait(lambda: any(events(read(fcitx_log)[fl:], "zwp_input_method_v2",
                                     "set_preedit_string")),
                 "Fcitx did not send a nonempty preedit")
            wait(lambda: after_done(read(notes_log)[nl:], "preedit_string"),
                 "Notes preedit not followed by protocol done")
            require(note.read_text() == "", "preedit changed the backing file")
            report["preedit"] = {"observed": composing, "grab_presses": KEYS}
            report["phase"] = "candidate-evidence-and-commit"
            save()
            def capture(name):
                try:
                    cmd("import", "-window", xid, out / name)
                except (AssertionError, OSError, subprocess.SubprocessError) as error:
                    with (out / "commands.log").open("a") as log:
                        log.write(f"diagnostic capture {name} failed: {error}\n")
                    return None
                return name

            report["candidate_evidence"] = {
                "popup_role_requested": bool(re.search(
                    r"zwp_input_method_v2@\d+\.get_input_popup_surface\(", read(fcitx_log))),
                "image": capture("before-select.png"),
                "visible_candidate": "not verified; nested renderer does not establish IM popup composition",
            }
            key("1")
            wait(lambda: events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string"),
                 "no real candidate commit after digit")
            wait(lambda: after_done(read(notes_log)[nl:], "commit_string"),
                 "Notes commit not followed by protocol done")
            commits = events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string")
            require(commits == [EXPECTED], f"unexpected commit(s): {commits!r}")
            require(events(read(fcitx_log)[fl:], "zwp_input_method_v2", "commit_string") == [EXPECTED],
                    "Fcitx engine did not send the same exact commit")
            cmd("fcitx5-remote", "-c", e=native)
            key("ctrl+s")
            wait(lambda: note.read_text() == EXPECTED, "Notes save lacks exact Chinese bytes")
            require(events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string") == [EXPECTED],
                    "late duplicate commit")
            report["commit"] = {"notes_and_engine_once": True, "saved": EXPECTED,
                                "image": capture("after-commit.png")}
            report["phase"] = "complete"
            save()
        finally:
            for p in reversed(procs):
                try:
                    os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                p.wait(timeout=3)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jwm", type=lambda name: Path(name).resolve(), required=True)
    p.add_argument("--notes", type=lambda name: Path(name).resolve(), required=True)
    p.add_argument("--out", type=lambda name: Path(name).resolve(), required=True)
    a = p.parse_args()
    if (os.geteuid() == 0 or os.getenv("JWM_FCITX_ISOLATED") != "1"
            or os.getenv("DISPLAY") or os.getenv("XAUTHORITY")
            or not os.getenv("DBUS_SESSION_BUS_ADDRESS") or os.getenv("WAYLAND_DISPLAY")
            or os.getenv("WAYLAND_SOCKET") or os.getenv("GSETTINGS_BACKEND") != "memory"):
        p.error("requires owned Xvfb and private D-Bus session on an unprivileged runner")
    a.out.mkdir(parents=True, exist_ok=False)
    report = {"source": PIN, "status": "failed", "phase": "startup",
              "scope": "nested native Wayland Notes, not #139 scroll, primary DRM, or physical keys"}
    signal.signal(signal.SIGTERM, timeout_signal)
    try:
        run(a, a.out, report)
        report["status"] = "preedit-and-commit-verified"
        return 0
    except Exception:
        (a.out / "error.log").write_text(traceback.format_exc())
        return 1
    finally:
        (a.out / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
