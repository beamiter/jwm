#!/usr/bin/env python3
"""Run the real daemon against a disposable child; no X server is involved.

This tests process/command lifecycle only, not window preservation or layout.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def wait(predicate, label):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(label)


def run_case(tool, backend, root):
    root.mkdir(mode=0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith("JWM_")
           and k not in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS")}
    for key, name in (("HOME", "home"), ("XDG_RUNTIME_DIR", "runtime"),
                      ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        (root / name).mkdir(mode=0o700)
        env[key] = str(root / name)
    worker = root / "worker"
    trigger, pidfile = root / "trigger", root / "worker.pid"
    worker.write_text(f"#!{sys.executable}\n" + '''import os, time
from pathlib import Path
root = Path(__file__).parent
(root / "worker.pid").write_text(str(os.getpid()))
while not (root / "trigger").exists():
    time.sleep(0.02)
action = (root / "trigger").read_text()
os.execv("/bin/" + action, [action])
''')
    worker.chmod(0o700)
    env.update(JWM_BINARY=str(worker), JWM_BACKEND=backend)
    report = {"backend": backend, "status": "failed", "real_x11": False}
    with (root / "daemon.log").open("w") as log:
        daemon = subprocess.Popen([str(tool), "daemon"], env=env, stdout=log, stderr=log,
                                  start_new_session=True)
        try:
            wait(pidfile.exists, "daemon must launch fixture worker")
            first_pid = int(pidfile.read_text())
            trigger.write_text("false")  # same PID exec -> exit(1)
            time.sleep(0.8)
            if backend.startswith("wayland"):
                assert daemon.wait(timeout=5) == 0
                report["status"] = "passed"
                return report
            assert daemon.poll() is None, "failed replacement ended X11 daemon"

            def control(action):
                return subprocess.check_output([str(tool), action], env=env, text=True,
                                               stderr=subprocess.STDOUT, timeout=15)

            report["status_after_failure"] = control("status")
            assert "JWM未运行" in report["status_after_failure"]
            trigger.unlink()
            pidfile.unlink()
            control("start")
            wait(pidfile.exists, "recovery start must launch a replacement")
            assert int(pidfile.read_text()) != first_pid
            report["status_after_start"] = control("status")
            assert "JWM运行中" in report["status_after_start"]
            control("stop")
            assert daemon.poll() is None, "explicit stop must keep daemon"
            control("start")
            control("quit")
            assert daemon.wait(timeout=5) == 0
            report["status"] = "passed"
        except Exception as error:
            report["error"] = repr(error)
        finally:
            if daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait(timeout=5)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    tool = args.tool.resolve(strict=True)
    work = Path(tempfile.mkdtemp(prefix="jwm-daemon-recovery-"))
    reports = [run_case(tool, backend, work / backend) for backend in ("xcb", "x11rb", "wayland-udev")]
    result = {"directory": str(work), "tool_sha256": hashlib.sha256(tool.read_bytes()).hexdigest(), "cases": reports}
    args.report.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if all(r["status"] == "passed" for r in reports) else 1


if __name__ == "__main__":
    raise SystemExit(main())
