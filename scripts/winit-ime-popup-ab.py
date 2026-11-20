#!/usr/bin/env python3
"""Owned Xvfb host -> JWM wayland-winit -> real Fcitx/Pinyin -> native Notes."""
import argparse
import hashlib
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
CANDIDATE = "aabf8f46394d9212ac9d2f1234cb9dc300005626"
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


def popup_protocol(text):
    """Evidence for the current final-preedit popup commit in this Fcitx stream.

    Object IDs can be reused. Request line positions, rather than bare IDs or
    32-bit compositor timestamps, bind callback, attachment and role lifetime.
    """
    popups, buffers, callbacks = {}, {}, {}
    final_key = preedit = selection_key = None
    last = None
    detached = None
    for sequence, raw in enumerate(text.splitlines(), 1):
        if not (m := re.fullmatch(r"\[(\d+\.\d+)\]\s+(->\s+)?(\w+)@(\d+)\.(\w+)\((.*)\)", raw)):
            continue
        stamp, arrow, kind, oid, event, args = m.groups()
        item = {"line": sequence, "wayland_ms": float(stamp), "trace": raw.strip()}
        if not arrow and kind == "zwp_input_method_keyboard_grab_v2" and event == "key":
            if re.fullmatch(r"\d+,\s*\d+,\s*24,\s*1", args):
                final_key, preedit, last, detached, selection_key = item, None, None, None, None
            elif preedit and re.fullmatch(r"\d+,\s*\d+,\s*2,\s*1", args):
                selection_key = item
        elif arrow and kind == "zwp_input_method_v2" and event == "set_preedit_string":
            if final_key and args.startswith('"ni hao",'):
                preedit, last, detached = item, None, None
        elif arrow and kind == "zwp_input_method_v2" and event == "get_input_popup_surface":
            if bind := re.fullmatch(r"new id zwp_input_popup_surface_v2@(\d+),\s*wl_surface@(\d+)", args):
                role, surface = bind.groups()
                if surface in popups or any(p["role"] == role for p in popups.values()):
                    raise AssertionError("ambiguous living input popup role")
                popups[surface] = {"role": role, "created": item, "frames": [], "rect": None}
        elif not arrow and kind == "zwp_input_popup_surface_v2" and event == "text_input_rectangle":
            for p in popups.values():
                if p["role"] == oid and (rect := re.fullmatch(r"(-?\d+),\s*(-?\d+),\s*(\d+),\s*(\d+)", args)):
                    p["rect"] = {**item, "rect": list(map(int, rect.groups()))}
        elif arrow and kind == "zwp_input_popup_surface_v2" and event == "destroy":
            popups = {surface: p for surface, p in popups.items() if p["role"] != oid}
        elif arrow and kind == "wl_shm_pool" and event == "create_buffer":
            if b := re.fullmatch(r"new id wl_buffer@(\d+),\s*\d+,\s*(\d+),\s*(\d+),\s*\d+,\s*\d+", args):
                buffer, width, height = b.groups()
                buffers[buffer] = {**item, "id": buffer, "size": [int(width), int(height)]}
        elif arrow and kind == "wl_buffer" and event == "destroy":
            buffers.pop(oid, None)
        elif arrow and kind == "wl_surface":
            p = popups.get(oid)
            if event == "frame" and (f := re.fullmatch(r"new id wl_callback@(\d+)", args)):
                callback = {"id": f[1], "request": item}
                callbacks[f[1]] = callback
                if p is not None:
                    p["frames"].append(callback)
            elif p is not None and event == "attach":
                if b := re.fullmatch(r"(?:wl_buffer@(\d+)|nil|null),\s*-?\d+,\s*-?\d+", args):
                    p["attach"] = (item, b[1], buffers.get(b[1]))
                else:
                    p.pop("attach", None)
            elif p is not None and event in ("damage", "damage_buffer"):
                p["damage"] = item
            elif p is not None and event == "commit":
                attach = p.pop("attach", None)
                damage = p.pop("damage", None)
                frames, p["frames"] = p["frames"], []
                if preedit and not detached:
                    if attach and attach[1] is None:
                        if last and p["created"]["line"] == last["role_created"]["line"]:
                            detached = {"attach": attach[0], "commit": item, "selection_key": selection_key,
                                        "superseded_commit_line": last["commit"]["line"]}
                    else:
                        # A later commit supersedes an earlier completed
                        # frame. Never return the earlier buffer's receipt.
                        last = {"surface": oid, "role": p["role"], "role_created": p["created"],
                                "key": final_key, "preedit": preedit, "commit": item,
                                "attach": attach[0] if attach else None,
                                "shm_buffer": attach[2] if attach else None,
                                "damage": damage, "rectangle": p["rect"],
                                "callbacks": frames}
            elif event == "destroy":
                popups.pop(oid, None)
        elif not arrow and kind == "wl_callback" and event == "done":
            if oid in callbacks:
                callbacks.pop(oid)["done"] = item
    if last:
        # Keep rect notifications from the same popup generation if the
        # compositor sent them after that frame completed.
        p = popups.get(last["surface"])
        last["live_generation"] = bool(
            p and p["role"] == last["role"] and p["created"]["line"] == last["role_created"]["line"])
        if last["live_generation"]:
            last["rectangle"] = p["rect"] or last["rectangle"]
    return last, detached


