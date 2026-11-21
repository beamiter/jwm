#!/usr/bin/env python3
"""Verify managed X11 clipboard consent and file progress on owned displays."""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def wait_for(label, read, *actors, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if any(actor.poll() is not None for actor in actors):
            raise RuntimeError(f'actor exited while waiting for {label}')
        if result := read():
            return result
        time.sleep(.025)
    raise RuntimeError(f'timed out waiting for {label}')


def records(path):
    return [json.loads(line) for line in path.read_bytes().splitlines(keepends=True)
            if line.endswith(b'\n')]


def digest(path):
    with path.open('rb') as copied:
        return hashlib.file_digest(copied, 'sha256').hexdigest()


def run(binary, directory, scenario):
    fixtures = Path(__file__).resolve().parent
    launch = runpy.run_path(str(fixtures / 'test-remote-viewer-e2e.py'))['launch']
    env = {key: value for key, value in os.environ.items()
           if key not in ('DISPLAY', 'XAUTHORITY', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET')
           and not key.startswith('JWM_REMOTE_')}

    with ExitStack() as cleanup:
        host_x, host_display = launch()
        cleanup.callback(stop, host_x)
        viewer_x, viewer_display = launch()
        cleanup.callback(stop, viewer_x)
        host_cache = directory / 'host-cache'
        viewer_cache = directory / 'viewer-cache'
        for cache in (host_cache, viewer_cache):
            cache.mkdir(mode=0o700)
        env['JWM_REMOTE_CLIPBOARD_BULK_KBIT'] = '6000'
        host_env = env | {'XDG_CACHE_HOME': str(host_cache)}
        viewer_env = env | {'XDG_CACHE_HOME': str(viewer_cache)}
        key = directory / 'session.key'
        subprocess.run([binary, 'keygen', '--output', key], env=env, check=True,
                       stdout=subprocess.DEVNULL, timeout=8)

        def start(name, command, actor_env, *, stdin=subprocess.DEVNULL):
            output = cleanup.enter_context((directory / f'{name}.stdout').open('wb'))
            errors = cleanup.enter_context((directory / f'{name}.stderr').open('wb'))
            process = subprocess.Popen(command, env=actor_env, stdin=stdin,
                                       stdout=output, stderr=errors)
            cleanup.callback(stop, process)
            return process

        from_host = scenario == 'from-host-transfer'
        transferring = scenario in ('to-host-transfer', 'from-host-transfer')
        host_direction = 'from-host' if from_host or scenario == 'direction-mismatch' else 'to-host'
        host_grant = [] if scenario == 'no-host-consent' else [
            '--allow-clipboard', '--clipboard-direction', host_direction, '--clipboard-file-limit', '12M']
        host = start('host', [binary, 'host', '--display', host_display,
                              '--capture-source', 'root', '--share-region', '320x240+0+0',
                              '--once', '--listen', '127.0.0.1:0', '--key-file', key] + host_grant, host_env)
        endpoint = wait_for('private host listener', lambda: re.search(
            r'listening on (127\.0\.0\.1:\d+)', (directory / 'host.stderr').read_text()), host)[1]
        if transferring:
            # Bound the data path long enough to observe active progress through
            # the helper's periodic snapshots. The proxy forwards opaque records.
            direction_flag = '--s2c-bytes-per-second' if from_host else '--c2s-bytes-per-second'
            proxy = start('proxy', [sys.executable, '-B', fixtures / 'remote-fault-proxy.py',
                                    '--upstream-host', '127.0.0.1', '--upstream-port', endpoint.rsplit(':', 1)[1],
                                    '--max-connections', '1', '--chunk-bytes', '16384', direction_flag, '1000000'], env)
            listener = wait_for('private proxy listener', lambda: records(directory / 'proxy.stdout'), host, proxy)[0]
            require(listener['listening'][0] == '127.0.0.1', 'proxy must remain on loopback')
            endpoint = f"127.0.0.1:{listener['listening'][1]}"
        direction = 'from-host' if from_host else 'to-host'
        consent = ['--clipboard', '--clipboard-direction', direction, '--clipboard-file-limit', '16M',
                   '--escape-key', 'Pause']
        viewer = start('viewer', [binary, 'session', endpoint, '--display', viewer_display,
                                  '--window-size', '640x400', '--jpeg', '--key-file', key]
                       + ([] if scenario == 'no-viewer-consent' else consent), viewer_env,
                       stdin=subprocess.PIPE)
        events = directory / 'viewer.stdout'

        def snapshot(predicate, label, *, after=0):
            return wait_for(label, lambda: next((entry for entry in records(events)[after:]
                if entry['event'] == 'snapshot' and predicate(entry)), None), host, viewer)

        def send(command):
            viewer.stdin.write(json.dumps(command).encode() + b'\n')
            viewer.stdin.flush()

        current = snapshot(lambda entry: entry['state'] == 'viewing' and entry['catalog'], 'first presentation')
        result = {'scenario': scenario, 'initial_clipboard': current['clipboard']}
        if not transferring:
            require(current['clipboard'] == {'mode': None, 'direction': None,
                    'file_limit_bytes': '0', 'transfer': None}, 'clipboard unexpectedly enabled')
        else:
            policy = {'mode': 'all', 'direction': direction,
                      'file_limit_bytes': str(12 << 20), 'transfer': None}
            require(current['clipboard'] == policy, 'effective intersection or file limit differs')
            if from_host:
                send_display, send_env = host_display, host_env
                receive_display, receive_env, cache = viewer_display, viewer_env, viewer_cache
            else:
                send_display, send_env = viewer_display, viewer_env
                receive_display, receive_env, cache = host_display, host_env, host_cache

            def offer(filename, size):
                source = directory / filename
                block = bytes(range(256)) * 4096
                with source.open('wb') as payload:
                    for _ in range(size):
                        payload.write(block)
                owner = start(filename, ['xclip', '-quiet', '-display', send_display,
                                        '-selection', 'clipboard', '-t', 'text/uri-list', '-i'],
                              send_env, stdin=subprocess.PIPE)
                owner.stdin.write((source.as_uri() + '\r\n').encode())
                owner.stdin.close()
                return source

            def progress(after):
                def advancing(entry):
                    transfer = (entry.get('clipboard') or {}).get('transfer')
                    return transfer and 0 < int(transfer['bytes_processed']) < int(transfer['total_bytes'])

                observed = snapshot(advancing, 'partial clipboard progress', after=after)
                transfer = observed['clipboard']['transfer']
                require(transfer['direction'] == direction and transfer['kind'] == 'files', 'incorrect progress scope')
                return transfer, records(events).index(observed) + 1

            completed = offer('complete.bin', 5)
            result['completed_partial'], after_partial = progress(0)
            paste = subprocess.check_output(['xclip', '-display', receive_display, '-selection', 'clipboard',
                                             '-t', 'text/uri-list', '-o'], env=receive_env, timeout=15, text=True)
            expected = digest(completed)
            copies = list(cache.rglob(completed.name))
            require(len(copies) == 1 and copies[0].as_uri() == paste.strip() and digest(copies[0]) == expected,
                    'receiving clipboard did not advertise the complete staged file')
            result['completed_file_sha256'] = expected
            snapshot(lambda entry: entry['clipboard'] == policy, 'cleared progress after completion', after=after_partial)

            after = len(records(events))
            cancelled = offer('cancel.bin', 8)
            result['cancelled_partial'], _ = progress(after)
            wait_for('active clipboard staging', lambda: list(cache.rglob(cancelled.name)), host, viewer)
            window = subprocess.check_output(['xdotool', 'search', '--onlyvisible', '--name', 'JWM Remote'],
                                             env=viewer_env | {'DISPLAY': viewer_display}, timeout=5, text=True).split()
            require(len(window) == 1, 'expected exactly one JWM viewer window')
            subprocess.run(['xdotool', 'windowfocus', '--sync', window[0], 'key', 'ctrl+super+alt+Pause'],
                           env=viewer_env | {'DISPLAY': viewer_display}, timeout=6, check=True)
            narrower = {'mode': 'text-images', 'direction': direction, 'file_limit_bytes': '0', 'transfer': None}
            result['narrowed'] = snapshot(lambda entry: entry['clipboard'] == narrower, 'narrowed clipboard')['clipboard']
            wait_for('confirmed clipboard restriction', lambda: 'clipboard restriction confirmed:'
                     in (directory / 'viewer.stderr').read_text(), host, viewer)
            wait_for('cancelled staging removed', lambda: not list(cache.rglob(cancelled.name)), host, viewer)
            result['restriction_confirmed_and_partial_removed'] = True
            result['restriction_base_key'] = 'Pause'

            catalog = current['catalog']
            other, = [source['source_id'] for source in catalog['entries'] if source['source_id'] != catalog['source_id']]
            send({'command': 'select_source', 'request': 1, 'selection':
                  {key: value for key, value in catalog.items() if key in ('session_id', 'source_epoch')}
                  | {'source_id': other}})
            switched = snapshot(lambda entry: (entry['catalog'] or {}).get('source_id') == other, 'new source presentation')
            require(switched['catalog']['source_epoch'] != catalog['source_epoch']
                    and switched['clipboard'] == narrower, 'source switch changed clipboard policy')
            result['switch_preserved_narrowing'] = True

        send({'command': 'disconnect', 'request': 2})
        result['viewer_exit'] = viewer.wait(timeout=10)
        result['host_exit'] = host.wait(timeout=10)
        final = [entry for entry in records(events) if entry['event'] == 'snapshot'][-1]
        require(final['state'] == 'closed' and final['clipboard'] is None
                and result['viewer_exit'] == result['host_exit'] == 0, 'session did not close cleanly')
        result['closed_snapshot_cleared'] = True
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--output', type=Path, required=True, help='directory for private run logs')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []
    for scenario in ('no-viewer-consent', 'no-host-consent', 'direction-mismatch', 'to-host-transfer', 'from-host-transfer'):
        directory = Path(tempfile.mkdtemp(prefix=f'{scenario}-', dir=args.output))
        results.append(run(args.binary.resolve(strict=True), directory, scenario))
    print(json.dumps({'binary_sha256': digest(args.binary), 'trials': results}, indent=2))


if __name__ == '__main__':
    main()
