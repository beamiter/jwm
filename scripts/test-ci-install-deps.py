#!/usr/bin/env python3
"""Offline CI dependency contract: never invokes sudo, apt, or networking."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts/ci-install-deps.sh"
WORKFLOWS = ("ci.yml", "remote-wayland-safety.yml", "remote-window-safety.yml",
             "remote-popup-qt.yml")


class DependencyInstallTests(unittest.TestCase):
    def invoke(self, packages=("libx11-dev", "pkg-config"), update=0, install=0):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            log = folder / "calls.jsonl"
            fake = folder / "sudo"
            fake.write_text(
                "#!/usr/bin/python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['FAKE_APT_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "assert args[0] == 'apt-get'\n"
                "phase = 'UPDATE' if args[-1] == 'update' else 'INSTALL'\n"
                "sys.exit(int(os.environ['FAKE_' + phase + '_STATUS']))\n"
            )
            fake.chmod(0o755)
            env = dict(os.environ, PATH=str(folder), FAKE_APT_LOG=str(log),
                       FAKE_UPDATE_STATUS=str(update), FAKE_INSTALL_STATUS=str(install))
            result = subprocess.run(["/bin/bash", str(HELPER), *packages], env=env,
                                    text=True, capture_output=True, timeout=5, check=False)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result, calls

    def test_success_uses_strict_update_and_bounded_official_source_commands(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        options = ["apt-get", "-o", "Acquire::http::Timeout=30", "-o",
                   "Acquire::https::Timeout=30", "-o", "Acquire::Retries=2",
                   "-o", "DPkg::Lock::Timeout=60"]
        self.assertEqual(calls[0], options + ["-o", "APT::Update::Error-Mode=any", "update"])
        self.assertEqual(calls[1], options + ["install", "-y", "--no-install-recommends",
                                            "libx11-dev", "pkg-config"])

    def test_update_failure_is_preserved_and_install_is_not_attempted(self):
        result, calls = self.invoke(update=100)
        self.assertEqual(result.returncode, 100)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][-1], "update")

    def test_install_failure_is_preserved_without_outer_retry(self):
        result, calls = self.invoke(install=100)
        self.assertEqual(result.returncode, 100)
        self.assertEqual(len(calls), 2)

    def test_empty_package_list_fails_before_any_command(self):
        result, calls = self.invoke(packages=())
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_all_workflow_installs_have_step_deadlines_and_no_bypass(self):
        count = 0
        for name in WORKFLOWS:
            text = (ROOT / ".github/workflows" / name).read_text()
            self.assertNotIn("apt-get", text, name)
            steps = re.split(r"(?m)^[ \t]*-[ \t]+(?=name:|uses:)", text)
            installs = [step for step in steps if "scripts/ci-install-deps.sh" in step]
            self.assertTrue(installs, name)
            for step in installs:
                self.assertRegex(step, r"(?m)^[ \t]+timeout-minutes:[ \t]+10[ \t]*$")
                self.assertNotIn("continue-on-error", step)
                self.assertNotIn("|| true", step)
                self.assertNotIn("cargo test", step)  # install timeout must not truncate tests
                count += 1
        self.assertEqual(count, 14)


class QtProbeContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = (ROOT / ".github/workflows/remote-popup-qt.yml").read_text()
        lines = cls.workflow.splitlines()
        blocks = []
        for index, line in enumerate(lines):
            match = re.match(r"^([ \t]*)run:[ \t]*\|[ \t]*$", line)
            if not match:
                continue
            indent = len(match.group(1))
            body = []
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) <= indent:
                    break
                body.append(following)
            script = textwrap.dedent("\n".join(body))
            if "test_name=" in script and "--ignored" in script:
                blocks.append(script)
        if len(blocks) != 1:
            raise AssertionError("Qt workflow must have one mandatory exact-test block")
        cls.script = blocks[0]

    def run_contract(self, listed=1, status=0,
                     summary="test result: ok. 1 passed; 0 failed; 0 ignored;", list_status=0):
        # Execute only the workflow's control/validation shell with fake cargo.
        # No compilation, display server, native app, or network is started.
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            binary = folder / "bin"
            binary.mkdir()
            log = folder / "calls.jsonl"
            cargo = binary / "cargo"
            cargo.write_text(
                "#!/usr/bin/python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['QT_FAKE_CALLS'], 'a') as f:\n"
                "    f.write(json.dumps({'args': args, 'display': os.getenv('DISPLAY'), "
                "'wayland': os.getenv('WAYLAND_DISPLAY')}) + '\\n')\n"
                "if '--list' in args:\n"
                "    for i in range(int(os.environ['QT_FAKE_LIST'])): print('fixture_' + str(i) + ': test')\n"
                "    sys.exit(int(os.environ['QT_FAKE_LIST_STATUS']))\n"
                "print('PRIVATE_SYNTHETIC_WIRE_LOG_DO_NOT_PUBLISH')\n"
                "print('JWM_QT_STAGE=root_mapped')\n"
                "print('JWM_QT_STAGE=PRIVATE_SYNTHETIC_UNKNOWN_STAGE')\n"
                "print(os.environ['QT_FAKE_SUMMARY'])\n"
                "sys.exit(int(os.environ['QT_FAKE_STATUS']))\n"
            )
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(binary) + ":/usr/bin:/bin", RUNNER_TEMP=str(folder),
                       QT_FAKE_CALLS=str(log), QT_FAKE_LIST=str(listed),
                       QT_FAKE_LIST_STATUS=str(list_status), QT_FAKE_STATUS=str(status),
                       QT_FAKE_SUMMARY=summary, DISPLAY="must_be_unset", WAYLAND_DISPLAY="must_be_unset")
            result = subprocess.run(["/bin/bash", "-c", self.script], env=env, text=True,
                                    capture_output=True, timeout=5, check=False)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            self.assertNotIn("PRIVATE_SYNTHETIC", result.stdout + result.stderr)
            return result, calls

    def test_native_wayland_and_mandatory_software_gl_contract(self):
        for key, value in (("EGL_PLATFORM", "surfaceless"), ("LIBGL_ALWAYS_SOFTWARE", "1"),
                           ("JWM_REQUIRE_HEADLESS_GL", "1")):
            self.assertRegex(self.workflow, rf"(?m)^[ \t]*{key}:[ \t]*['\"]?{value}['\"]?[ \t]*$")
        fixture = (ROOT / "src/backend/wayland_udev/remote_popup_qt_tests.rs").read_text()
        self.assertRegex(fixture, r'\.env\(\s*"QT_QPA_PLATFORM"\s*,\s*"wayland"\s*\)')
        self.assertRegex(fixture, r'HeadlessSmithayGles::new\(\)\.expect\(')
        self.assertNotIn("upload-artifact", self.workflow)
        self.assertNotIn("continue-on-error", self.workflow)

    def test_exact_ignored_test_runs_once_and_raw_output_stays_private(self):
        result, calls = self.run_contract()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        exact_name = "backend::wayland_udev::remote_popup::qt_tests::actual_qt_dynamic_private_seat_menu_selection_and_terminal_retirement"
        for call in calls:
            self.assertIn(exact_name, call["args"])
            self.assertIn("--ignored", call["args"])
            self.assertIn("--exact", call["args"])
            self.assertIn("remote-wayland,backend-wayland-udev", call["args"])
        self.assertIn("--list", calls[0]["args"])
        self.assertNotIn("--list", calls[1]["args"])
        self.assertIn("--test-threads=1", calls[1]["args"])
        self.assertIsNone(calls[1]["display"])
        self.assertIsNone(calls[1]["wayland"])
        self.assertIn("qt_stage=root_mapped", result.stdout)
        self.assertIn("qt_exact_test_passed=true", result.stdout)

    def test_missing_duplicate_or_failed_listing_never_executes_test(self):
        for listed, status in ((0, 0), (2, 0), (1, 7)):
            with self.subTest(listed=listed, status=status):
                result, calls = self.run_contract(listed=listed, list_status=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 1)

    def test_nonzero_execution_fails_even_with_success_summary(self):
        result, calls = self.run_contract(status=9)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)

    def test_zero_exit_requires_one_pass_and_zero_ignored(self):
        for summary in ("test result: ok. 0 passed; 0 failed; 0 ignored;",
                        "test result: ok. 1 passed; 0 failed; 1 ignored;", ""):
            with self.subTest(summary=summary):
                result, calls = self.run_contract(summary=summary)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()
