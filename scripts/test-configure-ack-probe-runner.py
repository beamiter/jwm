#!/usr/bin/env python3
import contextlib
import io
import json
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('ab', Path(__file__).with_name('test-configure-ack-probe.py'))
ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ab)
HASHES = {'jwm': 'a', 'host': 'b', 'frontend': 'c', 'tool': 'd'}


def result(arm='A', passed=True):
    root = '_NET_SUPPORTED(ATOM) = _NET_SUPPORTED' + (', _NET_WM_FRAME_DRAWN, _NET_WM_FRAME_TIMINGS' if arm == 'A' else '') + '\n'
    root += '_JWM_FRAME_SYNC_OWNER(WINDOW): window id # 0x200002\n' if arm == 'A' else '_JWM_FRAME_SYNC_OWNER: no such atom on any window.\n'
    return {'bar': 'tauri_react_bar', 'backend': 'x11rb', 'status': 'passed' if passed else 'failed',
            'binary_sha256': {k: HASHES[k] for k in ['jwm', 'host', 'frontend']}, 'fixture_sha256': 'f',
            'tauri_frontend_diagnostics': {'14': '_JWM_FRONTEND_READY(CARDINAL) = 1\n_JWM_FRONTEND_DOM_READY(UTF8_STRING) = "visible"\n'},
            'frame_feedback': {'root': root, 'sync_version': [3, 1], 'windows': {'14': {'properties': 'WM_PROTOCOLS(ATOM) = _NET_WM_SYNC_REQUEST', 'counters': [{'id': 15, 'value': 0}, {'id': 16, 'value': 14 if passed else 1}]}}},
            'frame_feedback_native_log': {'files': [{'status': 'saved', 'tail_offset': 0}], 'errors': []},
            'screenshots': ['desktop.png']}


