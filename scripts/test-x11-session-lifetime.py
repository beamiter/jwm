#!/usr/bin/env python3
"""Private X11 login-lifetime regression; never uses the caller's desktop.

Models a display manager ending its X server when the session command exits.
Tests exec restart, failed replacement, explicit recovery, and explicit logout.
Optional --shell/--bar exercise the production shared sidebar host as well.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

SPEC = importlib.util.spec_from_file_location("restart_fixture", Path(__file__).with_name("test-restart-x11.py"))
R = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(R)
F = R.FIXTURE


def run_case(args, backend):
    work = Path(tempfile.mkdtemp(prefix="jwm-session-lifetime-"))
    children = F.Children()
    stopping = threading.Event()
    ended = threading.Event()
    watcher = None
    report = {"backend": backend, "directory": str(work), "status": "failed",
              "uses_user_desktop": False, "shared_shell": bool(args.shell), "steps": [],
              "binaries": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                           for name in ("jwm", "tool", "shell", "bar") if (path := getattr(args, name))}}
    try:
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("JWM_", "OPENAI_")) and k not in
               ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                "DBUS_SYSTEM_BUS_ADDRESS", "NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC")}
        for key, name in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                          ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
            (work / name).mkdir(mode=0o700)
            env[key] = str(work / name)
        (work / "bin").mkdir()
        installed = work / "bin/jwm"
        installed.symlink_to(args.jwm)
        (work / "bin/jwm-tool").symlink_to(args.tool)
        env.update(JWM_BINARY=str(installed), JWM_BACKEND=backend,
                   JWM_SOCKET=str(work / "runtime/jwm-ipc.sock"), LIBGL_ALWAYS_SOFTWARE="1",
                   DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"),
                   PATH=str(work / "bin") + os.pathsep + env.get("PATH", ""))
        if args.shell:
            env["JWM_SHELL_BIN"] = str(args.shell)
        socket = Path(env["JWM_SOCKET"])

        def run(*argv):
            assert socket.parent == work / "runtime"
            return subprocess.check_output(list(map(str, argv)), env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=15)

        def wait(predicate, label, timeout=30):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                if ended.is_set():
                    raise AssertionError("login lifetime ended: daemon exit caused owned X server teardown")
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                    last = str(error)
                time.sleep(0.05)
            raise AssertionError(f"{label}: {last}")

        def query(name):
            return F.query(socket, name)

        def command(name, data=None):
            return F.command(socket, name, data or {})

        env["DISPLAY"] = children.display("Xvfb", env, work)
        server = children.children[-1]
        run(installed, "--gen-config")
        for config in (work / "config/jwm").glob("config_*.toml"):
            text = config.read_text()
            for section, key, value, required in (
                    ("behavior", "compositor", "false", True),
                    ("status_bar", "show_bar", str(bool(args.bar)).lower(), True),
                    ("status_bar", "name", json.dumps(str(args.bar or "/bin/true")), True),
                    ("desktop_shell", "enabled", str(bool(args.shell)).lower(), False)):
                text = F.edit_setting(text, section, key, value, required)
            config.write_text(text)
        session = children.spawn(["dbus-run-session", "--", str(args.tool), "daemon"], env, work / "daemon.log")

        def login_lifetime():
            # Exact owned Popen handles, no global kill/display lookup. Unlike
            # older fixtures, X server lifetime depends on the session command.
            while not stopping.wait(0.02):
                if session.poll() is not None:
                    ended.set()
                    if server.poll() is None:
                        server.terminate()
                    return

        watcher = threading.Thread(target=login_lifetime, daemon=True)
        watcher.start()
        before = wait(lambda: query("get_execution_context"), "initial WM")
        daemon_pid = int((work / "runtime/jwm_daemon.pid").read_text().split()[1])
        daemon_identity = R.identity(daemon_pid)
        wm_pids = Path(f"/proc/{daemon_pid}/task/{daemon_pid}/children").read_text().split()
        assert len(wm_pids) == 1, wm_pids
        wm_pid = int(wm_pids[0])
        wm_identity = R.identity(wm_pid)

        def shell_ready():
            if not args.shell:
                return True
            state = query("get_desktop_shell")
            return state if state["host"]["ready"] and len(state["host"]["panels"]) == 3 else None

        report["shell_before"] = wait(shell_ready, "initial shared shell")
        apps = []
        for index, tag in enumerate([1, 4, 128, 4, 1]):
            command("view", {"value": tag})
            command("setlayout", {"layout": "tile"})
            name = f"SessionLifetime{index}"
            child = children.spawn(["xmessage", "-name", name, "-title", name, name], env, work / f"app-{index}.log")
            row = wait(lambda: next((w for w in query("get_windows") if w["name"] == name), None), name)
            xid = int(run("xdotool", "search", "--name", "^" + name + "$").splitlines()[0])
            apps.append({"name": name, "identity": R.identity(child.pid), "xid": xid, "tags": row["tags"]})
        command("view", {"value": 4})
        layout = query("get_layout")
        context = query("get_execution_context")
        before_rows = {w["name"]: w for w in query("get_windows")}
        report["before"] = {"applications": apps, "layout": layout, "context": context, "windows": before_rows}

        def apps_alive():
            for app in apps:
                assert R.identity(app["identity"]["pid"]) == app["identity"], app
                assert int(run("xdotool", "search", "--name", "^" + app["name"] + "$").splitlines()[0]) == app["xid"], app

        command("restart")
        wait(lambda: query("get_execution_context")["wm_instance_id"] != before["wm_instance_id"], "exec restart")
        assert R.identity(wm_pid) == wm_identity
        assert R.identity(daemon_pid) == daemon_identity
        assert query("get_execution_context")["monitors"][0]["visible_tag_mask"] == 4
        assert query("get_layout") == layout
        def adopted_rows():
            rows = {w["name"]: w for w in query("get_windows")}
            return rows if all(a["name"] in rows for a in apps) else None

        rows = wait(adopted_rows, "all applications adopted")
        for app in apps:
            assert rows[app["name"]]["tags"] == app["tags"], app
            if app["tags"] == 4:
                for field in ("x", "y", "w", "h"):
                    assert rows[app["name"]][field] == before_rows[app["name"]][field], (app, field)
        apps_alive()
        report["shell_after_restart"] = wait(shell_ready, "replacement shared shell")
        report["steps"].append("exec restart retains daemon/WM identities, five app PID/XIDs, tags, view and layout")

        # Replace only this fixture's stable install symlink. The replacement
        # exec exits 1, emulating an upgrade which fails during bootstrap.
        replacement = work / "bin/jwm.next"
        replacement.symlink_to("/bin/false")
        replacement.replace(installed)
        command("restart")
        wait(lambda: R.identity(wm_pid) is None, "failed replacement exits")
        time.sleep(0.6)  # daemon polls every 200ms; observe its exit policy
        assert not ended.is_set() and session.poll() is None
        assert R.identity(daemon_pid) == daemon_identity
        apps_alive()
        report["failure_status"] = run(args.tool, "status")
        report["steps"].append("failed upgrade exits WM but retains daemon, login lifetime and five app PID/XIDs")

        replacement.symlink_to(args.jwm)
        replacement.replace(installed)
        run(args.tool, "start")
        wait(lambda: {a["name"] for a in apps}.issubset({w["name"] for w in query("get_windows")}),
             "manual recovery adopts existing windows")
        apps_alive()
        report["shell_after_recovery"] = wait(shell_ready, "recovered shared shell")
        report["steps"].append("rollback plus start recovers WM and existing applications; crash layout recovery not asserted")
        run(args.tool, "quit")
        assert ended.wait(5), "explicit logout must end session lifetime"
        server.wait(timeout=5)
        report["steps"].append("explicit quit ends modeled login lifetime and owned X server")
        report["status"] = "passed"
    except Exception as error:
        report["error"] = repr(error)
    finally:
        stopping.set()
        if watcher:
            watcher.join(timeout=2)
        children.close()
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, required=True)
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--shell", type=Path)
    parser.add_argument("--bar", type=Path)
    parser.add_argument("--backends", default="xcb,x11rb")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if bool(args.shell) != bool(args.bar):
        parser.error("--shell and --bar must be supplied together")
    backends = args.backends.split(",")
    if any(b not in ("xcb", "x11rb") for b in backends):
        parser.error("only explicit X11 backends are supported")
    for name in ("jwm", "tool", "shell", "bar"):
        path = getattr(args, name)
        if path:
            setattr(args, name, path.resolve(strict=True))
    for name in ("Xvfb", "dbus-run-session", "xmessage", "xdotool"):
        if not shutil.which(name):
            parser.error(f"missing dependency: {name}")
    reports = [run_case(args, backend) for backend in backends]
    args.report.write_text(json.dumps(reports, indent=2) + "\n")
    print(json.dumps(reports, indent=2))
    return 0 if all(r["status"] == "passed" for r in reports) else 1


if __name__ == "__main__":
    raise SystemExit(main())
