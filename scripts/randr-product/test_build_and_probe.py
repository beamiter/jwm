#!/usr/bin/env python3
"""No network/build/display: checks refusal and clean environment contracts."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import build_and_probe as b

class Contracts(unittest.TestCase):
    def test_reviewed_fixture_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            destination=b.verified_fixture_copy(Path(tmp)/'fixture')
            for name,digest in b.FIXTURE_HASHES.items(): self.assertEqual(b.sha(destination/name),digest)
    def test_changed_fixture_rejected_before_import(self):
        with tempfile.TemporaryDirectory() as tmp:
            with patch.dict(b.FIXTURE_HASHES,{'product_probe.py':'0'*64}):
                with self.assertRaisesRegex(RuntimeError,'unreviewed fixture'):
                    b.verified_fixture_copy(Path(tmp)/'fixture')
    def test_env_does_not_inherit_wrappers_or_display(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);work=root/'work';work.mkdir()
            tools={'cargo':{'path':'/reviewed/bin/cargo'},'rustc':{'path':'/reviewed/bin/rustc'}}
            with patch.dict(b.os.environ,{'CARGO_TARGET_DIR':'/bad','RUSTFLAGS':'bad','RUSTC_WRAPPER':'/bad','DISPLAY':':42','LD_PRELOAD':'/bad'}):
                runner=b.Runner(root,work,tools)
            for key in ('CARGO_TARGET_DIR','RUSTFLAGS','RUSTC_WRAPPER','DISPLAY','LD_PRELOAD'):
                self.assertNotIn(key,runner.env)
            self.assertEqual(runner.env['RUSTC'],'/reviewed/bin/rustc')
    def test_setup_failure_has_failed_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(b,'toolchain',side_effect=RuntimeError('bad tool')):
                rc=b.execute(Path(tmp),Path('/untrusted'))
            report=json.loads((Path(tmp)/'run.json').read_text())
            self.assertEqual((rc,report['status']),(1,'failed'))
    def test_wrong_tool_location_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(RuntimeError,'outside reviewed'):
                b.toolchain(Path(tmp))

if __name__=='__main__': unittest.main()
