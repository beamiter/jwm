#!/usr/bin/env python3
"""Offline WaterLily helper checks: no Julia, JWM, or active desktop needed."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("waterlily.sh")


class WaterLilyHelperTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="jwm-waterlily-helper-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.log = self.root / "calls.jsonl"
        stub = self.root / "executable with spaces"
        stub.write_text("""#!/usr/bin/env python3
import json, os, sys
with open(os.environ['CALL_LOG'], 'a') as stream:
    stream.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1:2] == ['msg']:
    if sys.argv[2] == 'get_waterlily_status':
        print(os.environ['STATUS_JSON'])
        sys.exit(int(os.environ.get('QUERY_EXIT', '0')))
    sys.exit(int(os.environ.get('CONTROL_EXIT', '0')))
sys.exit(int(os.environ.get('JULIA_EXIT', '0')))
""")
        stub.chmod(0o700)
        self.env = dict(os.environ, JWM_TOOL=str(stub), JULIA=str(stub),
                        CALL_LOG=str(self.log))

    def run_helper(self, *args, enabled=False, connected=False, status=None, **env):
        self.env["STATUS_JSON"] = json.dumps(status if status is not None else {
            "success": True, "data": {"enabled": enabled, "worker_connected": connected}
        })
        return subprocess.run(["bash", str(SCRIPT), *args],
                              env=dict(self.env, **env), text=True,
                              capture_output=True, timeout=10)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_help_and_invalid_arguments_have_no_side_effects(self):
        self.assertEqual(self.run_helper("--help").returncode, 0)
        for args in [("unknown",), ("stop", "extra"), ("case", 'bad"name'),
                     ("palette", "a", "b"), ("start", "--fps"),
                     ("start", "--unexpected", "value")]:
            with self.subTest(args=args):
                self.assertNotEqual(self.run_helper(*args).returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_default_start_enables_and_executes_foreground_worker(self):
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(calls[:2], [["msg", "get_waterlily_status", "--raw"],
                                     ["msg", "toggle_waterlily"]])
        self.assertEqual(calls[2], [f"--project={SCRIPT.parent.parent / 'waterlily'}",
                                    "--threads=auto", str(SCRIPT.parent.parent / "waterlily/runner.jl"),
                                    "--device", "auto"])

    def test_runner_arguments_and_project_preserve_boundaries(self):
        project = "/tmp/project space"
        result = self.run_helper("start", "--case", "dance", "--socket", "/tmp/socket space",
                                 enabled=True, JWM_WATERLILY_PROJECT=project)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.calls()), 2)
        self.assertEqual(self.calls()[-1][0], f"--project={project}")
        self.assertEqual(self.calls()[-1][-4:], ["--case", "dance", "--socket", "/tmp/socket space"])

    def test_start_reuses_connected_worker_idempotently(self):
        for enabled in [False, True]:
            with self.subTest(enabled=enabled):
                self.log.unlink(missing_ok=True)
                result = self.run_helper("start", enabled=enabled, connected=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(self.calls()), 1 if enabled else 2)
                self.assertIn("Reusing", result.stdout)

    def test_connected_worker_options_are_rejected_before_toggle(self):
        result = self.run_helper("start", "--case", "dance", connected=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.calls()), 1)

    def test_stop_only_hides_enabled_layer(self):
        for enabled in [False, True]:
            with self.subTest(enabled=enabled):
                self.log.unlink(missing_ok=True)
                result = self.run_helper("stop", enabled=enabled, connected=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(self.calls()), 2 if enabled else 1)
                self.assertIn("Ctrl+C", result.stdout)

    def test_case_and_palette_controls(self):
        for action, name in [("case", "dance"), ("palette", "fluent"), ("case", None)]:
            with self.subTest(action=action, name=name):
                self.log.unlink(missing_ok=True)
                args = [action] + ([name] if name else [])
                result = self.run_helper(*args, connected=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.calls()[-1], ["msg", f"waterlily_{action}",
                                                     "--args", json.dumps(name or "next")])

    def test_controls_require_connected_worker(self):
        self.assertNotEqual(self.run_helper("case", "dance").returncode, 0)
        self.assertEqual(len(self.calls()), 1)

    def test_status_prints_actual_reply(self):
        result = self.run_helper("status", enabled=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["data"]["enabled"], True)
        self.assertEqual(len(self.calls()), 1)

    def test_query_failures_and_malformed_flags_never_toggle_or_spawn(self):
        for status in [{"success": False, "error": "unsupported on Wayland"},
                       {"success": True, "data": {"enabled": "false", "worker_connected": False}},
                       {"success": True, "data": {}}, []]:
            with self.subTest(status=status):
                self.log.unlink(missing_ok=True)
                result = self.run_helper("start", status=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(self.calls()), 1)
        self.log.unlink()
        self.assertNotEqual(self.run_helper("start", QUERY_EXIT="7").returncode, 0)
        self.assertEqual(len(self.calls()), 1)

    def test_missing_julia_does_not_toggle(self):
        result = self.run_helper("start", JULIA=str(self.root / "missing"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.calls()), 1)

    def test_control_and_worker_exit_codes_propagate(self):
        self.assertEqual(self.run_helper("start", CONTROL_EXIT="9").returncode, 9)
        self.log.unlink()
        self.assertEqual(self.run_helper("start", JULIA_EXIT="17").returncode, 17)


if __name__ == "__main__":
    unittest.main()
