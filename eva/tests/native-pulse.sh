#!/usr/bin/env bash
# Native PulseAudio contracts on an isolated synthetic server, never hardware.
# Supply JWM_EVA_PULSE_TEST_BINARY; --fixture-only checks server isolation only.
set -euo pipefail
cd "$(dirname "$0")/../.."
command -v python3 >/dev/null || { echo "missing test dependency: python3" >&2; exit 1; }
exec python3 - "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description="Private synthetic PulseAudio integration; no user server or hardware")
    parser.add_argument("--fixture-only", action="store_true",
                        help="validate the private server without claiming Rust tests passed")
    args = parser.parse_args()
    programs = {name: shutil.which(name) for name in ("pulseaudio", "pactl")}
    for name, path in programs.items():
        if path is None:
            parser.error("missing test dependency: " + name)
    binary = None
    if not args.fixture_only:
        value = os.environ.get("JWM_EVA_PULSE_TEST_BINARY")
        if not value:
            parser.error("set JWM_EVA_PULSE_TEST_BINARY to the compiled Rust test executable (this script never runs cargo)")
        binary = Path(value).resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error("JWM_EVA_PULSE_TEST_BINARY must be an executable file")
    available = subprocess.run([programs["pulseaudio"], "--dump-modules"],
                               check=True, capture_output=True, text=True, timeout=5).stdout
    required = {"module-native-protocol-unix", "module-null-sink", "module-sine-source"}
    names = {line.split()[0] for line in available.splitlines() if line.split()}
    missing = sorted(required - names)
    if missing:
        parser.error("missing synthetic PulseAudio modules: " + ", ".join(missing))

    root = Path(tempfile.mkdtemp(prefix="jwm-eva-pulse-", dir="/tmp"))
    root.chmod(0o700)
    for leaf in ("runtime", "config", "state", "data"):
        (root / leaf).mkdir(mode=0o700)
    socket = root / "runtime/native"
    address = "unix:" + str(socket)
    client = root / "client.conf"
    client.write_text("autospawn=no\ndefault-server=" + address + "\n")
    (root / "config/daemon.conf").write_text(
        "default-sample-format=s16le\ndefault-sample-rate=24000\n"
        "alternate-sample-rate=24000\ndefault-sample-channels=1\n"
        "default-channel-map=mono\nflat-volumes=no\n")
    config = root / "test.pa"
    # module-null-sink uses its 50 ms initial maximum latency when norewinds
    # is enabled, keeping silent playback/drain tests off the 2 s default.
    config.write_text(
        ".fail\n"
        "load-module module-native-protocol-unix socket=" + str(socket)
        + " auth-anonymous=1 auth-cookie-enabled=0\n"
        "load-module module-null-sink sink_name=eva_test_output format=s16le rate=24000 channels=1 channel_map=mono norewinds=true\n"
        "load-module module-sine-source source_name=eva_test_input rate=24000 frequency=440\n"
        "set-default-sink eva_test_output\nset-default-source eva_test_input\n")
    # Explicit protocol addresses and configuration roots prevent any fallback
    # to the invoking user's PA/PipeWire graph, cookie, D-Bus or autospawn.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("PULSE_", "PIPEWIRE_"))}
    env.update({
        "LC_ALL": "C", "PULSE_SERVER": address,
        "PULSE_RUNTIME_PATH": str(root / "runtime"),
        "PULSE_STATE_PATH": str(root / "state"),
        "PULSE_CONFIG_PATH": str(root / "config"),
        "PULSE_CLIENTCONFIG": str(client), "PULSE_COOKIE": str(root / "unused-cookie"),
        "PULSE_SINK": "eva_test_output", "PULSE_SOURCE": "eva_test_input",
        "XDG_RUNTIME_DIR": str(root / "runtime"), "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "state"), "XDG_DATA_HOME": str(root / "data"),
        "PIPEWIRE_RUNTIME_DIR": str(root / "runtime"), "PIPEWIRE_REMOTE": "no-pipewire-in-pulse-test",
        "DBUS_SESSION_BUS_ADDRESS": "unix:path=" + str(root / "no-session-bus"),
        "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=" + str(root / "no-system-bus"),
        "JWM_EVA_PULSE_TEST": "1", "JWM_EVA_TEST_SOURCE": "eva_test_input",
        "JWM_EVA_TEST_SINK": "eva_test_output",
    })
    server = None
    result = 0
    report = {"fixture_only": args.fixture_only, "uses_hardware": False,
              "uses_user_audio_server": False, "server_address": address,
              "source": "eva_test_input", "sink": "eva_test_output"}

    class Interrupted(Exception):
        def __init__(self, signum):
            self.signum = signum
            super().__init__("interrupted by signal " + str(signum))

    def interrupt(signum, _frame):
        raise Interrupted(signum)

    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupt)

    def pactl(*arguments):
        return subprocess.run([programs["pactl"], "--server=" + address, *arguments],
                              env=env, check=True, capture_output=True, text=True,
                              timeout=2).stdout

    def live():
        if server is None or server.poll() is not None:
            raise RuntimeError("the private PulseAudio server exited; see server.log")

    def streams():
        live()
        return {name: pactl("list", "short", name).splitlines()
                for name in ("sink-inputs", "source-outputs")}

    try:
        with (root / "server.log").open("w") as output:
            server = subprocess.Popen(
                [programs["pulseaudio"], "-n", "-F", str(config), "--daemonize=no",
                 "--use-pid-file=no", "--exit-idle-time=-1", "--high-priority=no",
                 "--realtime=no", "--disable-shm=yes", "--disallow-module-loading=yes",
                 "--log-target=stderr", "--log-level=notice"],
                env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                start_new_session=True)
        report["server_pid"] = server.pid
        deadline = time.monotonic() + 5
        while True:
            live()
            try:
                info = pactl("info")
                break
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
                if time.monotonic() >= deadline:
                    raise RuntimeError("private PulseAudio protocol readiness timed out")
                time.sleep(0.02)
        info_fields = dict(line.split(": ", 1) for line in info.splitlines() if ": " in line)
        if (info_fields.get("Default Sink") != "eva_test_output"
                or info_fields.get("Default Source") != "eva_test_input"):
            raise RuntimeError("private synthetic defaults were not selected")
        modules = pactl("list", "short", "modules")
        sinks = pactl("list", "short", "sinks")
        sources = pactl("list", "short", "sources")
        (root / "modules.txt").write_text(modules)
        (root / "sinks.txt").write_text(sinks)
        (root / "sources.txt").write_text(sources)
        loaded = [line.split()[1] for line in modules.splitlines() if line.split()]
        if len(loaded) != 3 or set(loaded) != required:
            raise RuntimeError("unexpected module loaded; refusing a server that could expose hardware")
        if ([line.split()[1] for line in sinks.splitlines()] != ["eva_test_output"]
                or {line.split()[1] for line in sources.splitlines()}
                != {"eva_test_output.monitor", "eva_test_input"}):
            raise RuntimeError("unexpected sink/source; refusing non-synthetic audio endpoints")
        report["modules"] = loaded
        report["before_streams"] = streams()
        if any(report["before_streams"].values()):
            raise RuntimeError("private server already has a client audio stream")
        if binary is not None:
            listing = subprocess.run([str(binary), "audio::pulse", "--ignored", "--list", "--color=never"],
                                     env=env, check=True, capture_output=True, text=True, timeout=10).stdout
            tests = [line for line in listing.splitlines() if line.endswith(": test")]
            if not tests:
                raise RuntimeError("test executable has no ignored audio::pulse tests; zero tests is not a pass")
            report["tests_listed"] = tests
            print("Running " + str(len(tests)) + " ignored PulseAudio tests against synthetic endpoints", flush=True)
            with (root / "tests.log").open("w") as output:
                completed = subprocess.run([str(binary), "audio::pulse", "--ignored", "--test-threads=1", "--nocapture", "--color=never"],
                                           env=env, stdout=output, stderr=subprocess.STDOUT, timeout=90)
            text = (root / "tests.log").read_text()
            print(text, end="", flush=True)
            counts = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", text)
            if (completed.returncode != 0 or not counts or int(counts[-1][0]) == 0
                    or counts[-1][1:] != ("0", "0") or int(counts[-1][0]) != len(tests)):
                raise RuntimeError("native PulseAudio tests failed or did not execute every listed ignored test")
            report["passed"] = int(counts[-1][0])
        deadline = time.monotonic() + 3
        while True:
            after = streams()
            if not any(after.values()):
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("client streams remain after native capture/playback teardown")
            time.sleep(0.02)
        report["after_streams"] = after
        report["result"] = "fixture_verified" if args.fixture_only else "passed"
        print("Private PulseAudio fixture verified; zero remaining sink-inputs/source-outputs"
              + (" (Rust tests intentionally not run)" if args.fixture_only else ""), flush=True)
    except (Exception, KeyboardInterrupt) as error:
        result = (128 + error.signum if isinstance(error, Interrupted)
                  else 130 if isinstance(error, KeyboardInterrupt) else 1)
        report["result"] = "failed"
        report["error"] = str(error) or "interrupted"
        print("native PulseAudio test failed: " + report["error"], file=sys.stderr)
    finally:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        # Popen owns this exact child. Never use pulseaudio --kill, pactl exit,
        # systemctl or a PID discovered from the user's session.
        if server is not None:
            stopped = time.monotonic()
            if server.poll() is None:
                server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=3)
                result = 1
                report["result"] = "failed"
                report["error"] = "private PulseAudio server required forced termination"
            report["server_exit_code"] = server.returncode
            report["server_stop_ms"] = round((time.monotonic() - stopped) * 1000, 3)
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        keep = os.environ.get("JWM_EVA_KEEP_PULSE_TEST", os.environ.get("JWM_EVA_KEEP_AUDIO_TEST", "0")) == "1"
        if result != 0 or keep:
            print("native PulseAudio test artifacts: " + str(root), file=sys.stderr)
        else:
            shutil.rmtree(root)
    return result


raise SystemExit(main())
PY
