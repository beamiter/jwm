#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import sys
import unittest
spec = importlib.util.spec_from_file_location('readiness', Path(__file__).with_name('remote-gpu-readiness.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class ReadinessTests(unittest.TestCase):
    def test_listing_is_not_hardware_proof(self):
        calls = []
        def runner(args):
            calls.append(args)
            return {'status': 'passed', 'stdout': ' V....D h264_nvenc NVIDIA\n V....D h264_vaapi VAAPI\n', 'stderr': ''}
        r = m.collect('ffmpeg', runner=runner, render_nodes=['/dev/dri/renderD128'])
        self.assertEqual(len(calls), 1)
        self.assertTrue(r['encoders'][0]['advertised'])
        self.assertFalse(r['end_to_end_zero_copy_verified'])
        self.assertEqual(r['encoders'][0]['probe_status'], 'not_requested')

    def test_no_device_and_failed_probe(self):
        def runner(args):
            if '-encoders' in args:
                return {'status': 'passed', 'stdout': ' V....D h264_nvenc GPU\n V....D h264_vaapi GPU\n'}
            return {'status': 'failed', 'stderr': 'No device'}
        r = m.collect('ffmpeg', True, [], runner)
        self.assertEqual(r['encoders'][0]['probes'][0]['status'], 'failed')
        self.assertEqual(r['encoders'][3]['probe_status'], 'no_render_node')

    def test_success_remains_synthetic(self):
        def runner(args):
            return {'status': 'passed', 'stdout': ' V....D av1_vaapi GPU\n', 'stderr': ''}
        r = m.collect('ffmpeg', True, ['/dev/dri/renderD129', '/dev/dri/renderD128'], runner)
        self.assertEqual(len(r['encoders'][5]['probes']), 2)
        self.assertFalse(r['dmabuf_import_verified'])
        self.assertFalse(r['remote_codec_negotiation_verified'])
        self.assertFalse(r['screen_capture_attempted'])

    def test_missing_ffmpeg(self):
        r = m.collect(None, True, [])
        self.assertEqual(r['encoder_inventory_status'], 'unavailable')
        self.assertEqual(r['encoders'][0]['probe_status'], 'inventory_unavailable')

    def test_parser_does_not_accept_error_mentions(self):
        self.assertEqual(m.parse_encoders('Unknown encoder h264_nvenc\n A....D h264_nvenc bad\n'), set())

    def test_commands_bounded_synthetic_and_upload_explicit(self):
        a = m.probe_command('ffmpeg', 'h264_vaapi', '/dev/dri/renderD128')
        self.assertIn('format=nv12,hwupload', a)
        self.assertEqual(a[a.index('-frames:v') + 1], '3')
        self.assertNotIn('x11grab', a)
        with self.assertRaises(ValueError): m.probe_command('ffmpeg', 'invalid')

    def test_process_timeout(self):
        r = m.run_bounded([sys.executable, '-c', 'import time; time.sleep(10)'], timeout=.05)
        self.assertEqual(r['status'], 'timeout')

    def test_process_output_limit(self):
        r = m.run_bounded([sys.executable, '-c', 'print("x"*100000)'], limit=100)
        self.assertEqual(r['status'], 'output_limit')
        self.assertLessEqual(len(r['stdout']), 100)

    def test_process_fail_and_success(self):
        self.assertEqual(m.run_bounded([sys.executable, '-c', 'raise SystemExit(2)'])['status'], 'failed')
        self.assertEqual(m.run_bounded([sys.executable, '-c', 'print(1)'])['status'], 'passed')


if __name__ == '__main__': unittest.main()
