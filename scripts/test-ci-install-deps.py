#!/usr/bin/env python3
"""Offline CI dependency contract: never invokes sudo, apt, or networking."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts/ci-install-deps.sh"
WORKFLOWS = ("ci.yml", "remote-wayland-safety.yml", "remote-window-safety.yml")


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
            steps = re.split(r"(?m)^      - ", text)
            installs = [step for step in steps if "scripts/ci-install-deps.sh" in step]
            self.assertTrue(installs, name)
            for step in installs:
                self.assertRegex(step, r"(?m)^        timeout-minutes: 10$")
                self.assertNotIn("continue-on-error", step)
                self.assertNotIn("|| true", step)
                self.assertNotIn("cargo test", step)  # install timeout must not truncate tests
                count += 1
        self.assertEqual(count, 13)


if __name__ == "__main__":
    unittest.main()