class Tests(unittest.TestCase):
    def test_valid_passes_and_failed_odd_samples_are_not_promoted(self):
        for arm in ('A', 'B'):
            self.assertEqual(ab.classify(result(arm), 0, arm, HASHES, 'f'), ('passed', []))
            self.assertEqual(ab.classify(result(arm, False), 1, arm, HASHES, 'f')[0], 'failed-initial-odd-before-first-raf')

    def test_wrong_advertisement_and_evidence_are_invalid(self):
        self.assertEqual(ab.classify(result('A'), 0, 'B', HASHES, 'f')[0], 'invalid')
        for key in ['frame_feedback', 'tauri_frontend_diagnostics', 'frame_feedback_native_log', 'screenshots', 'binary_sha256', 'fixture_sha256']:
            sample = result(); del sample[key]
            self.assertEqual(ab.classify(sample, 0, 'A', HASHES, 'f')[0], 'invalid', key)

    def test_wrong_xid_counter_truncation_and_exit_rejected(self):
        samples = []
        sample = result(); sample['frame_feedback']['windows']['99'] = sample['frame_feedback']['windows'].pop('14'); samples.append(sample)
        sample = result(); sample['frame_feedback']['windows']['14']['counters'][1]['value'] = None; samples.append(sample)
        sample = result(); sample['frame_feedback_native_log']['files'][0]['tail_offset'] = 100; samples.append(sample)
        for sample in samples:
            self.assertEqual(ab.classify(sample, 0, 'A', HASHES, 'f')[0], 'invalid')
        self.assertEqual(ab.classify(result(), 1, 'A', HASHES, 'f')[0], 'invalid')


    def exercise_runner(self, fail_first, corrupt_hash=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ['jwm', 'jwm-tool', 'bar', 'host', 'fixture']:
                (root / name).write_text(name)
            hashes = {key: ab.digest(root / name) for key, name in [('jwm', 'jwm'), ('tool', 'jwm-tool'), ('frontend', 'bar'), ('host', 'host')]}
            calls = []
            def child(command, **kwargs):
                calls.append((command, kwargs['env']))
                output = Path(command[command.index('--evidence-dir') + 1]) / 'tauri_react_bar-x11rb-1280x800-light'
                output.mkdir(parents=True)
                self.assertEqual(kwargs['env']['JWM_X11_FRAME_SYNC'], '1')
                self.assertIn('--configure-notify-probe', command)
                arm = 'A'
                passed = not fail_first or len(calls) != 1
                sample = result(arm, passed)
                sample['configure_notify_probe'] = {'original_acceptance': 'failed', 'returncode': 0, 'evidence': {'original_acceptance': 'failed', 'sent_count': 1, 'snapshots': {'before': {}, 'sham': {}, 'after': {}}, 'rescue_observed': True}} if not passed else None
                sample['binary_sha256'] = {k: hashes[k] for k in ('jwm', 'host', 'frontend')}
                sample['fixture_sha256'] = ab.digest(root / 'fixture')
                log = output / 'native.log'; log.write_text('scoped native log')
                sample['frame_feedback_native_log']['files'][0].update(file=log.name, sha256=ab.digest(log))
                screenshot = output / 'desktop.png'; screenshot.write_bytes(b'\x89PNG\r\n\x1a\n')
                sample['screenshots'] = [str(screenshot.relative_to(root))]
                if not passed:
                    for stage in ('before', 'sham', 'after'):
                        probe_image = output / f'{stage}.png'
                        probe_image.write_bytes(b'\x89PNG\r\n\x1a\n')
                        sample['configure_notify_probe']['evidence']['snapshots'][stage].update(screenshot=str(probe_image), screenshot_sha256='bad' if corrupt_hash else ab.digest(probe_image))
                (output / 'result.json').write_text(json.dumps(sample))
                return SimpleNamespace(returncode=0 if passed else 1)
            args = SimpleNamespace(bin_dir=root, bar_bin=root/'bar', shell_bin=root/'host', evidence_dir=root/'evidence')
            with patch.object(ab, 'ROOT', root), patch.object(ab, 'FIXTURE', root/'fixture'), patch.object(ab.subprocess, 'run', side_effect=child), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(ab.run(args), 1 if fail_first else 0)
            self.assertEqual(len(calls), 24)
            environments = [{k:v for k,v in env.items() if k != 'JWM_X11_FRAME_SYNC'} for _, env in calls]
            self.assertTrue(all(env == environments[0] for env in environments))
            summary = json.loads((root/'evidence/summary.json').read_text())
            self.assertEqual(len(summary['samples']), 24)
            self.assertEqual(sum(s['classification'] == 'passed' for s in summary['samples']), 23 if fail_first else 24)
            if fail_first:
                self.assertEqual(summary['samples'][0]['classification'], 'failed-initial-odd-before-first-raf')
                self.assertTrue(summary['samples'][0]['configure_notify_probe']['evidence']['rescue_observed'])
                self.assertEqual(summary['samples'][0]['probe_classification'], 'invalid' if corrupt_hash else 'rescued-after-notify')
                self.assertEqual(summary['conclusion'], 'original-failures-preserved-see-probe-evidence')
            else:
                self.assertEqual(summary['conclusion'], 'inconclusive-no-original-failure')

    def test_missing_or_failed_probe_is_invalid_without_promoting_failure(self):
        original = 'failed-initial-odd-before-first-raf'
        self.assertEqual(ab.classify_probe(original, None)[0], 'invalid')
        self.assertEqual(ab.classify_probe(original, {'returncode': 1})[0], 'invalid')
        self.assertEqual(ab.classify_probe('passed', None)[0], 'not-triggered')

    def test_screenshot_hash_mismatch_preserves_failure(self):
        self.exercise_runner(True, corrupt_hash=True)

    def test_sham_recovery_without_intervention_is_not_rescue(self):
        from copy import deepcopy
        before = {'counters': {'windows': {'14': {'counters': [{}, {'id': 16, 'value': 1}]}}}, 'phases': ''}
        sham = deepcopy(before)
        probe = {'original_acceptance': 'failed', 'returncode': 0, 'evidence': {
            'original_acceptance': 'failed', 'sent_count': 0, 'rescue_observed': False,
            'not_sent_reason': 'first-frame stall changed during sham', 'target': {'xid': '14'},
            'snapshots': {'before': before, 'sham': sham}}}
        original = 'failed-initial-odd-before-first-raf'
        self.assertEqual(ab.classify_probe(original, probe)[0], 'invalid')
        sham['counters']['windows']['14']['counters'][1]['value'] = 2
        self.assertEqual(ab.classify_probe(original, probe)[0], 'no-intervention-counter-advanced')
        sham['phases'] = '_JWM_FRONTEND_RENDERED(CARDINAL) = 1\n'
        self.assertEqual(ab.classify_probe(original, probe)[0], 'recovered-during-sham')
        for key, value in [('not_sent_reason', 'unknown'), ('send_attempted', True), ('rescue_observed', True)]:
            invalid = deepcopy(probe); invalid['evidence'][key] = value
            self.assertEqual(ab.classify_probe(original, invalid)[0], 'invalid')
        del probe['evidence']['snapshots']['before']
        self.assertEqual(ab.classify_probe(original, probe)[0], 'invalid')

    def test_rescue_does_not_erase_original_failure(self):
        self.exercise_runner(True)

    def test_no_reproduction_is_inconclusive_not_a_fix(self):
        self.exercise_runner(False)



if __name__ == '__main__': unittest.main()
