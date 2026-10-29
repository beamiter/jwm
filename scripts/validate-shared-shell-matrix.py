#!/usr/bin/env python3
"""Build and exercise each selected frontend, retaining explicit failures.

This orchestrator never marks compile-only or stale binary results as accepted.
Run with --bars NAME,NAME to resume a subset. Independent runs can use separate
--output files; the static inventory always remains the 28-item scope.
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess
import time
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bars', help='comma-separated subset; default all 28')
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--backend', choices=('xcb', 'x11rb', 'both'), default='both')
    parser.add_argument('--build-only', action='store_true')
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--output', type=Path, default=ROOT / 'artifacts/shared-shell/matrix.json')
    parser.add_argument('--shared-target', type=Path)
    args = parser.parse_args()
    inventory = json.loads((ROOT / 'docs/shared-shell-coverage.json').read_text())
    selected = set(args.bars.split(',')) if args.bars else {item['name'] for item in inventory['frontends']}
    assert selected <= {item['name'] for item in inventory['frontends']}, 'unknown bar'
    destination = args.output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    evidence_dir = destination.parent / (destination.stem + '-logs')
    evidence_dir.mkdir(exist_ok=True)
    backends = ['xcb', 'x11rb'] if args.backend == 'both' else [args.backend]
    report = {'format': 1, 'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(), 'frontends': []}

    def exercise(item):
        name = item['name']
        directory = ROOT / 'bars' / name
        tauri = (directory / 'src-tauri').is_dir()
        workdir = ROOT
        manifest = ROOT / item['manifest']
        command = ['cargo', 'build', '--manifest-path', str(manifest), '-j', '2']
        tracked_lock = subprocess.run(['git', 'ls-files', '--error-unmatch', str(manifest.with_name('Cargo.lock'))], cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
        if tracked_lock:
            command += ['--locked']
        target = args.shared_target.resolve() if args.shared_target else None
        binary = (target / 'debug' / name) if target else ROOT / item['binary']
        if tauri:
            workdir = directory
            command = ['cargo', 'tauri', 'build', '--debug', '--no-bundle']
        # Dioxus embeds its generated static assets via its desktop runtime; this
        # package's existing Cargo entry point is also directly executable.
        env = dict(os.environ, CARGO_BUILD_JOBS='2')
        env.setdefault('RUST_LOG', 'warn')
        if target:
            env['CARGO_TARGET_DIR'] = str(target)
        started = time.monotonic()
        result = {'name': name, 'build_command': command, 'cwd': str(workdir), 'runtime': {},
                  'runtime_environment': {key: env[key] for key in
                      ('RUST_LOG', 'WGPU_BACKEND', 'VK_ICD_FILENAMES', 'LIBGL_ALWAYS_SOFTWARE') if key in env}}
        log_path = evidence_dir / (name + '-build.log')
        with log_path.open('w') as log:
            status = subprocess.run(command, cwd=workdir, env=env, stdout=log, stderr=log).returncode
        result.update(build_status='passed' if status == 0 and binary.is_file() else 'failed',
                      build_exit=status, binary=str(binary), build_log=str(log_path.relative_to(ROOT)),
                      build_seconds=round(time.monotonic() - started, 3))
        lockfile = manifest.with_name('Cargo.lock')
        if lockfile.is_file():
            lock_copy = evidence_dir / (name + '-Cargo.lock')
            shutil.copyfile(lockfile, lock_copy)
            result['lockfile'] = str(lock_copy.relative_to(ROOT))
        if result['build_status'] == 'passed':
            digest = hashlib.sha256()
            with binary.open('rb') as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                    digest.update(chunk)
            result['sha256'] = digest.hexdigest()
        if result['build_status'] != 'passed' or args.build_only:
            return result
        for backend in backends:
            command = [sys.executable, str(ROOT / 'scripts/test-shared-sidebar-shell-x11.py'), '--bar', name,
                       '--bar-bin', str(binary), '--backend', backend, '--bin-dir', str(args.bin_dir),
                       '--evidence-dir', str(evidence_dir)]
            log_path = evidence_dir / f'{name}-{backend}.log'
            with log_path.open('w') as log:
                status = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=log).returncode
            result['runtime'][backend] = {'status': 'passed' if status == 0 else 'failed',
                                         'log': str(log_path.relative_to(ROOT)), 'exit': status}
        return result

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
        futures = [executor.submit(exercise, item) for item in inventory['frontends'] if item['name'] in selected]
        for future in concurrent.futures.as_completed(futures):
            try:
                result = future.result()
            except Exception as error:
                result = {'status': 'runner_error', 'reason': str(error)}
            report['frontends'].append(result)
            destination.write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(result), flush=True)
    assert len(report['frontends']) == len(selected)
    if any(item.get('build_status') != 'passed' or any(run['status'] != 'passed' for run in item.get('runtime', {}).values()) for item in report['frontends']):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
