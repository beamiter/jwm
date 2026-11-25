#!/usr/bin/env python3
"""Install only into private temporary homes; never execute installed TSX code."""
import os
import contextlib
import io
from types import SimpleNamespace
from pathlib import Path
import subprocess
import tempfile
import unittest
import unittest.mock

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / 'scripts/jwm-tsx-preview.mjs').read_bytes()

class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='jwm-tsx-install-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.target = self.home / '.local/bin/jwm-tsx-preview'

    def install(self, **overrides):
        return subprocess.run(['bash', str(ROOT / 'scripts/install-tsx-preview.sh')],
                              env={**os.environ, "HOME": str(self.home), **overrides},
                              capture_output=True, text=True, timeout=10)

    def test_clean_and_stale_upgrade_are_exact_and_do_not_execute(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.target.read_bytes(), SOURCE)
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o755)
        sentinel = self.home / 'must-not-run'
        old = f'#!/bin/sh\ntouch "{sentinel}"\n'.encode()
        self.target.write_bytes(old)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.target.read_bytes(), SOURCE)
        self.assertFalse(sentinel.exists())
        self.assertFalse(list(self.target.parent.glob('.jwm-tsx-preview.*')))
        self.assertIn('Installation grants no project trust', result.stdout)
        # Reversal is just restoring the caller's preserved old file, not an
        # automatic downgrade or launcher execution.
        self.target.write_bytes(old)
        self.assertEqual(self.target.read_bytes(), old)

    def test_path_node_is_never_executed_or_used_as_fallback(self):
        fake_bin = self.home / 'fake-bin'; fake_bin.mkdir()
        sentinel = self.home / 'fake-node-executed'
        fake = fake_bin / 'node'
        fake.write_text(f'#!/bin/sh\ntouch "{sentinel}"\n')
        fake.chmod(0o755)
        with unittest.mock.patch.dict(os.environ, PATH=str(fake_bin) + os.pathsep + os.environ['PATH']):
            result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(sentinel.exists())
        try:
            node = Path('/usr/bin/node').resolve(strict=True)
            trusted = node.is_relative_to(Path('/usr/bin')) and node.is_file()
            trusted = trusted and bool(node.stat().st_mode & 0o111)
            trusted = trusted and all(p.stat().st_uid == 0 and not p.stat().st_mode & 0o022
                                      for p in (node, *node.parents))
        except OSError:
            trusted = False
        if os.environ.get('JWM_TEST_REQUIRE_TRUSTED_NODE') == '1':
            self.assertTrue(trusted, 'CI requires a real trusted /usr/bin/node; PATH is not accepted')
        expected = 'System Node path meets' if trusted else 'preflight is NOT ready'
        rejected = 'preflight is NOT ready' if trusted else 'System Node path meets'
        self.assertIn(expected, result.stdout)
        self.assertNotIn(rejected, result.stdout)

    def test_status_branches_deterministically_without_changing_system_node(self):
        # Execute the exact metadata-only diagnostic with mocked filesystem
        # metadata. This is not evidence of a real Node installation.
        script = (ROOT / 'scripts/install-tsx-preview.sh').read_text()
        diagnostic = script.rsplit("python3 - <<'CHECK'\n", 1)[1].split('\nCHECK')[0]
        for state in ('trusted', 'missing', 'writable', 'nonroot', 'outside'):
            with self.subTest(state=state):
                node = unittest.mock.Mock()
                node.resolve.return_value = node
                node.is_relative_to.return_value = state != 'outside'
                node.is_file.return_value = True
                node.parents = ()
                node.stat.return_value = SimpleNamespace(
                    st_uid=1000 if state == 'nonroot' else 0,
                    st_mode=0o100777 if state == 'writable' else 0o100755)
                if state == 'missing':
                    node.resolve.side_effect = FileNotFoundError()
                output = io.StringIO()
                with unittest.mock.patch('pathlib.Path', return_value=node), contextlib.redirect_stdout(output):
                    exec(compile(diagnostic, 'installer-node-diagnostic', 'exec'), {})
                expected = 'System Node path meets' if state == 'trusted' else 'preflight is NOT ready'
                rejected = 'preflight is NOT ready' if state == 'trusted' else 'System Node path meets'
                self.assertIn(expected, output.getvalue())
                self.assertNotIn(rejected, output.getvalue())

    def test_failed_copy_preserves_old_bytes_and_removes_temporary_file(self):
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(b'old launcher')
        self.target.chmod(0o755)
        fake_bin = self.home / 'fake-bin'; fake_bin.mkdir()
        fake = fake_bin / 'install'
        fake.write_text('#!/bin/sh\nfor last; do :; done\nprintf partial > "$last"\nexit 77\n')
        fake.chmod(0o755)
        result = self.install(PATH=str(fake_bin) + os.pathsep + os.environ['PATH'])
        self.assertEqual(result.returncode, 77)
        self.assertEqual(self.target.read_bytes(), b'old launcher')
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o755)
        self.assertFalse(list(self.target.parent.glob('.jwm-tsx-preview.*')))
        self.assertNotIn('Installed ', result.stdout)

    def test_writable_existing_launcher_is_not_repaired(self):
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(b'old launcher')
        self.target.chmod(0o777)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.target.read_bytes(), b'old launcher')
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o777)
        self.assertFalse(list(self.target.parent.glob('.jwm-tsx-preview.*')))

    def test_home_symlink_uses_canonical_destination_and_checks_target(self):
        link = self.home / 'alias'
        real = self.home / 'real'; real.mkdir()
        link.symlink_to(real, target_is_directory=True)
        result = self.install(HOME=str(link))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((real / '.local/bin/jwm-tsx-preview').read_bytes(), SOURCE)
        self.assertIn(str(real / '.local/bin/jwm-tsx-preview'), result.stdout)
        self.assertNotIn(str(link), result.stdout)
        real.chmod(0o777)
        result = self.install(HOME=str(link))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(real.stat().st_mode & 0o777, 0o777)
        self.assertEqual((real / '.local/bin/jwm-tsx-preview').read_bytes(), SOURCE)

    def test_unsafe_directory_is_not_repaired(self):
        self.home.chmod(0o777)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.target.exists())
        self.assertEqual(self.home.stat().st_mode & 0o777, 0o777)

    def test_symlink_directory_is_not_followed(self):
        outside = self.home / 'outside'; outside.mkdir()
        (self.home / '.local').symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_special_existing_targets_are_not_replaced(self):
        self.target.parent.mkdir(parents=True)
        for kind in ('symlink', 'fifo', 'directory'):
            with self.subTest(kind=kind):
                if kind == 'symlink': self.target.symlink_to(self.home / 'absent')
                elif kind == 'fifo': os.mkfifo(self.target)
                else: self.target.mkdir()
                self.assertNotEqual(self.install().returncode, 0)
                if kind == 'directory': self.target.rmdir()
                else: self.target.unlink()

if __name__ == '__main__':
    unittest.main()
