#!/usr/bin/env python3
"""Diagnostic 12-pair React/X11RB experiment, never a replacement quality gate.

Each child is the unmodified private-display acceptance fixture. Any failed or
invalid sample makes this runner fail, even after all 24 samples are collected.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SEED = 20261115
PAIRS = 12
FIXTURE = ROOT / 'scripts/test-shared-sidebar-shell-x11.py'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def plan():
    rng = random.Random(SEED)
    result = []
    for pair in range(1, PAIRS + 1):
        arms = ['A', 'B']
        rng.shuffle(arms)
        result.extend((pair, arm) for arm in arms)
    return result


def classify(result, returncode, arm, hashes, fixture_hash):
    errors = []
    if result.get('binary_sha256') != {key: hashes[key] for key in ('frontend', 'host', 'jwm')}:
        errors.append('executed binary hashes differ or are missing')
    if result.get('fixture_sha256') != fixture_hash:
        errors.append('fixture hash differs or is missing')
    if result.get('bar') != 'tauri_react_bar' or result.get('backend') != 'x11rb':
        errors.append('unexpected frontend/backend')
    feedback = result.get('frame_feedback', {})
    root = feedback.get('root', '')
    supported = re.search(r'^_NET_SUPPORTED\(ATOM\) = (.*)$', root, re.M)
    atoms = set(supported[1].split(', ')) if supported else set()
    enabled = {'_NET_WM_FRAME_DRAWN', '_NET_WM_FRAME_TIMINGS'}
    owner = re.search(r'^_JWM_FRAME_SYNC_OWNER\(WINDOW\): window id # (0x[0-9a-f]+)', root, re.M)
    if not supported or (arm == 'A' and (not enabled <= atoms or not owner or int(owner[1], 16) == 0)) or (arm == 'B' and (enabled & atoms or owner)):
        errors.append('root frame-feedback advertisement does not match arm')
    diagnostics = result.get('tauri_frontend_diagnostics', {})
    exact = [xid for xid, raw in diagnostics.items() if re.search(r'_JWM_FRONTEND_READY\([^)]*\)\s*=\s*1\s*$', raw, re.M)]
    windows = feedback.get('windows', {})
    if len(exact) != 1 or set(windows) != set(exact):
        errors.append('missing or ambiguous exact frontend XID evidence')
    window_evidence = windows.get(exact[0], {}) if len(exact) == 1 else {}
    counters = window_evidence.get('counters', [])
    if '_NET_WM_SYNC_REQUEST' not in window_evidence.get('properties', ''):
        errors.append('frontend sync protocol property missing')
    if len(counters) != 2 or any(not isinstance(c.get('id'), int) or c['id'] <= 0 or type(c.get('value')) is not int for c in counters):
        errors.append('missing actual pair of XSync counter values')
    if feedback.get('sync_version') != [3, 1]:
        errors.append('unexpected or missing XSync version')
    archives = result.get('frame_feedback_native_log', {})
    if not archives.get('files') or archives.get('errors') or any(f.get('status') != 'saved' or f.get('tail_offset') != 0 for f in archives.get('files', [])):
        errors.append('native log evidence missing, failed or truncated')
    status = result.get('status')
    if status not in ('passed', 'failed') or (status == 'passed') != (returncode == 0):
        errors.append('fixture exit/status disagreement')
    if not result.get('screenshots'):
        errors.append('actual display evidence missing')
    if errors:
        return 'invalid', errors
    if status == 'passed':
        return 'passed', []
    raw = diagnostics[exact[0]]
    first_raf = re.search(r'^_JWM_FRONTEND_RAF_FIRST\(', raw, re.M)
    dom = re.search(r'^_JWM_FRONTEND_DOM_READY\(', raw, re.M)
    if dom and not first_raf and counters[1]['value'] == 1:
        return 'failed-initial-odd-before-first-raf', []
    return 'failed-other', []


def run(args):
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=False)  # Never consume a stale sample.
    paths = {'jwm': args.bin_dir / 'jwm', 'tool': args.bin_dir / 'jwm-tool',
             'frontend': args.bar_bin, 'host': args.shell_bin}
    paths = {key: path.resolve() for key, path in paths.items()}
    hashes = {key: digest(path) for key, path in paths.items()}
    fixture_hash = digest(FIXTURE)
    base_env = dict(os.environ)
    summary = {'seed': SEED, 'pairs': PAIRS, 'binary_sha256': hashes,
               'fixture_sha256': fixture_hash, 'single_variable': 'JWM_X11_FRAME_SYNC',
               'interpretation': 'Diagnostic experiment; no arm is a production fix.', 'samples': []}
    for index, (pair, arm) in enumerate(plan(), 1):
        output = evidence / f'{index:02d}-pair{pair:02d}-{arm}'
        sample = {'index': index, 'pair': pair, 'arm': arm, 'frame_sync': '1' if arm == 'A' else '0'}
        env = dict(base_env, JWM_X11_FRAME_SYNC=sample['frame_sync'])
        command = [sys.executable, str(FIXTURE), '--bar', 'tauri_react_bar', '--backend', 'x11rb',
                   '--bin-dir', str(args.bin_dir.resolve()), '--bar-bin', str(paths['frontend']),
                   '--shell-bin', str(paths['host']), '--evidence-dir', str(output)]
        sample['command'] = command
        try:
            if {key: digest(path) for key, path in paths.items()} != hashes or digest(FIXTURE) != fixture_hash:
                raise ValueError('binary or fixture changed during experiment')
            with (evidence / f'{index:02d}-runner.log').open('w') as log:
                completed = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
            sample['returncode'] = completed.returncode
            result_path = output / 'tauri_react_bar-x11rb-1280x800-light/result.json'
            result = json.loads(result_path.read_text())
            sample['result'] = str(result_path.relative_to(evidence))
            sample['classification'], sample['errors'] = classify(result, completed.returncode, arm, hashes, fixture_hash)
            sample['reason'] = result.get('reason')
            for record in result.get('frame_feedback_native_log', {}).get('files', []):
                path = result_path.parent / record['file']
                if not path.resolve().is_relative_to(output.resolve()) or path.is_symlink() or digest(path) != record.get('sha256'):
                    raise ValueError('native log artifact missing or hash mismatch')
            for screenshot in result.get('screenshots', []):
                path = ROOT / screenshot
                if not path.resolve().is_relative_to(output.resolve()) or path.is_symlink() or path.read_bytes()[:8] != b'\x89PNG\r\n\x1a\n':
                    raise ValueError('display screenshot artifact missing or invalid')
            if {key: digest(path) for key, path in paths.items()} != hashes:
                raise ValueError('binary changed while fixture ran')
        except (OSError, ValueError, KeyError) as error:
            sample.update(classification='invalid', errors=[str(error)])
        summary['samples'].append(sample)
        summary['counts'] = {arm: dict(Counter(s['classification'] for s in summary['samples'] if s['arm'] == arm)) for arm in ('A', 'B')}
        (evidence / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps(sample), flush=True)
    return 0 if all(s['classification'] == 'passed' for s in summary['samples']) else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pairs', type=int, choices=[PAIRS], default=PAIRS)
    parser.add_argument('--bin-dir', type=Path, required=True)
    parser.add_argument('--bar-bin', type=Path, required=True)
    parser.add_argument('--shell-bin', type=Path, required=True)
    parser.add_argument('--evidence-dir', type=Path, required=True)
    sys.exit(run(parser.parse_args()))