def ready_popup_frame(text):
    frame, detached = popup_protocol(text)
    if (not frame or not frame["live_generation"] or detached or not frame["attach"] or not frame["shm_buffer"]
            or not frame["damage"] or not frame["rectangle"] or not frame["callbacks"]
            or min(frame["shm_buffer"]["size"]) <= 0 or min(frame["rectangle"]["rect"][2:]) <= 0):
        return None
    if not all(event["line"] > frame["preedit"]["line"]
               for event in (frame["attach"], frame["damage"], frame["commit"])):
        return None
    if not all(c.get("done") and c["request"]["line"] > frame["preedit"]["line"]
               and c["done"]["line"] > frame["commit"]["line"] for c in frame["callbacks"]):
        return None
    return frame


def timeout_signal(*_):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise TimeoutError("hosted fixture deadline interrupted")


def display_paths(number):
    return Path(f"/tmp/.X11-unix/X{number}"), Path(f"/tmp/.X{number}-lock")


def display_state(number):
    outer, lock = display_paths(number)
    names = {line.split()[-1] for line in Path("/proc/net/unix").read_text().splitlines()
             if line.split()}
    return {"filesystem_socket": os.path.lexists(outer), "lock": os.path.lexists(lock),
            "unix_socket": str(outer) in names, "abstract_socket": "@" + str(outer) in names}


