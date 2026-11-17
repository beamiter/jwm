#!/usr/bin/env python3
"""Execute the exact repeat workflow shell with a stub fixture, never a display."""
from pathlib import Path
import os
import re
import subprocess
import tempfile
import unittest

SOURCE = (Path(__file__).resolve().parents[1] / '.github/workflows/shared-shell.yml').read_text()
BLOCK = SOURCE.split('      - name: Repeat original React acceptance without intervention\n', 1)[1].split('      - name:', 1)[0]
SCRIPT = '\n'.join(line[10:] for line in BLOCK.split('        run: |\n', 1)[1].splitlines())

class RepeatWorkflowTests(unittest.TestCase):
    def test_real_shell_keeps_first_failure_after_all_twenty_four(self):
        for fail_first in ('0', '1'):
            with self.subTest(fail_first=fail_first), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name in ('frontend', 'target/release/jwm', 'target/release/jwm-tool', 'crates/jwm_shell/target/debug/jwm-shell'):
                    p = root / name
                    p.parent.mkdir(parents=True, exist_ok=True)
                    p.write_text(name)
                (root / 'artifacts/shared-shell').mkdir(parents=True)
                (root / 'bin').mkdir()
                stub = root / 'bin/python3'
                stub.write_text('''#!/bin/sh
if [ "$1" = -c ]; then echo frontend; exit 0; fi
case "$*" in *--configure-notify-probe*) exit 99;; esac
printf '%s\\n' "$*" >> calls
n=$(wc -l < calls)
if [ "$FAIL_FIRST" = 1 ] && [ "$n" = 1 ]; then exit 1; fi
exit 0
''')
                stub.chmod(0o700)
                run = subprocess.run(['bash', '-e', '-c', SCRIPT], cwd=root,
                                     env=dict(os.environ, PATH=str(root/'bin')+os.pathsep+os.environ['PATH'], FAIL_FIRST=fail_first),
                                     text=True, capture_output=True)
                self.assertEqual(run.returncode, int(fail_first), run.stderr)
                calls = (root/'calls').read_text().splitlines()
                self.assertEqual(len(calls), 24)
                for index, call in enumerate(calls, 1):
                    self.assertIn('--backend x11rb', call)
                    self.assertIn(f'--evidence-dir artifacts/shared-shell/react-repeat/{index}', call)
                self.assertEqual((root/'artifacts/shared-shell/react-repeat-binaries.sha256').read_text().count('\n'), 4)

    def test_production_defaults_and_original_acceptance_remain(self):
        self.assertIn("if: matrix.bar == 'tauri_react_bar'", BLOCK)
        self.assertIn("JWM_X11_FRAME_SYNC: '1'", BLOCK)
        self.assertNotIn('continue-on-error', BLOCK)
        self.assertNotIn('--configure-notify-probe', BLOCK)
        self.assertIn('test-shared-sidebar-shell-x11.py', SCRIPT)
        self.assertIn('sha256sum --check', SCRIPT)

if __name__ == '__main__':
    unittest.main()
