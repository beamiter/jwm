#!/usr/bin/env python3
"""Offline native-CI acquisition contracts; no package installation/network."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("prefetch", ROOT / "scripts/ci-prefetch-cjk.py")
prefetch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prefetch)
DATA = b"synthetic archive bytes"
NAME = "fonts-noto-cjk_1%3a20230817+repack1-3_all.deb"
HASH = "SHA256:" + hashlib.sha256(DATA).hexdigest()
URL = "http://us-east-2.ec2.archive.ubuntu.com/ubuntu/pool/main/f/fonts-noto-cjk/" + "fonts-noto-cjk_20230817%2brepack1-3_all.deb"
RECORD = f"'{URL}' {NAME} {len(DATA)} {HASH}\n"


class PrefetchTests(unittest.TestCase):
    def test_same_signed_index_archive_and_hash(self):
        name, size, digest, primary, url = prefetch.archive_record(RECORD)
        self.assertEqual(primary, URL)
        self.assertEqual((name, size, digest), (NAME, len(DATA), HASH))
        self.assertEqual(url, URL.replace("http://us-east-2.ec2.archive.ubuntu.com", "https://archive.ubuntu.com"))

    def test_real_apt_epoch_and_encoded_archive_path(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            lists = folder / "lists"
            lists.mkdir()
            source = folder / "ubuntu.list"
            source.write_text("deb http://official.invalid/ubuntu noble main\n")
            (folder / "status").write_text("")
            config = folder / "apt.conf"
            config.write_text('Dir::Etc::parts "-";\nDir::Etc::main "-";\n')
            index = lists / "official.invalid_ubuntu_dists_noble_main_binary-amd64_Packages"
            index.write_text(
                "Package: fonts-noto-cjk\nVersion: 1:20230817+repack1-3\n"
                "Architecture: all\nDescription: offline test fixture\n"
                "Filename: pool/main/f/fonts-noto-cjk/fonts-noto-cjk_20230817+repack1-3_all.deb\n"
                f"Size: {len(DATA)}\nSHA256: {HASH.removeprefix('SHA256:')}\n"
                f"SHA512: {hashlib.sha512(DATA).hexdigest()}\n\n"
            )
            command = [
                "apt-get", "-o", f"Dir::Etc::sourcelist={source}",
                "-o", "Dir::Etc::sourceparts=-", "-o", f"Dir::State::lists={lists}",
                "-o", f"Dir::State::status={folder / 'status'}",
                "-o", "Dir::Cache::pkgcache=", "-o", "Dir::Cache::srcpkgcache=",
                "-o", "APT::Architecture=amd64", "--print-uris", "download", "fonts-noto-cjk",
            ]
            run_options = dict(cwd=folder, env=dict(os.environ, APT_CONFIG=str(config), LC_ALL="C"),
                               capture_output=True, text=True, timeout=5, check=True)
            default = subprocess.run(command, **run_options)
            self.assertIn("SHA512:", default.stdout)
            with self.assertRaises(ValueError):
                prefetch.archive_record(default.stdout)
            result = subprocess.run(command + ["-o", "Acquire::ForceHash=SHA256"], **run_options)
            name, size, digest, primary, fallback = prefetch.archive_record(result.stdout)
            self.assertEqual(name, NAME)
            self.assertEqual((size, digest), (len(DATA), HASH))
            self.assertTrue(primary.endswith("fonts-noto-cjk_20230817%2brepack1-3_all.deb"))
            self.assertEqual(fallback, primary.replace("http://official.invalid", "https://archive.ubuntu.com"))
            # Forcing SHA256 must not silently fall back to SHA512 or MD5 when
            # the authenticated record lacks the requested strong digest.
            index.write_text("\n".join(line for line in index.read_text().splitlines()
                                       if not line.startswith("SHA256:")) + "\n")
            missing = subprocess.run(command + ["-o", "Acquire::ForceHash=SHA256"],
                                     **dict(run_options, check=False))
            if missing.returncode == 0:
                with self.assertRaises(ValueError):
                    prefetch.archive_record(missing.stdout)


    def test_malformed_or_weak_metadata_fails_closed(self):
        for bad in ("", RECORD + RECORD, RECORD.replace("SHA256:", "MD5Sum:"),
                    RECORD.replace("fonts-noto-cjk_", "other_"), RECORD.replace("/pool/", "/../"),
                    RECORD.replace(f" {len(DATA)} ", " 0 ", 1)):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                prefetch.archive_record(bad)

    def exercise(self, statuses):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            calls = []
            def acquire(command, *, cwd):
                calls.append(command)
                status = statuses[len(calls) - 1]
                if not status:
                    target = Path(command[-2])
                    target.write_bytes(DATA)
                return status
            selected = subprocess.CompletedProcess([], 0, RECORD, "")
            with patch.object(prefetch, "CACHE", cache), patch.object(prefetch, "bounded", acquire), \
                 patch.object(prefetch.subprocess, "run", return_value=selected) as select:
                if statuses[-1]:
                    with self.assertRaises(RuntimeError):
                        prefetch.prefetch(["-o", "Dir::Etc::sourceparts=-"])
                    self.assertFalse((cache / NAME).exists())
                else:
                    prefetch.prefetch(["-o", "Dir::Etc::sourceparts=-"])
                    self.assertEqual((cache / NAME).read_bytes(), DATA)
                self.assertEqual(select.call_count, 1)  # no second index/version selection
                self.assertIn("Acquire::ForceHash=SHA256", select.call_args.args[0])
            return calls

    def test_primary_success_does_not_use_fallback(self):
        calls = self.exercise([0, 0])
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0][-3], URL)
        self.assertEqual(calls[0][-1], HASH)
        self.assertTrue(calls[1][-3].startswith("file:"))

    def test_timeout_or_failure_uses_exact_fallback_and_digest(self):
        for status in (124, 100):
            calls = self.exercise([status, 0, 0])
            self.assertEqual(calls[1][-3], prefetch.archive_record(RECORD)[4])
            self.assertEqual(calls[1][-1], HASH)
            self.assertTrue(all("install" not in call for call in calls))

    def test_both_mirrors_failed_never_populates_cache(self):
        self.exercise([124, 100])

    def test_hash_verification_failure_never_populates_cache(self):
        self.exercise([0, 100])

    def test_wall_clock_limit_interrupts_continuous_output(self):
        with tempfile.TemporaryDirectory() as directory:
            start = time.monotonic()
            status = prefetch.bounded([sys.executable, "-c", "import time\nwhile True:\n print('.', flush=True); time.sleep(.03)"],
                                      cwd=directory, seconds=.15)
            self.assertEqual(status, 124)
            self.assertLess(time.monotonic() - start, 2)

    def test_real_apt_helper_rejects_wrong_hash_without_network(self):
        with tempfile.TemporaryDirectory() as directory:
            source, target = Path(directory) / "source", Path(directory) / "target"
            source.write_bytes(DATA)
            for digest, success in ((HASH, True), ("SHA256:" + "0" * 64, False)):
                target.unlink(missing_ok=True)
                result = subprocess.run([prefetch.APT_HELPER, "download-file", source.as_uri(), str(target), digest],
                                        text=True, capture_output=True, timeout=5)
                self.assertEqual(result.returncode == 0, success, result.stderr)

    def test_opt_in_prefetch_is_after_strict_update_and_before_install(self):
        for prefetch_status in (0, 1):
            with tempfile.TemporaryDirectory() as directory:
                folder = Path(directory)
                source = folder / "ubuntu.sources"
                source.write_text("fixture")
                log = folder / "calls"
                sudo = folder / "sudo"
                sudo.write_text(
                    "#!/usr/bin/python3\nimport json, os, sys\n"
                    "with open(os.environ['LOG'], 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                    "sys.exit(int(os.environ['FAIL']) if sys.argv[1] == '/usr/bin/python3' else 0)\n"
                )
                sudo.chmod(0o755)
                env = dict(os.environ, PATH=str(folder) + ":/usr/bin:/bin", LOG=str(log),
                           FAIL=str(prefetch_status), JWM_CI_APT_SOURCES=str(source), JWM_CI_PREFETCH_CJK="1")
                result = subprocess.run(["/bin/bash", str(ROOT / "scripts/ci-install-deps.sh"), "fonts-noto-cjk", "grim"],
                                        env=env, capture_output=True, text=True, timeout=5)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual(result.returncode, prefetch_status, result.stderr)
                self.assertEqual(calls[0][-1], "update")
                self.assertIn("APT::Update::Error-Mode=any", calls[0])
                self.assertEqual(calls[1][0], "/usr/bin/python3")
                self.assertTrue(calls[1][1].endswith("/ci-prefetch-cjk.py"))
                self.assertEqual(calls[1][2:], calls[0][1:-3])
                self.assertEqual(len(calls), 3 if prefetch_status == 0 else 2)
                if not prefetch_status:
                    self.assertEqual(calls[2][-5:], ["install", "-y", "--no-install-recommends", "fonts-noto-cjk", "grim"])

    def test_only_native_job_opts_in_with_original_step_deadline(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertEqual(workflow.count('JWM_CI_PREFETCH_CJK: "1"'), 1)
        self.assertIn('timeout-minutes: 10\n        env:\n          JWM_CI_PREFETCH_CJK: "1"', workflow)
        self.assertIn("fonts-noto-cjk sway wtype grim", workflow)


if __name__ == "__main__":
    unittest.main()
