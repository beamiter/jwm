#!/usr/bin/env python3
"""Verify env-selected daemon-managed restart on an owned private Xvfb.

Never connects to the live desktop. Both daemon control and WM IPC use the
fixture's private runtime directory. No production source is modified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
        if fields[0] == "Z":
            return None
        return {"pid": pid, "start_time": int(fields[19]), "parent": int(fields[1])}
    except (OSError, IndexError, ValueError):
        return None


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--shell-bin", type=Path, default=ROOT / "crates/jwm_shell/target/release/jwm-shell")
    parser.add_argument("--bar-bin", type=Path, default=ROOT / "target/tauri-release/release/tauri_leptos_bar")
    parser.add_argument("--evidence-dir", type=Path, default=ROOT / "artifacts/shared-shell/daemon-final")
    parser.add_argument("--capture-child-stderr", action="store_true", help="additional observer variant: shell exec wrapper only redirects the managed child's stdout/stderr")
    args = parser.parse_args()
    binaries = {"jwm": (args.bin_dir / "jwm").resolve(), "tool": (args.bin_dir / "jwm-tool").resolve(),
                "host": args.shell_bin.resolve(), "bar": args.bar_bin.resolve()}
    mode = "env-x11rb-child-log" if args.capture_child_stderr else "env-x11rb-exact"
    evidence = (args.evidence_dir / mode).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    result = {"status": "running", "mode": mode, "steps": [], "backend_source": "JWM_BACKEND environment only",
              "fixture_sha256": digest(__file__),
              "binaries": {name: {"path": str(path), "sha256": digest(path)} for name, path in binaries.items()}}
    children = []
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="jwm-daemon-case-") as directory:
        work = Path(directory)
        result["private_directory"] = str(work)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11", NO_AT_BRIDGE="1")
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "JWM_SOCKET", "JWM_DAEMON_PID",
                    "JWM_RESTARTING", "JWM_RESTART_STATE", "JWM_DESKTOP_SHELL", "JWM_DESKTOP_SHELL_BACKEND", "JWM_BAR_LAYOUT"):
            env.pop(key, None)
        for key, name in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime"), ("XDG_PICTURES_DIR", "pictures")):
            (work / name).mkdir(mode=0o700)
            env[key] = str(work / name)
        env["JWM_BACKEND"] = "x11rb"
        env["JWM_BINARY"] = str(binaries["jwm"])
        env["JWM_SHELL_BIN"] = str(binaries["host"])
        env["JWM_SOCKET"] = str(work / "runtime/jwm-ipc.sock")
        env["XBAR_CONFIG"] = str(work / "xbar.toml")
        (work / "xbar.toml").write_text('theme="dark"\n')
        (work / "bin").mkdir()
        for name, path in (("jwm", binaries["jwm"]), ("jwm-tool", binaries["tool"]), ("tauri_leptos_bar", binaries["bar"])):
            (work / "bin" / name).symlink_to(path)
        env["PATH"] = str(work / "bin") + os.pathsep + env.get("PATH", "")
        logs = {name: (evidence / (name + ".log")).open("w+") for name in ("daemon-stdout", "daemon-stderr", "xvfb", "applications")}
        if args.capture_child_stderr:
            # No fork or backend argument: after exec this is still the
            # daemon's exact child PID, running the authoritative JWM binary.
            import shlex
            wrapper = work / "bin/capture-jwm"
            wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(str(binaries["jwm"])) + " >" + shlex.quote(str(evidence / "jwm-stdout.log")) + " 2>" + shlex.quote(str(evidence / "jwm-stderr.log")) + "\n")
            wrapper.chmod(0o700)
            env["JWM_BINARY"] = str(wrapper)

        def run(*command):
            # This assertion guards every control invocation, including cleanup.
            assert Path(env["JWM_SOCKET"]).parent == work / "runtime"
            return subprocess.check_output(list(map(str, command)), env=env, text=True,
                                           stderr=subprocess.STDOUT, timeout=15)

        def spawn(*command, stdout=None, stderr=None, **kwargs):
            child = subprocess.Popen(list(map(str, command)), env=env,
                                     stdout=stdout or logs["applications"], stderr=stderr or logs["applications"],
                                     start_new_session=True, **kwargs)
            children.append(child)
            return child

        def wait(predicate, label, timeout=40):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, AssertionError, subprocess.SubprocessError, StopIteration) as error:
                    last = str(error)
                time.sleep(0.1)
            raise AssertionError(f"{label}; last={last}")

        def msg(command, arguments=None):
            argv = [binaries["tool"], "msg", command, "--raw"]
            if arguments is not None:
                argv += ["--args", json.dumps(arguments)]
            response = json.loads(run(*argv))
            assert response.get("success", True), response
            return response.get("data", response)

        def state():
            return msg("get_desktop_shell")

        def ready():
            value = state()
            return value if value["host"]["ready"] and len(value["host"]["panels"]) == 3 and any(row.get("window") and row.get("pid") for row in value["host"]["top_bars"]) else None

        def frontend_ready():
            current = ready()
            if not current:
                return None
            pid = current["host"]["top_bars"][0]["pid"]
            if Path(f"/proc/{pid}/exe").resolve() != binaries["bar"]:
                return None
            properties = {}
            for window in run("xdotool", "search", "--pid", pid).split():
                raw = run("xprop", "-id", window, "_JWM_FRONTEND_READY", "_JWM_FRONTEND_RENDERED", "_JWM_FRONTEND_ERROR")
                properties[window] = raw
                if re.search(r"_JWM_FRONTEND_READY\([^)]*\)\s*=\s*1\s*$", raw, re.M) and re.search(r"_JWM_FRONTEND_RENDERED\([^)]*\)\s*=\s*1\s*$", raw, re.M):
                    return {"pid": pid, "properties": properties, "sha256": digest(f"/proc/{pid}/exe")}
            return None

        def snapshot(label):
            data = {"label": label, "daemon_pidfile": (work / "runtime/jwm_daemon.pid").read_text() if (work / "runtime/jwm_daemon.pid").exists() else None}
            for name, command in (("shell", "get_desktop_shell"), ("windows", "get_windows")):
                try:
                    data[name] = msg(command)
                except Exception as error:
                    data[name] = {"error": str(error)}
            try:
                data["daemon_status"] = run(binaries["tool"], "status")
            except Exception as error:
                data["daemon_status"] = {"error": str(error)}
            return data

        def record(text):
            result["steps"].append(text)
            print("PASS:", text, flush=True)

        try:
            for name in ("Xvfb", "xdotool", "xprop", "xwininfo", "xmessage", "dbus-run-session", "xdpyinfo"):
                assert shutil.which(name), name
            read_fd, write_fd = os.pipe()
            try:
                server = spawn("Xvfb", "-displayfd", write_fd, "-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset",
                               stdout=logs["xvfb"], stderr=logs["xvfb"], pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                number = bytearray()
                deadline = time.monotonic() + 10
                while b"\n" not in number:
                    assert select.select([read_fd], [], [], max(0, deadline - time.monotonic()))[0], "owned Xvfb readiness"
                    chunk = os.read(read_fd, 32)
                    assert chunk, "owned Xvfb closed readiness pipe"
                    number.extend(chunk)
                env["DISPLAY"] = ":" + number.decode().strip()
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            run(binaries["jwm"], "--gen-config")
            config = work / "config/jwm/config_x11.toml"
            content = config.read_text()
            content = re.sub(r'(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)', '[status_bar]\nname="tauri_leptos_bar"\nshow_bar=true\nposition="top"\nlayout="single"\n\n', content)
            content = re.sub(r'(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)', '', content)
            content += '\n[desktop_shell]\nenabled=true\nleft="always"\nright="always"\nbottom="always"\n'
            config.write_text(content)
            run(binaries["jwm"], "--check-config")
            session = spawn("dbus-run-session", "--", binaries["tool"], "daemon", stdout=logs["daemon-stdout"], stderr=logs["daemon-stderr"])
            before = wait(ready, "daemon-managed current JWM and final panels ready", timeout=60)
            result["frontend_before"] = wait(frontend_ready, "final Leptos actual rendered acknowledgement", timeout=60)
            assert result["frontend_before"]["sha256"] == result["binaries"]["bar"]["sha256"]
            daemon_pid = int((work / "runtime/jwm_daemon.pid").read_text().split()[1])
            wm_pid = identity(before["host"]["pid"])["parent"]
            daemon_identity, wm_identity, server_identity = identity(daemon_pid), identity(wm_pid), identity(server.pid)
            assert wm_identity["parent"] == daemon_pid
            wm_arguments = Path(f"/proc/{wm_pid}/cmdline").read_bytes().split(b"\0")
            assert b"--backend" not in wm_arguments and len([arg for arg in wm_arguments if arg]) == 1, wm_arguments
            wm_environment = dict(entry.split(b"=", 1) for entry in Path(f"/proc/{wm_pid}/environ").read_bytes().split(b"\0") if b"=" in entry)
            assert wm_environment[b"JWM_BACKEND"] == b"x11rb" and wm_environment[b"JWM_DAEMON_PID"] == str(daemon_pid).encode()
            assert before["host"]["backend"] == "x11rb"
            result["managed_child_stderr_fd"] = os.readlink(f"/proc/{wm_pid}/fd/2")
            assert digest(f"/proc/{wm_pid}/exe") == result["binaries"]["jwm"]["sha256"]
            assert digest(f'/proc/{before["host"]["pid"]}/exe') == result["binaries"]["host"]["sha256"]
            record("final daemon owns final JWM; backend comes only from environment, no CLI backend")

            applications = []
            for index in range(5):
                name = f"PrivateDaemonApplication{index}"
                process = spawn("xmessage", "-name", name, "-title", name, "private daemon restart application")
                window = wait(lambda: next((row for row in msg("get_windows") if row["name"] == name), None), name)
                xid = int(run("xdotool", "search", "--name", "^" + re.escape(name) + "$ ".rstrip()).strip().splitlines()[0])
                applications.append({"name": name, "identity": identity(process.pid), "window": window["id"], "x11_window": xid})
            for application in applications[-2:]:
                msg("focus_window", {"id": application["window"]})
                msg("minimize")
                wait(lambda: next(row for row in msg("get_windows") if row["id"] == application["window"])["is_minimized"], "minimized task before daemon-managed restart")
            before_snapshot = snapshot("before_restart")
            result.update(before=before_snapshot, daemon_identity=daemon_identity, jwm_identity=wm_identity, xserver_identity=server_identity,
                          applications=applications, jwm_arguments=[arg.decode() for arg in wm_arguments if arg])
            result["daemon_status_before"] = run(binaries["tool"], "status")
            record("five real application processes/windows exist, including two minimized tasks")

            msg("restart")
            after = wait(lambda: (current if (current := ready()) and current["session"] != before["session"] else None), "IPC restart under daemon completes", timeout=60)
            result["frontend_after"] = wait(frontend_ready, "replacement final Leptos actual rendered acknowledgement", timeout=60)
            assert result["frontend_after"]["sha256"] == result["binaries"]["bar"]["sha256"]
            assert identity(daemon_pid) == daemon_identity, "parent daemon changed or exited"
            assert identity(wm_pid) == wm_identity, "restart did not preserve managed JWM process identity"
            assert identity(server.pid) == server_identity and server.poll() is None, "private X server changed or exited"
            assert session.poll() is None, "daemon session supervisor exited"
            run("xdpyinfo")
            after_windows = {row["name"]: row for row in msg("get_windows")}
            before_windows = {row["id"]: row for row in before_snapshot["windows"]}
            for application in applications:
                assert identity(application["identity"]["pid"]) == application["identity"], application
                assert application["name"] in after_windows, application
                # WM WindowId tokens belong to a session; the actual X11 XID
                # and external application's process are the durable objects.
                xid = int(run("xdotool", "search", "--name", "^" + re.escape(application["name"]) + "$ ".rstrip()).strip().splitlines()[0])
                assert xid == application["x11_window"], application
                assert after_windows[application["name"]]["is_minimized"] == before_windows[application["window"]]["is_minimized"], application
                application["after_window"] = after_windows[application["name"]]["id"]
                application["x11_after"] = run("xwininfo", "-id", xid)
            after_environment = dict(entry.split(b"=", 1) for entry in Path(f"/proc/{wm_pid}/environ").read_bytes().split(b"\0") if b"=" in entry)
            assert after_environment[b"JWM_BACKEND"] == b"x11rb" and after_environment[b"JWM_DAEMON_PID"] == str(daemon_pid).encode()
            assert after["host"]["backend"] == "x11rb"
            assert after["host"]["pid"] != before["host"]["pid"]
            result["after"] = snapshot("after_restart")
            result["daemon_status_after"] = run(binaries["tool"], "status")
            time.sleep(1)
            assert identity(daemon_pid) == daemon_identity and session.poll() is None
            daemon_log = work / "home/.local/share/jwm/jwm_daemon.log"
            log_text = daemon_log.read_text()
            assert "检测到JWM意外退出" not in log_text
            (evidence / "daemon-before-cleanup.log").write_text(log_text)
            record("IPC exec restart preserves daemon, JWM PID, private X server and all five apps/minimized states")
            record("replacement shared host registers, final Leptos remains managed and daemon loop still answers status")
            result["status"] = "passed"
        except BaseException as error:
            result.update(status="failed", error=repr(error))
            if isinstance(error, subprocess.CalledProcessError):
                result["command_output"] = str(error.output)[-8000:]
            result["failure_snapshot"] = snapshot("failure")
            raise
        finally:
            # Only this fixture's exact child process groups are terminated.
            for child in reversed(children):
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                        child.wait(timeout=5)
                    except (ProcessLookupError, subprocess.TimeoutExpired):
                        try:
                            os.killpg(child.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        child.wait(timeout=10)
            daemon_log = work / "home/.local/share/jwm/jwm_daemon.log"
            if daemon_log.exists():
                shutil.copyfile(daemon_log, evidence / "daemon-complete.log")
            result["elapsed_seconds"] = round(time.monotonic() - started, 3)
            result["child_stderr_observation"] = "exec wrapper redirects stderr; same PID and authoritative executable verified" if args.capture_child_stderr else "exact daemon launch redirects managed child stderr to /dev/null; daemon stderr is captured"
            (evidence / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            for log in logs.values():
                log.close()
    print(json.dumps({"status": result["status"], "evidence": str(evidence)}), flush=True)


if __name__ == "__main__":
    main()