def stop_xvfb(child):
    """Only signal the directly spawned child; Xvfb removes its own paths."""
    if child.poll() is not None:
        return "already-exited", child.returncode
    child.terminate()
    try:
        return "SIGTERM", child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        child.kill()
        return "SIGKILL-fallback", child.wait(timeout=2)


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
        display_number = {"A": 199, "B": 200}[args.role]
        outer_report = {"display": f":{display_number}", "owned_pid": None}
        report["outer_display"] = outer_report

        def verify_outer():
            require(xvfb is not None and xvfb.poll() is None, "owned Xvfb not live")
            require(Path(f"/proc/{xvfb.pid}/exe").resolve() == Path("/usr/bin/Xvfb").resolve(),
                    "wrong Xvfb executable")
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(2)
                conn.connect("/tmp/.X11-unix/X" + env["DISPLAY"][1:])
                peer = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(peer[:2] == (xvfb.pid, os.getuid()), "wrong outer Xvfb peer")

        def cmd(*argv, e=env, command_timeout=8):
            if argv[0] in ("xdotool", "import", "xdpyinfo"):
                verify_outer()
            completed = subprocess.run(list(map(str, argv)), env=e, text=True, capture_output=True,
                                       timeout=command_timeout)
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
            outer_path, lock = display_paths(display_number)
            outer_report["preflight"] = display_state(display_number)
            save()
            require(not any(outer_report["preflight"].values()),
                    "fixed outer display/lock/socket not free; no foreign cleanup attempted")
            env["DISPLAY"] = f":{display_number}"
            xvfb = launch("xvfb", "/usr/bin/Xvfb", env["DISPLAY"], "-auth", auth,
                           "-screen", "0", "1280x900x24", "-nolisten", "tcp", "-noreset")
            outer_report["owned_pid"] = xvfb.pid

            def x_ready():
                require(xvfb.poll() is None, "owned Xvfb exited")
                return lock.is_file() and outer_path.is_socket()

            wait(x_ready, "owned outer Xvfb socket/lock not ready", 8)
            require(lock.read_text().strip() == str(xvfb.pid), "wrong outer Xvfb lock owner")
            verify_outer()
            outer_report["peer_and_lock_verified"] = True
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

            def notes_window_geometry():
                matches = [r for r in ipc("query", "get_windows") if r["id"] == app_id]
                require(len(matches) == 1 and matches[0]["pid"] == notes.pid,
                        "ambiguous Notes capture geometry")
                r = matches[0]
                return {key: r[key] for key in ("x", "y", "w", "h")}

            def capture(name, seconds=8):
                path = out / name
                before = time.monotonic_ns()
                cmd("import", "-window", xid, path, command_timeout=seconds)
                after = time.monotonic_ns()
                png = path.read_bytes()
                require(png[:8] == b"\x89PNG\r\n\x1a\n" and png[12:16] == b"IHDR",
                        "owned-window capture did not write PNG")
                width, height = struct.unpack("!2I", png[16:24])
                require(width and height, "empty owned-window capture")
                return {"image": name, "sha256": hashlib.sha256(png).hexdigest(),
                        "size": [width, height], "capture_started_ns": before,
                        "capture_completed_ns": after}

            outer = cmd("xdotool", "getwindowgeometry", "--shell", xid)
            dims = dict(re.findall(r"(?m)^(WIDTH|HEIGHT)=(\d+)$", outer))
            require(set(dims) == {"WIDTH", "HEIGHT"}, "missing outer dimensions")
            # Away from text and expected popup; same action for A and B.
            cmd("xdotool", "mousemove", "--window", xid, int(dims["WIDTH"]) - 8,
                int(dims["HEIGHT"]) - 8)
            report["idle_capture"] = capture("idle.png")
            report["capture_geometry"] = {"outer": outer, "notes_before_keys": notes_window_geometry()}
            save()

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
            def final_notes_preedit():
                trace = read(notes_log)[nl:]
                markers = re.findall(
                    r'zwp_text_input_v3@(\d+)\.(preedit_string|done)\(([^\n]*)\)', trace)
                last = None
                for oid, call, params in markers:
                    if call == "preedit_string":
                        last = oid if params.startswith('"ni hao",') else None
                    elif last == oid:
                        return True
                return False
            wait(final_notes_preedit, "Notes did not acknowledge exact final ni hao preedit")
            report["preedit"] = {"observed": composing, "grab_presses": KEYS,
                                 "final_ni_hao_done": True}
            report["phase"] = "candidate-evidence-and-commit"
            save()
            # Poll and capture within 8s, bracketing the screenshot by protocol reads.
            # If another popup commit superseded it during capture, try once
            # more against that new frame, never retaining stale evidence.
            deadline = time.monotonic() + 8
            before = None
            for _ in range(2):
                frame = wait(lambda: ready_popup_frame(read(fcitx_log)),
                             "no exact final-popup frame callback before capture",
                             max(0, deadline - time.monotonic()))
                observed = time.monotonic_ns()
                report["candidate_protocol_before_capture"] = {
                    "protocol": frame, "observed_monotonic_ns": observed,
                }
                save()
                remaining = deadline - time.monotonic()
                require(remaining > 0, "no time left for final popup screenshot")
                check = capture("before-select.png", remaining)
                after = ready_popup_frame(read(fcitx_log))
                if after and (frame["role_created"]["line"], frame["commit"]["line"]) == (
                        after["role_created"]["line"], after["commit"]["line"]):
                    before = after
                    break
            require(before, "final popup changed during owned capture")
            checked = time.monotonic_ns()
            report["candidate_evidence"] = {
                "protocol": before, "frame_observed_monotonic_ns": observed,
                "bracket_checked_monotonic_ns": checked, "capture": check,
                "notes_geometry": notes_window_geometry(),
                "visible_candidate": "pending independent PNG/ROI review; callback is not pixel proof",
            }
            save()
            verified = ready_popup_frame(read(fcitx_log))
            require(verified and (verified["role_created"]["line"], verified["commit"]["line"]) == (
                    before["role_created"]["line"], before["commit"]["line"]),
                    "popup generation or commit changed between capture and digit")
            key("1")
            wait(lambda: events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string"),
                 "no real candidate commit after digit")
            wait(lambda: after_done(read(notes_log)[nl:], "commit_string"),
                 "Notes commit not followed by protocol done")
            commits = events(read(notes_log)[nl:], "zwp_text_input_v3", "commit_string")
            require(commits == [EXPECTED], f"unexpected commit(s): {commits!r}")
            require(events(read(fcitx_log)[fl:], "zwp_input_method_v2", "commit_string") == [EXPECTED],
                    "Fcitx engine did not send the same exact commit")
            detach = wait(lambda: popup_protocol(read(fcitx_log))[1],
                          "no null-buffer commit for this popup after digit", 4)
            require(detach["selection_key"] and detach["superseded_commit_line"] == before["commit"]["line"]
                    and before["commit"]["line"] < detach["selection_key"]["line"] < detach["attach"]["line"]
                    < detach["commit"]["line"], "popup detach was not linked to captured frame and actual selection")
            report["candidate_evidence"]["detach"] = detach
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
            cleanup_errors = []
            for p in reversed(procs):
                if p is xvfb:
                    continue
                try:
                    # No signal after poll() has reaped this direct child.
                    if p.returncode is None:
                        try:
                            os.killpg(p.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    p.wait(timeout=3)
                except Exception as error:
                    cleanup_errors.append(repr(error))
            if xvfb is not None:
                try:
                    method, code = stop_xvfb(xvfb)
                    outer_report["cleanup"] = {"method": method, "returncode": code,
                                               "paths": display_state(display_number)}
                except Exception as error:
                    cleanup_errors.append(repr(error))
            else:
                outer_report["cleanup"] = {"method": "not-started",
                                           "paths": display_state(display_number)}
            # Report any residue but never unlink X11 socket/lock paths.
            # A and B intentionally use independent displays.
            if cleanup_errors:
                outer_report["cleanup_errors"] = cleanup_errors
                raise RuntimeError("owned fixture process cleanup failed")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jwm", type=lambda name: Path(name).resolve(), required=True)
    p.add_argument("--notes", type=lambda name: Path(name).resolve(), required=True)
    p.add_argument("--role", choices=("A", "B"), required=True)
    p.add_argument("--source", required=True)
    p.add_argument("--out", type=lambda name: Path(name).resolve(), required=True)
    a = p.parse_args()
    if (os.geteuid() == 0 or os.getenv("JWM_FCITX_ISOLATED") != "1"
            or os.getenv("DISPLAY") or os.getenv("XAUTHORITY")
            or not os.getenv("DBUS_SESSION_BUS_ADDRESS") or os.getenv("WAYLAND_DISPLAY")
            or os.getenv("WAYLAND_SOCKET") or os.getenv("GSETTINGS_BACKEND") != "memory"):
        p.error("requires owned Xvfb and private D-Bus session on an unprivileged runner")
    if a.source != (PIN if a.role == "A" else CANDIDATE):
        p.error("A must be exact #156 source SHA; B must be separately pinned")
    a.out.mkdir(parents=True, exist_ok=False)
    def digest(path):
        with path.open("rb") as f:
            return hashlib.file_digest(f, "sha256").hexdigest()
    report = {"source": a.source, "role": a.role,
              "binary_sha256": {"jwm": digest(a.jwm), "notes": digest(a.notes)}, "status": "failed", "phase": "startup",
              "scope": "nested native Wayland Notes, not #139 scroll, primary DRM, or physical keys"}
    signal.signal(signal.SIGTERM, timeout_signal)
    try:
        run(a, a.out, report)
        report["status"] = "popup-frame-and-commit-verified-pixel-review-pending"
        return 0
    except Exception:
        (a.out / "error.log").write_text(traceback.format_exc())
        return 1
    finally:
        (a.out / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
