"""Compile/run a native Xlib dock request client inside the existing private fixture."""
import hashlib
import json
import os
from pathlib import Path
import subprocess


def verify(result):
    assert [case['name'] for case in result['cases']] == ['rejected_resize', 'same_geometry', 'rejected_position', 'rejected_border', 'stack_only'], result
    for case in result['cases']:
        synthetic = [event for event in case['events'] if event['send_event']]
        assert len(synthetic) == case['synthetic_count'] == 1, case
        event = synthetic[0]
        assert event['window'] == event['event'] == result['window'], event
        assert event['geometry'] == result['geometry'], event
        assert event['above'] == 0 and not event['override_redirect'], event
        assert case['server_geometry'] == result['geometry'], case
        assert case['server_geometry_unchanged'] and case['passed'], case
    assert result['passed'], result


def run(window, pid, evidence, env, identity):
    directory = evidence / 'dock-configure-check'
    directory.mkdir(exist_ok=True)
    expected = identity(pid)
    assert expected is not None, 'frontend owner disappeared'
    source = Path(__file__).with_name('dock_configure_client.c')
    binary = directory / 'dock-configure-client'
    compiled = subprocess.run(['cc', '-std=c11', '-D_DEFAULT_SOURCE', '-Wall', '-Wextra', '-Werror',
                               str(source), '-lX11', '-o', str(binary)], env=env, capture_output=True, text=True, timeout=60)
    (directory/'build.log').write_text(compiled.stdout+compiled.stderr)
    assert compiled.returncode == 0, 'dock Xlib client compile failed'
    assert identity(pid) == expected, 'frontend identity changed before requests'
    completed = subprocess.run([str(binary), str(window), str(pid)], env=env, capture_output=True, text=True, timeout=20)
    (directory/'stdout.json').write_text(completed.stdout)
    (directory/'stderr.log').write_text(completed.stderr)
    metadata = dict(returncode=completed.returncode, identity=list(expected),
                    identity_after=identity(pid), source_sha256=hashlib.sha256(source.read_bytes()).hexdigest())
    (directory/'result.json').write_text(json.dumps(metadata,indent=2))
    assert identity(pid) == expected, 'frontend identity changed during requests'
    result = json.loads(completed.stdout)
    verify(result)
    assert completed.returncode == 0, metadata
    return dict(metadata, observations=result)
