#!/usr/bin/env python3
"""Isolated real JWM + native terminal session association/reveal acceptance.

Uses synthetic Codex history and a local, inert stdin reader. No model, user's
home, application session, microphone, or provider credentials are accessed.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import signal
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("private_desktop", ROOT / "eva/tests/desktop-ipc.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def wait_for(call, description, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = call()
            if value:
                return value
        except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
            last = str(error)
        time.sleep(.05)
    raise RuntimeError(f"{description} timed out; last error: {last}")


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(jwm, app, binary, output):
    output.mkdir(parents=True)
    private = Path(tempfile.mkdtemp(prefix=f"jwm-session-{app}-"))
    children = fixture.Children()
    report = {"provider": app, "status": "failed", "jwm_sha256": sha(jwm), "terminal_sha256": sha(binary),
              "scope": "isolated_x11_real_gui_with_synthetic_provider", "physical_acceptance": False, "model_called": False, "steps": []}
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("OPENAI_", "ANTHROPIC_", "JWM_", "EVA_", "CODEX_", "CLAUDE_", "PULSE_", "PIPEWIRE_", "JTERM_", "ANVIL_", "FORGE_", "EMBER_", "FROST_")) or key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"):
            env.pop(key, None)
    for name, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
        path = private / leaf
        path.mkdir(mode=0o700)
        env[name] = str(path)
    env.update(LIBGL_ALWAYS_SOFTWARE="1", GDK_BACKEND="x11", WINIT_UNIX_BACKEND="x11", JWM_COMPOSITOR_API="glx", RUST_LOG="warn")
    bin_dir = private / "bin"
    bin_dir.mkdir()
    launches = private / "provider-launches"
    stub = bin_dir / "codex"
    stub.write_text(f'#!/bin/sh\nprintf "%s %s\\n" "$$" "$*" >> "{launches}"\nprintf "Synthetic desktop session; no network or model.\\n"\nexec /bin/cat\n')
    stub.chmod(0o700)
    env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
    env["JTERM_WORKSPACE_STATE_DIR"]=str(private/"state/jterm/agent-workspace")
    codex = private / "codex"
    (codex / "sessions").mkdir(parents=True)
    env["CODEX_HOME"] = str(codex)
    env["CLAUDE_CONFIG_DIR"] = str(private / "empty-claude")
    session_id = "01912345-1234-7123-8123-123456789abc"
    records = [
        {"type": "session_meta", "payload": {"id": session_id, "cwd": str(private)}},
        {"type": "event_msg", "payload": {"type": "user_message", "message": "Desktop session alpha"}},
        {"type": "event_msg", "payload": {"type": "task_complete"}},
    ]
    (codex / "sessions/rollout.jsonl").write_text("".join(json.dumps(record) + "\n" for record in records))
    (private / "config/jwm").mkdir(mode=0o700)
    subprocess.run([str(jwm), "--backend", "x11rb", "--gen-config"], env=env, check=True, capture_output=True, timeout=30)
    for config in (private / "config/jwm").glob("config_*.toml"):
        text = fixture.edit_setting(config.read_text(), "status_bar", "show_bar", "false")
        text = fixture.edit_setting(text, "desktop_shell", "enabled", "false", required=False)
        config.write_text(text)
    config = private / f"config/{app}/config.toml"
    config.parent.mkdir(mode=0o700)
    config.write_text('shell = "/bin/bash"\nrestore_session = false\ninitial_width = 1100\ninitial_height = 760\n')
    config.chmod(0o600)
    # Inherited default shells must not read the user's startup files.
    (private / "home/.bash_profile").write_text('export HISTFILE=/dev/null\nPS1="PRIVATE_DESKTOP> "\n')
    owned_helpers = {}
    def identity(pid):
        try:
            return Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()[19]
        except (OSError,IndexError):
            return None
    def remember_helpers():
        # Only exact PIDs recorded by this fixture's own private state are
        # considered, and their start ticks fence reuse before cleanup.
        lock=private/"state/jterm/agent-workspace/run/host.lock"
        try:
            host=json.loads(lock.read_text())
            pid=int(host[0]);
            if identity(pid)==str(host[1]):owned_helpers[pid]=str(host[1])
        except (OSError,ValueError,IndexError,TypeError):
            pass
        if launches.exists():
            for line in launches.read_text().splitlines():
                pid=int(line.split()[0]);start=identity(pid)
                if start:owned_helpers.setdefault(pid,start)
    try:
        bus_config=private/"session-bus.conf"
        bus_config.write_text('<busconfig><type>session</type><listen>unix:tmpdir='+env["XDG_RUNTIME_DIR"]+'</listen><policy context="default"><allow own="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy></busconfig>')
        bus_log=output/"private-bus.log"
        children.spawn(["dbus-daemon","--nofork","--print-address=1","--config-file="+str(bus_config)],env,bus_log)
        env["DBUS_SESSION_BUS_ADDRESS"]=wait_for(lambda:bus_log.read_text().splitlines()[0] if bus_log.exists() and bus_log.read_text().strip() else None,"private bus")
        env.update(GTK_USE_PORTAL="0",NO_AT_BRIDGE="1",GTK_A11Y="none")
        env["DISPLAY"] = children.display("Xvfb", env, output)
        env["DISPLAY"] = children.display("Xephyr", env, output)
        env["JWM_SOCKET"] = str(private / "runtime/jwm-ipc.sock")
        wm = children.spawn([str(jwm), "--backend", "x11rb"], env, output / "jwm.log")
        context = wait_for(lambda: fixture.query(Path(env["JWM_SOCKET"]), "get_execution_context"), "JWM context")
        assert wm.poll() is None and context["monitors"]
        child = children.spawn([str(binary)], env, output / f"{app}.log")
        def app_window():
            result = subprocess.run(["xdotool", "search", "--onlyvisible", "--pid", str(child.pid)], env=env, capture_output=True, text=True, timeout=3)
            return result.stdout.splitlines()[0] if result.returncode == 0 and result.stdout.strip() else None
        window = wait_for(app_window, "native terminal window")
        subprocess.run(["xdotool", "windowactivate", "--sync", window], env=env, check=True, timeout=5)
        time.sleep(1)
        subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+shift+alt+g"], env=env, check=True, timeout=5)
        time.sleep(2)
        if app in ("ember","frost"):
            # Their shortcut opens the persistent workspace, whose recent
            # session rows are direct native actions; it does not focus a
            # modal search field as the GTK manager does. Geometry belongs
            # to this fixed 1280x800 private display, not the user's screen.
            y=230 if app=="ember" else 310
            subprocess.run(["xdotool","mousemove","--sync","170",str(y),"click","1"],env=env,check=True,timeout=5)
        else:
            subprocess.run(["xdotool", "type", "--clearmodifiers", "Desktop session alpha"], env=env, check=True, timeout=5)
            time.sleep(1)
            subprocess.run(["xdotool", "key", "--clearmodifiers", "Down", "Return"], env=env, check=True, timeout=5)
        wait_for(lambda: launches.exists(), "native session launch")
        remember_helpers()
        def registered():
            snapshot = fixture.query(Path(env["JWM_SOCKET"]), "get_agent_sessions")
            (output / "last-registry.json").write_text(json.dumps(snapshot,indent=2))
            (output / "last-context.json").write_text(json.dumps(fixture.query(Path(env["JWM_SOCKET"]),"get_execution_context"),indent=2))
            entries = [row for row in snapshot["sessions"] if row["provider"] == app and row["can_reveal"]]
            return entries[0] if len(entries) == 1 else None
        session = wait_for(registered, "terminal session registration")
        assert session["pane_id"] and session["window_token"]
        before = launches.read_text().splitlines()
        for index in range(2):
            # Switch away, then reveal from JWM. The terminal must select the
            # already-owned pane, and never create a second provider process.
            subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+shift+t"], env=env, check=True, timeout=5)
            request = fixture.command(Path(env["JWM_SOCKET"]), "activate_agent_session", {
                "wm_instance_id": context["wm_instance_id"], "session_id": session["session_id"], "revision": session["revision"]})["data"]
            assert request["status"] == "queued"
            def completed():
                response = fixture.ipc(Path(env["JWM_SOCKET"]), {"query": "get_agent_session_requests", "args": {"host_id": session["host_id"], "host_incarnation": session["host_incarnation"]}})["data"]
                return next((item for item in response["requests"] if item["operation_id"] == request["operation_id"] and item["status"] == "revealed"), None)
            receipt = wait_for(completed, "GUI-owned pane reveal receipt", 8)
            assert launches.read_text().splitlines() == before, "desktop reveal launched another provider"
            report["steps"].append({"name": f"existing_pane_reveal_{index}", "result": "passed", "receipt": receipt})
        # A stale identity must be refused without focusing a replacement.
        stale = fixture.command(Path(env["JWM_SOCKET"]), "activate_agent_session", {"wm_instance_id": context["wm_instance_id"], "session_id": session["session_id"], "revision": session["revision"] + 1}, expect=False)
        assert not stale["success"]
        report["steps"].append({"name": "stale_session_revision_refused", "result": "passed"})
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        try:
            subprocess.run(["import", "-window", "root", str(output / "failure.png")], env=env, capture_output=True, timeout=5)
        except Exception:
            pass
        raise
    finally:
        remember_helpers()
        children.close()
        for pid,start in owned_helpers.items():
            if identity(pid)==start:
                try:os.kill(pid,signal.SIGTERM)
                except ProcessLookupError:pass
        report["provider_launches"] = launches.read_text().splitlines() if launches.exists() else []
        report["session_registry"] = locals().get("session")
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        shutil.rmtree(private)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, required=True)
    parser.add_argument("--terminal", action="append", required=True, help="provider=/absolute/path/to/binary")
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    for terminal in args.terminal:
        name, binary = terminal.split("=", 1)
        if name not in ("anvil", "forge", "ember", "frost"):
            parser.error("unknown terminal provider")
        run(args.jwm.resolve(strict=True), name, Path(binary).resolve(strict=True), args.evidence_dir.resolve() / name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
