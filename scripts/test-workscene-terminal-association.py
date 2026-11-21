#!/usr/bin/env python3
"""Real private X11 launcher and focus; inert window/provider stand-ins only."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", action="store_true")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    assert os.environ.get("JWM_WORKSCENE_PRIVATE") == "1" and os.geteuid() != 0
    assert os.environ.get("DISPLAY", "").startswith(":") and os.environ.get("XAUTHORITY")
    assert not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("WAYLAND_SOCKET"))
    sock = Path(os.environ["JWM_SOCKET"])
    assert sock == Path(os.environ["XDG_RUNTIME_DIR"]) / "jwm-ipc.sock"
    assert sock.parent.parent == Path(os.environ["HOME"])
    args.out.mkdir(mode=0o700)

    def run(*argv):
        return subprocess.check_output(list(map(str, argv)), text=True,
                                       stderr=subprocess.PIPE, timeout=8).strip()

    def ipc(action, name, **params):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(3)
            conn.connect(str(sock))
            conn.sendall(json.dumps({action: name, "args": params}).encode() + b"\n")
            with conn.makefile("rb") as lines:
                raw = lines.readline(1024 * 1024)
            assert raw.endswith(b"\n")
        result = json.loads(raw)
        assert result["success"], (name, result)
        return result["data"]

    def wait(probe, problem, timeout=8):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if (v := probe()):
                return v
            time.sleep(.06)
        raise AssertionError(problem)

    def key(*names):
        run("xdotool", "key", "--clearmodifiers", "--delay", "55", *names)

    def shot(name):
        run("import", "-window", "root", args.out / (name + ".png"))

    def ui():
        return ipc("query", "get_system_ui").get("kind")

    def context():
        return ipc("query", "get_execution_context")

    def focused():
        return context().get("focus_token")

    def active_xid():
        try:
            return int(run("xdotool", "getactivewindow"))
        except subprocess.CalledProcessError:
            return None

    session_id = "fixture:workscene-session"

    def show():
        assert ui() is None
        key("Alt+r")
        wait(lambda: ui() == "launcher", "launcher unavailable")
        run("xdotool", "type", "--clearmodifiers", "--delay", "35", "scenes: Pane Witness")
        key("Return")
        wait(lambda: ui() == "workscene_preview", "scene unavailable")
        time.sleep(.45)

    def dismiss():
        key("Escape")
        wait(lambda: ui() is None, "scene did not dismiss")

    def requests(session):
        return ipc("query", "get_agent_session_requests",
                   host_id=session["host_id"], host_incarnation=session["host_incarnation"])["requests"]

    def register(session):
        ipc("command", "register_agent_session", wm_instance_id=wm, session=session)
        rows = ipc("query", "get_agent_sessions")["sessions"]
        row, = [r for r in rows if r["session_id"] == session_id]
        assert row["pane_id"] == session["pane_id"] and row["can_reveal"]

    def complete(request):
        identity = {k: request[k] for k in
                    ("operation_id", "session_id", "host_id", "host_incarnation",
                     "pane_id", "window_token", "revision")}
        assert ipc("command", "claim_agent_session_reveal",
                   wm_instance_id=wm, **identity)["status"] == "claimed"
        return ipc("command", "agent_session_revealed", wm_instance_id=wm,
                   status="revealed", **identity)

    children = []
    report = {"candidate": args.candidate, "physical_acceptance": False,
              "provider": "inert fixture, no actual terminal pane or model", "status": "failed"}
    try:
        wm = context()["wm_instance_id"]
        windows = []
        for name in ("workscene-original", "workscene-replacement"):
            proc = subprocess.Popen(["xmessage", "-name", name, "-title", name, name])
            children.append(proc)
            def window():
                try:
                    xid = int(run("xdotool", "search", "--onlyvisible", "--classname",
                                  "^" + name + "$").splitlines()[0])
                except subprocess.CalledProcessError as exc:
                    if exc.returncode == 1:
                        return None
                    raise
                return next((row for row in context()["windows"]
                             if int(row["id"]) == xid), None)
            windows.append(wait(window, "inert window not managed"))
        original, replacement = windows
        old = dict(session_id=session_id, project_id="fixture-project", provider="forge",
                   host_id="synthetic-host", host_incarnation="boot:old", pane_id="pane:old",
                   window_token=original["target_token"], title="Original inert pane",
                   status="running", can_reveal=True, revision=1)
        new = dict(old, host_incarnation="boot:new", pane_id="pane:new",
                   window_token=replacement["target_token"], title="Replacement inert pane")
        run("xdotool", "windowactivate", "--sync", original["id"])
        wait(lambda: focused() == original["target_token"] and
                     active_xid() == int(original["id"]), "original window not focused")
        register(old)
        show()
        shot("old-preview")
        ipc("command", "unregister_agent_session", wm_instance_id=wm,
            session_id=session_id, host_id=old["host_id"], host_incarnation=old["host_incarnation"])
        register(new)
        assert ui() == "workscene_preview" and not requests(new) and not requests(old)
        key("Down", "Return")
        time.sleep(.25)
        report["stale"] = {"before_focus": original["target_token"], "after_focus": focused(),
                           "queued": requests(new), "original_requests": requests(old),
                           "active_xid": active_xid()}
        shot("stale-action")
        assert not requests(old)
        if args.candidate:
            assert not requests(new) and focused() == original["target_token"]
            assert active_xid() == int(original["id"])
        else:
            queued, = requests(new)
            assert queued["status"] == "queued" and queued["pane_id"] == new["pane_id"]
            assert focused() == new["window_token"] and active_xid() == int(replacement["id"])
            report["baseline_receipt"] = complete(queued)

        dismiss()
        run("xdotool", "windowactivate", "--sync", original["id"])
        wait(lambda: focused() == original["target_token"], "original window not refocused")
        show()
        shot("fresh-preview")
        before = {r["operation_id"] for r in requests(new)}
        key("Down", "Return")
        queued, = [r for r in requests(new) if r["operation_id"] not in before]
        assert queued["status"] == "queued" and queued["pane_id"] == new["pane_id"]
        assert focused() == new["window_token"] and active_xid() == int(replacement["id"])
        report["fresh_receipt"] = complete(queued)
        report["fresh_focus"] = focused()
        shot("fresh-action")
        report["status"] = "passed" if args.candidate else "baseline incorrect focus/reveal reproduced"
    except Exception as error:
        report["error"] = repr(error)
        try:
            shot("error")
        except Exception:
            pass
        raise
    finally:
        (args.out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)


if __name__ == "__main__":
    main()
