#!/usr/bin/env python3
"""Deterministic fixtures; no real hardware, display or network required."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('metadata', Path(__file__).with_name('remote-benchmark-metadata.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class MetadataTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.proc = self.root / 'proc'
        self.sys = self.root / 'sys'
        self.proc.mkdir()
        self.sys.mkdir()
        self.args = m.parser().parse_args([])

    def write(self, path, data):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def collect(self):
        with patch.object(m.sys, 'platform', 'linux'), patch.object(m.os, 'cpu_count', return_value=8):
            return m.collect(self.args, self.proc, self.sys)

    def test_unknown_and_default_no_gpu_probe(self):
        with patch.object(m, 'gpu_ids', side_effect=AssertionError('not opted in')):
            value = self.collect()
        self.assertEqual(value['cpu'], {'logical_count': 8, 'model': None})
        self.assertEqual(value['gpu'], {'requested': False, 'pci_ids': None})
        self.assertEqual(value['declared'], dict(backend=None, display_size=None, build_label=None))

    def test_cpu_only_whitelisted_line_and_no_environment(self):
        self.write(self.proc / 'cpuinfo', b'model name : Test CPU\nSerial: PRIVATE_SERIAL\nusername: PRIVATE_USER\nhostname: PRIVATE_HOST\nip: PRIVATE_IP\n')
        with patch.dict(os.environ, {'SECRET': 'PRIVATE_SECRET', 'HOSTNAME': 'PRIVATE_HOST'}):
            result = self.collect()
        self.assertEqual(result['cpu']['model'], 'Test CPU')
        self.assertNotIn('PRIVATE', json.dumps(result))

    def test_cpu_read_byte_bound_and_partial_line(self):
        self.write(self.proc / 'cpuinfo', b'x' * m.CPU_BYTES + b'\nmodel name: late\n')
        self.assertIsNone(m.cpu_model(self.proc))
        self.write(self.proc / 'cpuinfo', b'x\n' * (m.CPU_BYTES // 2 - 7) + b'model name: Long truncated model\n')
        self.assertIsNone(m.cpu_model(self.proc))
        self.assertEqual(len(m.read_bounded(self.proc / 'cpuinfo', 8, self.proc)), 8)

    def test_invalid_cpu_models(self):
        for model in (b'a' * 161, b'bad\x00value', b'bad\xffvalue', b''):
            self.write(self.proc / 'cpuinfo', b'model name: ' + model + b'\n')
            self.assertIsNone(m.cpu_model(self.proc))

    def test_sysfs_gpu_allowlist_and_parent_symlink(self):
        device = self.sys / 'devices/gpu'
        for field, value in [('vendor', b'0xABCD\n'), ('device', b'0x1234\n'), ('serial', b'PRIVATE'), ('subsystem_vendor', b'not hex')]:
            self.write(device / field, value)
        card = self.sys / 'class/drm/card0'
        card.mkdir(parents=True)
        (card / 'device').symlink_to(device, target_is_directory=True)
        self.args.include_gpu = True
        result = self.collect()
        self.assertEqual(result['gpu']['pci_ids'], [dict(vendor='0xabcd', device='0x1234', subsystem_vendor=None, subsystem_device=None)])
        self.assertNotIn('PRIVATE', json.dumps(result))

    def test_gpu_probe_count_is_bounded(self):
        with patch.object(m, 'read_bounded', return_value=None) as read:
            self.assertIsNone(m.gpu_ids(self.sys))
        self.assertEqual(read.call_count, m.GPU_SLOTS * 4)
        self.assertTrue(all(call.args[1] == m.GPU_BYTES for call in read.call_args_list))

    def test_outside_symlink_and_special_files_refused(self):
        outside = self.root / 'outside'
        outside.write_text('PRIVATE')
        (self.proc / 'cpuinfo').symlink_to(outside)
        self.assertIsNone(m.cpu_model(self.proc))
        (self.proc / 'cpuinfo').unlink()
        os.mkfifo(self.proc / 'cpuinfo')
        self.assertIsNone(m.cpu_model(self.proc))
        self.assertIsNone(m.read_bounded(self.proc, 10, self.proc))

    def test_explicit_arguments(self):
        self.args = m.parser().parse_args(['--backend', 'remote-x11', '--display-size', '1920x1080', '--build-label', 'c8cf77ec-release'])
        self.assertEqual(self.collect()['declared'], dict(backend='remote-x11', display_size=dict(width=1920, height=1080), build_label='c8cf77ec-release'))
        for value in ('0x1', '1x32769', '1x1\n', '1;echo x', '-1x2'):
            with self.assertRaises(argparse.ArgumentTypeError):
                m.display_size(value)
        for value in ('x' * 97, 'a/b', 'user@example', 'a\nb', ''):
            with self.assertRaises(argparse.ArgumentTypeError):
                m.label(value)

    def test_non_linux_and_missing_cpu_count(self):
        with patch.object(m.sys, 'platform', 'darwin'), patch.object(m.os, 'cpu_count', return_value=None), patch.object(m, 'read_bounded', side_effect=AssertionError('no proc on macOS')):
            self.args.include_gpu = True
            result = m.collect(self.args, self.proc, self.sys)
        self.assertIsNone(result['cpu']['model'])
        self.assertIsNone(result['cpu']['logical_count'])
        self.assertIsNone(result['gpu']['pci_ids'])
        self.assertEqual(result['os']['family'], 'darwin')


if __name__ == '__main__':
    unittest.main()
