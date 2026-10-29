#!/usr/bin/env python3
"""Exercise Eva's real IPC on hidden, disposable desktops; no model/audio API.

The caller supplies the newly built JWM binary. Xvfb hosts Xephyr or nested
Wayland; every WM has private XDG directories, a private D-Bus session and a
private IPC socket. The invoking user's display, IPC and D-Bus are never used.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

REQUIRED = ['Xvfb', 'Xephyr', 'dbus-run-session']


def ipc(path, message, expect=True):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        client.connect(str(path))
        client.sendall(json.dumps(message, separators=(',', ':')).encode() + b'\n')
        frame = bytearray()
        while b'\n' not in frame:
            part = client.recv(4096)
            if not part:
                raise RuntimeError('IPC closed before a complete response')
            frame.extend(part)
            if len(frame) > 1024 * 1024:
                raise RuntimeError('IPC response exceeds frame bound')
        result = json.loads(frame.split(b'\n', 1)[0])
    if expect and not result.get('success'):
        raise RuntimeError(f'IPC {message.get("command", message.get("query"))}: {result.get("error")}')
    return result


def query(path, name):
    return ipc(path, {'query': name, 'args': {}}).get('data')


def command(path, name, args, expect=True):
    return ipc(path, {'command': name, 'args': args}, expect)


class Children:
    def __init__(self):
        self.children = []
        self.guards = []
        self.logs = []

    def spawn(self, argv, env, log, pass_fds=()):
        output = open(log, 'wb')
        self.logs.append(output)
        child = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL,
                                 stdout=output, stderr=subprocess.STDOUT,
                                 start_new_session=True, pass_fds=pass_fds)
        self.children.append(child)
        return child

    def close(self):
        for child in reversed(self.children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=2)
        for output in self.logs:
            output.close()
        for guard in self.guards:
            guard.close()

    def display(self, executable, env, directory):
        for number in range(120, 220):
            path = f'/tmp/.jwm-eva-test-X{number}.guard'
            descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            guard = os.fdopen(descriptor, 'a+b')
            metadata = os.fstat(guard.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
                guard.close()
                continue
            try:
                fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                guard.close()
                continue
            # Inspect symlinks as occupied too; never reclaim another X server.
            if any(os.path.lexists(p) for p in [f'/tmp/.X{number}-lock', f'/tmp/.X11-unix/X{number}']):
                guard.close()
                continue
            self.guards.append(guard)
            break
        else:
            raise RuntimeError('no conservatively free private X display')
        read, write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
        argv = [executable, f':{number}', '-displayfd', str(write), '-nolisten', 'tcp', '-ac']
        argv.extend(['-screen', '0', '1280x800x24', '+extension', 'GLX', '+extension', 'RENDER']
                    if executable == 'Xvfb' else ['-screen', '1280x800'])
        child = self.spawn(argv, env, directory / (executable.lower() + '.log'), (write,))
        os.close(write)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(read, selectors.EVENT_READ)
                if not selector.select(8):
                    raise RuntimeError(f'{executable} startup timed out')
                reported = os.read(read, 64).strip().decode()
                if reported != str(number) or child.poll() is not None:
                    raise RuntimeError(f'{executable} did not start its reserved display')
        finally:
            os.close(read)
        return f':{number}'


def edit_setting(text, section, key, literal, required=True):
    lines = text.splitlines()
    inside = False
    changed = False
    for index, line in enumerate(lines):
        clean = line.strip()
        if clean.startswith('['):
            inside = clean == f'[{section}]'
        if inside and clean.startswith(key + ' ='):
            lines[index] = f'{key} = {literal}'
            changed = True
    if required and not changed:
        raise RuntimeError(f'generated config omitted {section}.{key}')
    return '\n'.join(lines) + '\n'


def sealed(context, permit, identifier, action):
    monitor = context['monitors'][0]
    request = {'protocol_version': 1, 'wm_instance_id': context['wm_instance_id'],
               'assistant_permit': permit, 'operation_id': identifier,
               'expected_context': {'session_unlocked': True,
                                    'monitor_token': monitor['monitor_token'],
                                    'visible_tag_mask': monitor['visible_tag_mask']},
               'action': action}
    encoded = json.dumps(request, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()
    request['request_digest'] = hashlib.sha256(encoded).hexdigest()
    return request


# Expose only this newly created session's bus address to the optional daemon.
# All paths and executable arguments stay argv entries, never shell fragments.
PRIVATE_BUS_WRAPPER = """import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
with os.fdopen(fd, 'w') as output:
    output.write(os.environ['DBUS_SESSION_BUS_ADDRESS'])
