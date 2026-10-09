#!/usr/bin/env python3
"""Exercise actual showcase function definitions without starting JWM/Julia."""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT = Path(sys.argv.pop(1)).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parent / "record_waterlily_showcase.sh"
SOURCE = SCRIPT.read_text()


def section(start, end):
    return SOURCE[SOURCE.index(start):SOURCE.index(end, SOURCE.index(start))]


class ShowcaseTests(unittest.TestCase):
    def run_shell(self, body, *, lifecycle=True, palette=False):
        definitions = section("worker_pids()", "# 唯一可信") if lifecycle else ""
        if palette:
            definitions += section("RECORDING=0", "trap cleanup EXIT")
        program = "set -euo pipefail\nlog() { :; }\n" + definitions + "\n" + body
        result = subprocess.run(["bash", "-c", program], text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def test_unknown_or_mismatched_identity_never_signals_or_waits(self):
        for state in ("unknown", "mismatch"):
            with self.subTest(state=state):
                self.run_shell("""
SPAWNED_WORKER_PID=12345
SPAWNED_WORKER_START=1
owned_worker_state() { echo STATE; }
kill() { echo unsafe-signal; exit 91; }
wait() { echo unsafe-wait; exit 92; }
if kill_owned_worker; then exit 93; fi
[[ $SPAWNED_WORKER_PID == 12345 ]]
""".replace("STATE", state))

    def test_exited_or_gone_child_is_reaped_and_cleared(self):
        for state in ("gone", "exited"):
            with self.subTest(state=state):
                output = self.run_shell("""
SPAWNED_WORKER_PID=12345
SPAWNED_WORKER_START=1
owned_worker_state() { echo STATE; }
kill() { echo unsafe-signal; exit 91; }
wait() { echo REAPED; }
kill_owned_worker
[[ -z $SPAWNED_WORKER_PID && -z $SPAWNED_WORKER_START ]]
""".replace("STATE", state))
                self.assertEqual(output.strip(), "REAPED")

    def test_stubborn_child_stops_polling_and_never_waits(self):
        output = self.run_shell("""
SPAWNED_WORKER_PID=12345
SPAWNED_WORKER_START=1
owned_worker_state() { echo live; }
kill() { echo "SIGNAL $*"; }
sleep() { :; }
wait() { echo unsafe-wait; exit 92; }
if kill_owned_worker; then exit 93; fi
[[ $SPAWNED_WORKER_PID == 12345 ]]
""")
        self.assertEqual(output.splitlines(), ["SIGNAL -- 12345", "SIGNAL -9 -- 12345"])

    @unittest.skipUnless(Path("/proc/self/stat").exists(), "Linux process identity regression")
    def test_only_own_child_can_be_stopped(self):
        self.run_shell("""
sleep 30 &
SPAWNED_WORKER_PID=$!
trap 'kill "$SPAWNED_WORKER_PID" 2>/dev/null || true; wait "$SPAWNED_WORKER_PID" 2>/dev/null || true' EXIT
SPAWNED_WORKER_START=$(worker_identity "$SPAWNED_WORKER_PID")
[[ $(owned_worker_state) == live ]]
kill_owned_worker
[[ -z $SPAWNED_WORKER_PID ]]
trap - EXIT
""")

    def test_palette_is_restored_only_after_change(self):
        output = self.run_shell("""
SPAWNED_WORKER=0
ORIGINAL_PALETTE_JSON='"mica"'
ipc() { printf '%s\n' "$*"; }
cleanup
set -e
set_palette '"sith"'
cleanup
:
""", lifecycle=False, palette=True)
        self.assertEqual(output.splitlines(), [
            'waterlily_palette --args "sith"',
            'waterlily_palette --args "mica"',
        ])

    def test_recording_path_is_json_encoded(self):
        line = next(line for line in SOURCE.splitlines() if line.startswith("ipc start_recording --args"))
        program = "set -euo pipefail\nipc() { printf '%s' \"$3\"; }\n" + line
        path = '/tmp/quote" backslash\\ and\nnewline.mp4'
        result = subprocess.run(["bash", "-c", program], env=dict(os.environ, OUT_FILE=path), text=True, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"path": path})

    def test_existing_worker_is_not_killed(self):
        body = section("ensure_worker()", "RECORDING=0")
        result = subprocess.run(["bash", "-c",
            "set -euo pipefail\nlog() { :; }; ipc() { :; }; waterlily_flag() { return 1; }; "
            "worker_pids() { echo 12345; }; wait_for_worker() { return 1; }; "
            "kill() { echo unsafe-signal; exit 91; }; STALE_GRACE=0;\n" + body + "\nensure_worker"],
            text=True, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("unsafe-signal", result.stdout)
        self.assertIn("refusing to stop an unowned process", result.stderr)


if __name__ == "__main__":
    unittest.main()
