#!/usr/bin/env python3
"""Build and exercise each selected frontend, retaining explicit failures.

This orchestrator never marks compile-only or stale binary results as accepted.
Run with --bars NAME,NAME to resume a subset. Independent runs can use separate
--output files; the static inventory always remains the 28-item scope.
Explicit ELF loader/rpath overrides are local validation settings: their
absolute native dependencies do not establish portable distribution support.
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

from acceptance_evidence import artifact, provenance

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bars', help='comma-separated subset; default all 28')
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--backend', choices=('xcb', 'x11rb', 'both'), default='both')
    parser.add_argument('--build-only', action='store_true')
    parser.add_argument('--release', action='store_true', help='ordinary Cargo release profile, without extra profile overrides')
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--output', type=Path, default=ROOT / 'artifacts/shared-shell/matrix.json')
    parser.add_argument('--shared-target', type=Path)
    parser.add_argument('--shell-bin', type=Path, default=ROOT / 'crates/jwm_shell/target/debug/jwm-shell')
    parser.add_argument('--elf-rpath', help='explicit private native-library runtime path for selected frontends')
    parser.add_argument('--elf-interpreter', help='explicit compatible ELF loader for selected frontends')
    parser.add_argument('--elf-replace-needed', nargs=2, action='append', default=[],
                        metavar=('SONAME', 'PATH'), help='explicit local ABI dependency replacement, recorded with its hash')
    args = parser.parse_args()
    inventory = json.loads((ROOT / 'docs/shared-shell-coverage.json').read_text())
    selected = set(args.bars.split(',')) if args.bars else {item['name'] for item in inventory['frontends']}
    assert selected <= {item['name'] for item in inventory['frontends']}, 'unknown bar'
    destination = args.output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    evidence_dir = destination.parent / (destination.stem + '-logs')
    evidence_dir.mkdir(exist_ok=True)
    backends = ['xcb', 'x11rb'] if args.backend == 'both' else [args.backend]
    report = {'format': 2, 'default_frontend': 'xilem_bar', 'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(), 'frontends': []}

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
        profile = 'release' if args.release else 'debug'
        binary = (target / profile / name) if target else ROOT / item['binary'].replace('/debug/', f'/{profile}/')
        if args.release:
            command += ['--release']
        if tauri:
            workdir = directory
            command = ['cargo', 'tauri', 'build', '--no-bundle']
            if not args.release:
                command += ['--debug']
        # Dioxus embeds its generated static assets via its desktop runtime; this
        # package's existing Cargo entry point is also directly executable.
        env = dict(os.environ, CARGO_BUILD_JOBS='2')
        env.setdefault('RUST_LOG', 'warn')
        if target:
            env['CARGO_TARGET_DIR'] = str(target)
        started = time.monotonic()
        result = {'name': name, 'profile': profile, 'build_command': command, 'cwd': str(workdir), 'runtime': {},
                  'provenance': provenance([f'bars/{name}', 'crates/shared_structures',
                                            'crates/xbar_core', 'crates/jwm_ipc_transport',
                                            'crates/jwm_shell', 'vendor', 'src', 'Cargo.toml', 'Cargo.lock',
                                            'scripts/validate-shared-shell-matrix.py',
                                            'scripts/test-shared-sidebar-shell-x11.py',
                                            'scripts/test-four-edge-shell-x11.py',
                                            'scripts/acceptance_evidence.py']),
                  'level': 'compiled',
                  'runtime_environment': {key: env[key] for key in
                      ('RUST_LOG', 'WGPU_BACKEND', 'VK_ICD_FILENAMES', 'LIBGL_ALWAYS_SOFTWARE',
                       'PKG_CONFIG_PATH', 'LD_LIBRARY_PATH', 'LIBRARY_PATH', 'CARGO_TARGET_DIR') if key in env}}
        native = {}
        for package in ('glib-2.0', 'gtk4', 'gtk+-3.0', 'webkit2gtk-4.1', 'pango', 'cairo', 'alsa',
                        'libadwaita-1', 'vte-2.91-gtk4', 'xcb', 'x11'):
            version = subprocess.run(['pkg-config', '--modversion', package], env=env,
                                     capture_output=True, text=True)
            if version.returncode == 0:
                native[package] = version.stdout.strip()
        result['native_dependency_versions'] = native
        log_path = evidence_dir / (name + '-build.log')
        with log_path.open('w') as log:
            status = subprocess.run(command, cwd=workdir, env=env, stdout=log, stderr=log).returncode
        result.update(build_status='passed' if status == 0 and binary.is_file() else 'failed',
                      build_exit=status, binary=str(binary), build_log=str(log_path.relative_to(ROOT)),
                      build_seconds=round(time.monotonic() - started, 3))
        if result['build_status'] == 'passed' and (args.elf_rpath or args.elf_interpreter or args.elf_replace_needed):
            # Older patchelf versions can alias dynstr offsets when replacing
            # a dependency and RPATH together. Apply and verify each operation.
            patches = [['patchelf', '--replace-needed', old, new, str(binary)]
                       for old, new in args.elf_replace_needed]
            if args.elf_interpreter:
                patches.append(['patchelf', '--set-interpreter', args.elf_interpreter, str(binary)])
            if args.elf_rpath:
                patches.append(['patchelf', '--force-rpath', '--set-rpath', args.elf_rpath, str(binary)])
            patched = 0
            with log_path.open('a') as log:
                for patch in patches:
                    patched = subprocess.run(patch, stdout=log, stderr=log).returncode
                    if patched:
                        break
            observed_elf = {}
            for key, flag in (('rpath', '--print-rpath'), ('interpreter', '--print-interpreter'),
                              ('needed', '--print-needed')):
                observed_elf[key] = subprocess.check_output(['patchelf', flag, str(binary)], text=True).strip()
            verified = ((not args.elf_rpath or observed_elf['rpath'] == args.elf_rpath)
                        and (not args.elf_interpreter or observed_elf['interpreter'] == args.elf_interpreter)
                        and all(new in observed_elf['needed'].splitlines() for _, new in args.elf_replace_needed))
            result['runtime_link_commands'] = patches
            result['runtime_link_metadata_verified'] = verified
            result['local_abi_dependencies'] = [artifact(replacement)
                for _, replacement in args.elf_replace_needed if Path(replacement).is_file()]
            if patched or not verified:
                result.update(build_status='failed', runtime_link_exit=patched,
                              runtime_link_metadata_verified=verified)
        result['provenance'] = provenance(result['provenance']['dependencies'])
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
            result['binaries'] = [artifact(binary)]
            linked_log = evidence_dir / (name + '-linked-libraries.log')
            with linked_log.open('w') as log:
                linked_status = subprocess.run(['ldd', str(binary)], env=env,
                                               stdout=log, stderr=log).returncode
            result['linked_libraries'] = artifact(linked_log)
            result['linked_libraries_exit'] = linked_status
        if result['build_status'] != 'passed' or args.build_only:
            return result
        for backend in backends:
            command = [sys.executable, str(ROOT / 'scripts/test-shared-sidebar-shell-x11.py'), '--bar', name,
                       '--bar-bin', str(binary), '--backend', backend, '--bin-dir', str(args.bin_dir),
                       '--evidence-dir', str(evidence_dir), '--shell-bin', str(args.shell_bin)]
            log_path = evidence_dir / f'{name}-{backend}.log'
            with log_path.open('w') as log:
                status = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=log).returncode
            fixture = evidence_dir / f'{name}-{backend}-1280x800-light'
            raw = [log_path, fixture / 'result.json', fixture / 'session.log']
            runtime_binaries = [artifact(path) for path in
                                (binary, args.bin_dir / 'jwm', args.bin_dir / 'jwm-tool', args.shell_bin)
                                if path.is_file()]
            observed = {}
            if (fixture / 'result.json').is_file():
                observed = json.loads((fixture / 'result.json').read_text()).get('binary_sha256', {})
            current = {role: artifact(path)['sha256'] if path.is_file() else None
                       for role, path in (('frontend', binary), ('host', args.shell_bin),
                                          ('jwm', args.bin_dir / 'jwm'))}
            stable = {role: observed.get(role) is not None and observed[role] == digest
                      for role, digest in current.items()}
            result['runtime'][backend] = {'status': 'passed' if status == 0 and all(stable.values()) else 'failed',
                                         'log': str(log_path.relative_to(ROOT)), 'exit': status,
                                         'level': 'isolated', 'backend': backend,
                                         'raw_evidence': [artifact(path) for path in raw if path.is_file()],
                                         'binaries': runtime_binaries,
                                         'observed_binary_identity_matches_current_files': stable,
                                         'gpu': 'private Xvfb; LIBGL_ALWAYS_SOFTWARE=1 requested; toolkit adapter not independently verified',
                                         'physical_outputs': False}
        return result

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
        futures = {executor.submit(exercise, item): item['name']
                   for item in inventory['frontends'] if item['name'] in selected}
        for future in concurrent.futures.as_completed(futures):
            try:
                result = future.result()
            except Exception as error:
                result = {'name': futures[future], 'status': 'runner_error',
                          'build_status': 'failed', 'reason': str(error)}
            report['frontends'].append(result)
            destination.write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(result), flush=True)
    assert len(report['frontends']) == len(selected)
    if any(item.get('build_status') != 'passed' or any(run['status'] != 'passed' for run in item.get('runtime', {}).values()) for item in report['frontends']):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
