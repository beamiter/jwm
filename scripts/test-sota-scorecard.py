#!/usr/bin/env python3
import copy
import importlib.util
import json
import pathlib
import tempfile
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location('score', pathlib.Path(__file__).with_name('sota-scorecard.py'))
score = importlib.util.module_from_spec(spec)
spec.loader.exec_module(score)


def fixture():
    conditions = {key: 'fixed-test-condition' for key in score.CONDITIONS}
    conditions.update(measurement_method='software_input_to_present',
                      timestamp_uncertainty_us=100, deadline_us=1000)
    return dict(schema_version=1, product='synthetic', revision='fixture',
                binary_sha256='a'*64, config_sha256='b'*64, conditions=conditions,
                runs=[dict(repetition=str(i), evidence_sha256='c'*64,
                           window_start_us=0, window_end_us=10000,
                           events=[dict(id='1', input_us=100, present_us=600),
                                   dict(id='2', input_us=200, present_us=2200),
                                   dict(id='3', input_us=300, present_us=None)])
                      for i in range(3)])


class Tests(unittest.TestCase):
    def test_fixed_denominator(self):
        run = score.summarize(fixture())['runs'][0]
        self.assertEqual((run['planned'], run['delivered'], run['missing'],
                          run['late_delivered'], run['deadline_missed']), (3, 2, 1, 1, 2))
        self.assertEqual(run['latency_p95_us'], 2000)
        self.assertEqual(run['deadline_missed_fraction'], 2/3)

    def test_delivering_late_replaces_missing_not_extra_penalty(self):
        a = fixture(); b = copy.deepcopy(a)
        for r in b['runs']: r['events'][2]['present_us'] = 3000
        result = score.compare(a, b)
        self.assertEqual(result['paired_deltas'][0]['candidate_minus_baseline']['deadline_missed_fraction'], 0)

    def test_all_missing_no_fake_zero_latency(self):
        a = fixture()
        for r in a['runs']:
            for e in r['events']: e['present_us'] = None
        out = score.summarize(a)['runs'][0]
        self.assertIsNone(out['latency_p99_us'])
        self.assertEqual(out['deadline_missed_fraction'], 1)

    def test_compare_refuses_different_conditions(self):
        for key in score.CONDITIONS:
            a = fixture(); b = copy.deepcopy(a)
            b['conditions'][key] = (200 if key.endswith('_us') else 'different')
            with self.subTest(key=key), self.assertRaises(ValueError): score.compare(a,b)

    def test_schedule_and_repetition_matching(self):
        for field in ('input_us', 'id'):
            a = fixture(); b = copy.deepcopy(a)
            b['runs'][0]['events'][0][field] = 101 if field == 'input_us' else 'new'
            with self.assertRaises(ValueError): score.compare(a,b)
        b = fixture(); b['runs'][0]['repetition'] = 'extra'
        with self.assertRaises(ValueError): score.compare(fixture(), b)

    def test_fail_closed(self):
        mutations = [lambda x: x.update(schema_version=True),
                     lambda x: x['runs'].pop(),
                     lambda x: x['runs'].append(x['runs'][0]),
                     lambda x: x['runs'][0]['events'].append(x['runs'][0]['events'][0]),
                     lambda x: x['runs'][0]['events'][0].update(input_us=True),
                     lambda x: x['runs'][0]['events'][0].update(present_us=50),
                     lambda x: x['runs'][0]['events'][0].update(present_us=10001),
                     lambda x: x['runs'][0]['events'][0].update(input_us=9500, present_us=None),
                     lambda x: x['runs'][0]['events'][0].pop('present_us'),
                     lambda x: x.update(binary_sha256='unknown'),
                     lambda x: x['conditions'].update(network='unknown')]
        for mutate in mutations:
            a=fixture(); mutate(a)
            with self.assertRaises(ValueError): score.summarize(a)

    def test_reorder_and_absolute_clock_shift(self):
        a=fixture(); b=copy.deepcopy(a)
        b['product']='competitor'; b['config_sha256']='d'*64
        b['runs'].reverse()
        for run in b['runs']:
            run['window_start_us'] += 100000
            run['window_end_us'] += 100000
            for event in run['events']:
                event['input_us'] += 100000
                if event['present_us'] is not None: event['present_us'] += 100000
        self.assertEqual(score.compare(a,b)['paired_deltas'][0]['candidate_minus_baseline']['latency_p95_us'],0)

    def test_file_validation(self):
        with tempfile.TemporaryDirectory() as tmp:
            p=pathlib.Path(tmp)/'evidence.json'
            for text in ('{"x":1,"x":2}', '{"x":NaN}'):
                p.write_text(text)
                with self.assertRaises(ValueError): score.read(p)
            p.write_text(json.dumps(fixture()))
            self.assertEqual(score.read(p), fixture())

    def test_deep_json_cli_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = pathlib.Path(tmp) / 'deep.json'
            p.write_text('[' * 10000 + '0' + ']' * 10000)
            result = subprocess.run([sys.executable, str(pathlib.Path(__file__).with_name(
                'sota-scorecard.py')), str(p)], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, '')
            self.assertNotIn('Traceback', result.stderr)

    def test_quantile_nearest_rank(self):
        self.assertEqual(score.percentile(list(range(1,101)), .99), 99)
        self.assertIsNone(score.percentile([], .95))


if __name__ == '__main__': unittest.main()