os.execv(sys.argv[2], sys.argv[2:])
"""


def process_sample(pid):
    text = Path(f'/proc/{pid}/stat').read_text()
    fields = text.rsplit(')', 1)[1].split()
    status = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
    tasks = []
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            tasks.append(task.joinpath('comm').read_text().strip())
        except FileNotFoundError:
            pass
    return {'ticks': int(fields[11]) + int(fields[12]), 'start_ticks': int(fields[19]),
            'rss_kib': int(status['VmRSS'].split()[0]), 'threads': int(status['Threads']),
            'thread_names': sorted(tasks)}


def audio_thread_names(sample):
    return [name for name in sample['thread_names']
            if 'eva-capture' in name or 'eva-playback' in name
            or name.lower().startswith(('pw-', 'pipewire', 'data-loop'))]


def assert_idle_audio_free(control, child):
    status = ipc(control, {'command': 'status'})
    assert status['pid'] == child.pid, 'measure the actual Eva process, not a wrapper'
    assert status['data']['state'] == 'idle', status
    assert status['data']['microphone_active'] is False, status
    assert status.get('assistant_permit') is None, status
    sample = process_sample(child.pid)
    assert not audio_thread_names(sample), sample
    return sample


def measure_eva_idle(binary, wm_path, env, root, children, context):
    """Real daemon + real isolated WM, before activation; never a voice test."""
    config = root / 'config/eva-idle.toml'
    config.write_text('[interaction]\nfeedback="text"\n[openai]\napi_key_source="file"\n'
                      + 'api_key_file=' + json.dumps(str(root / 'intentionally-absent-api-key'))
                      + '\ninput_transcription=false\n')
    config.chmod(0o600)
    daemon_env = env.copy()
    daemon_env['DBUS_SESSION_BUS_ADDRESS'] = (root / 'private-bus-address').read_text()
    assert daemon_env['DBUS_SESSION_BUS_ADDRESS'].startswith('unix:'), 'expected the private session bus'
    daemon_env['JWM_SOCKET'] = str(wm_path)
    for key in list(daemon_env):
        if key.startswith(('OPENAI_', 'PIPEWIRE_', 'PULSE_')) or key in ('NOTIFY_SOCKET', 'WATCHDOG_USEC', 'WATCHDOG_PID'):
            daemon_env.pop(key, None)
    initial_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
    child = children.spawn([str(binary), '--config', str(config), 'daemon'], daemon_env, root / 'eva-idle.log')
    control = root / 'runtime/jwm-eva/control.sock'
    deadline = time.monotonic() + 8
    while True:
        if child.poll() is not None:
            raise RuntimeError(f'Eva exited before Idle ({child.returncode}); see {root}/eva-idle.log')
        try:
            initial = assert_idle_audio_free(control, child)
            break
        except (OSError, ValueError, RuntimeError):
            if time.monotonic() >= deadline:
                raise RuntimeError('real Eva daemon did not become Idle on the private WM')
            time.sleep(0.02)
    assert query(wm_path, 'get_assistant_status')['active'] is False
    started = time.monotonic()
    while time.monotonic() - started < 2.0:
        sample = process_sample(child.pid)
        assert sample['start_ticks'] == initial['start_ticks'], 'process identity changed during measurement'
        assert not audio_thread_names(sample), sample
        time.sleep(0.05)
    seconds = time.monotonic() - started
    final = assert_idle_audio_free(control, child)
    cpu_seconds = (final['ticks'] - initial['ticks']) / os.sysconf('SC_CLK_TCK')
    assert cpu_seconds >= 0
    assert query(wm_path, 'get_execution_context')['wm_instance_id'] == context['wm_instance_id']
    assert query(wm_path, 'get_assistant_status')['active'] is False
    stopping = time.monotonic()
    child.send_signal(signal.SIGTERM)
    assert child.wait(timeout=4) == 0, 'Eva must shut down cleanly after SIGTERM'
    stop_ms = (time.monotonic() - stopping) * 1000
    assert not control.exists(), 'Eva must remove its private control endpoint'
    assert not Path(f'/proc/{child.pid}').exists(), 'Eva process must be gone'
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == initial_sha, 'Eva artifact changed during measurement'
    return {'name': 'eva_idle_on_real_wm', 'result': 'passed',
            'environment': 'isolated real Eva and real nested WM; shared private DBus and runtime; no cloud, credentials or audio hardware',
            'profile': 'debug path' if 'debug' in binary.parts else 'supplied artifact; build profile not inferred',
            'binary_sha256': initial_sha, 'wm_instance_id': context['wm_instance_id'],
            'measurement_seconds': seconds, 'cpu_seconds': cpu_seconds,
            'cpu_percent_one_core': 100 * cpu_seconds / seconds,
            'cpu_timer_granularity_seconds': 1 / os.sysconf('SC_CLK_TCK'),
            'rss_kib': final['rss_kib'], 'threads': final['threads'], 'thread_names': final['thread_names'],
            'microphone_active': False, 'audio_threads_observed': False, 'audio_thread_markers': ['eva-capture', 'eva-playback', 'pw-*', 'pipewire*', 'data-loop*'], 'sampling_interval_ms': 50,
            'activation_performed': False, 'clean_shutdown': True, 'shutdown_ms': stop_ms,
            'source_epoch_seconds': time.time(), 'proves_voice_e2e': False}


def run_backend(binary, backend, keep, eva=None):
    root = Path(tempfile.mkdtemp(prefix=f'jwm-eva-desktop-{backend}-', dir='/dev/shm' if Path('/dev/shm').is_dir() else '/tmp'))
    root.chmod(0o700)
    children = Children()
    report = {'backend': backend, 'artifact_dir': str(root), 'steps': [], 'result': 'failed', 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
    heartbeat_stop = threading.Event()
    heartbeat = None
    try:
        env = os.environ.copy()
        for key in list(env):
            if key.startswith(('JWM_', 'OPENAI_', 'PIPEWIRE_', 'PULSE_')) or key in ('DISPLAY', 'WAYLAND_DISPLAY', 'DBUS_SESSION_BUS_ADDRESS', 'XAUTHORITY'):
                env.pop(key, None)
        # Deliberately preserve HOME. All writable application namespaces are
        # redirected through XDG, and no user application/bar is launched.
        for key, leaf in [('XDG_CONFIG_HOME', 'config'), ('XDG_STATE_HOME', 'state'),
                          ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'), ('XDG_RUNTIME_DIR', 'runtime')]:
            directory = root / leaf
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        (root / 'config/jwm').mkdir()
        env.update({'LIBGL_ALWAYS_SOFTWARE': '1', 'WINIT_UNIX_BACKEND': 'x11',
                    'JWM_COMPOSITOR_API': 'glx', 'RUST_LOG': 'info'})
        subprocess.run([str(binary), '--backend', backend, '--gen-config'], env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=30, check=True)
        for config in (root / 'config/jwm').glob('config_*.toml'):
            text = config.read_text()
            text = edit_setting(text, 'status_bar', 'show_bar', 'false')
            text = edit_setting(text, 'behavior', 'compositor', 'true')
            text = edit_setting(text, 'desktop_shell', 'enabled', 'false', required=False)
            config.write_text(text)
        host = children.display('Xvfb', env, root)
        env['DISPLAY'] = host
        if backend in ('x11rb', 'xcb'):
            env['DISPLAY'] = children.display('Xephyr', env, root)
        wm_argv = ['dbus-run-session', '--']
        if eva:
            wm_argv.extend([sys.executable, '-c', PRIVATE_BUS_WRAPPER, str(root / 'private-bus-address')])
        wm_argv.extend([str(binary), '--backend', backend])
        wm = children.spawn(wm_argv, env, root / 'jwm.log')
        path = root / 'runtime/jwm-ipc.sock'
        until = time.monotonic() + 30
        while True:
            if wm.poll() is not None:
                raise RuntimeError(f'JWM exited ({wm.returncode}); see {root}/jwm.log')
            try:
                context = query(path, 'get_execution_context')
                if context and context.get('monitors'):
                    break
            except (OSError, ValueError, RuntimeError):
                pass
            if time.monotonic() >= until:
                raise RuntimeError('new Eva execution context did not become ready')
            time.sleep(0.05)
        assert context['protocol_version'] == 1
        report['steps'].append({'name': 'private_context_ready', 'result': 'passed', 'wm_instance_id': context['wm_instance_id']})
        if eva:
            report['steps'].append(measure_eva_idle(eva, path, env, root, children, context))
        activation_attempts = 0
        activation_started = time.monotonic()
        while True:
            activation_attempts += 1
            response = command(path, 'assistant_control', {'action': 'activate'}, expect=False)
            if response.get('success'):
                activation = response['data']
                break
            if response.get('error') != 'assistant_overlay_unavailable':
                raise RuntimeError(f'assistant activation failed: {response}')
            # IPC readiness precedes a first submitted frame on nested
            # compositors. A rejected activation creates no permit or action.
            assert query(path, 'get_assistant_status')['active'] is False
            if time.monotonic() - activation_started >= 5:
                raise RuntimeError(f'no presentable assistant overlay after {activation_attempts} attempts')
            time.sleep(0.05)
        report['activation_readiness'] = {
            'attempts': activation_attempts,
            'first_ready_ms': (time.monotonic() - activation_started) * 1000,
            'result': 'passed',
        }
        permit = activation['assistant_permit']
        assert permit and activation['active']
        def renew():
            while not heartbeat_stop.wait(0.5):
                try:
                    command(path, 'set_assistant_status', {'assistant_permit': permit, 'state': 'executing', 'text': '隔离协议验证（无音频）', 'level': 0})
                except (OSError, RuntimeError):
                    return
        heartbeat = threading.Thread(target=renew, daemon=True)
        heartbeat.start()
        context = query(path, 'get_execution_context')
        monitor = context['monitors'][0]['monitor_token']
        switch = sealed(context, permit, 'desktop-switch-3', {'name': 'switch_tag', 'arguments': {'monitor_token': monitor, 'tag_mask': 4}})
        receipt = command(path, 'guarded_execute', switch)['data']
        assert receipt['status'] == 'succeeded', receipt
        context = query(path, 'get_execution_context')
        assert context['monitors'][0]['visible_tag_mask'] == 4
        report['steps'].append({'name': 'switch_tag_3_verified', 'result': 'passed'})
        layout = sealed(context, permit, 'desktop-layout-scrolling', {'name': 'set_layout', 'arguments': {'monitor_token': monitor, 'tag_mask': 4, 'layout': 'scrolling'}})
        receipt = command(path, 'guarded_execute', layout)['data']
        assert receipt['status'] == 'succeeded', receipt
        context = query(path, 'get_execution_context')
        workspace = next(w for w in context['workspaces'] if w['monitor_token'] == monitor and w['tag_number'] == 3)
        assert workspace['layout'] == 'scrolling', workspace
        report['steps'].append({'name': 'layout_scrolling_verified', 'result': 'passed'})
        # A native ownership receipt must work in the real selected backend,
        # including Wayland's data-device selection. The fixture contains no
        # private history and never touches the invoking user's clipboard.
        command(path, 'clipboard_record', {'text': 'Eva isolated native clipboard fixture'})
        context = query(path, 'get_execution_context')
        copy = sealed(context, permit, 'desktop-native-clipboard', {'name': 'clipboard_copy', 'arguments': {'index': 0}})
        receipt = command(path, 'guarded_execute', copy)['data']
        until = time.monotonic() + 3
        while receipt['status'] == 'accepted' and time.monotonic() < until:
            time.sleep(0.02)
            receipt = ipc(path, {'query': 'get_execution_status', 'args': {'wm_instance_id': context['wm_instance_id'], 'operation_id': copy['operation_id']}})['data']
        assert receipt['status'] == 'succeeded', receipt
        assert receipt['verified']['completion'] == 'native_clipboard_ownership_observed', receipt
        report['steps'].append({'name': 'native_clipboard_ownership_receipt', 'result': 'passed'})
        heartbeat_stop.set()
        heartbeat.join(timeout=2)
        command(path, 'assistant_control', {'action': 'cancel'})
        late = sealed(context, permit, 'desktop-late-switch-8', {'name': 'switch_tag', 'arguments': {'monitor_token': monitor, 'tag_mask': 128}})
        rejected = command(path, 'guarded_execute', late, expect=False)
        assert rejected['success'] is False and 'permit' in rejected['error'], rejected
        old_heartbeat = command(path, 'set_assistant_status', {'assistant_permit': permit, 'state': 'listening', 'text': 'stale'}, expect=False)
        assert old_heartbeat['success'] is False
        assert query(path, 'get_assistant_status')['active'] is False
        assert query(path, 'get_execution_context')['monitors'][0]['visible_tag_mask'] == 4
        report['steps'].append({'name': 'cancel_rejects_late_action_and_old_heartbeat', 'result': 'passed'})
        replay = command(path, 'guarded_execute', switch)['data']
        assert replay['status'] == 'succeeded'
        assert query(path, 'get_execution_context')['monitors'][0]['visible_tag_mask'] == 4
        status = ipc(path, {'query': 'get_execution_status', 'args': {'wm_instance_id': context['wm_instance_id'], 'operation_id': switch['operation_id']}})['data']
        assert status['status'] == 'succeeded'
        report['steps'].append({'name': 'historical_receipt_returned_without_redispatch', 'result': 'passed'})
        if hashlib.sha256(binary.read_bytes()).hexdigest() != report['binary_sha256']:
            raise RuntimeError('binary changed during this backend test; rerun on a fixed artifact')
        report['result'] = 'passed'
    except Exception as error:
        report['error'] = str(error)
    finally:
        heartbeat_stop.set()
        if heartbeat:
            heartbeat.join(timeout=2)
        children.close()
        if report['result'] == 'passed' and not keep:
            shutil.rmtree(root)
            report.pop('artifact_dir', None)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jwm', type=Path, default=Path('target/debug/jwm'))
    parser.add_argument('--eva', type=Path, help='measure a real Eva daemon in Idle before any activation')
    parser.add_argument('--backends', default='x11rb,xcb,wayland-winit,wayland-x11')
    parser.add_argument('--keep', action='store_true')
    parser.add_argument('--preflight', action='store_true')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    dependencies = {name: shutil.which(name) for name in REQUIRED}
    if args.preflight:
        print(json.dumps({'dependencies': dependencies, 'binary': str(args.jwm.resolve()), 'binary_exists': args.jwm.is_file(), 'eva': str(args.eva.resolve()) if args.eva else None, 'eva_exists': args.eva.is_file() if args.eva else None, 'uses_api': False, 'touches_user_desktop': False}, indent=2))
        return 0 if all(dependencies.values()) and args.jwm.is_file() and (not args.eva or args.eva.is_file()) else 1
    if not all(dependencies.values()) or not args.jwm.is_file():
        parser.error('install required Xvfb/Xephyr/dbus-run-session or supply the newly compiled --jwm path')
    if args.eva and not args.eva.is_file():
        parser.error('supply an existing --eva daemon executable')
    backends = args.backends.split(',')
    if not backends or any(b not in ('x11rb', 'xcb', 'wayland-winit', 'wayland-x11') for b in backends):
        parser.error('only isolated X11/nested Wayland backends are allowed')
    report = {'schema_version': 1, 'uses_api': False, 'runs': [run_backend(args.jwm.resolve(), b, args.keep, args.eva.resolve() if args.eva else None) for b in backends]}
    text = json.dumps(report, indent=2, ensure_ascii=False) + '\n'
    if args.report:
        args.report.write_text(text)
    print(text, end='')
    return 0 if all(r['result'] == 'passed' for r in report['runs']) else 1


if __name__ == '__main__':
    raise SystemExit(main())
