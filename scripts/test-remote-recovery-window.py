#!/usr/bin/env python3
"""Exercise X11 viewer recovery and clipboard restriction over private TCP."""
import argparse
import asyncio
from contextlib import ExitStack, contextmanager, suppress
import importlib.util
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
from threading import Thread
import time


HERE = Path(__file__).resolve().parent


def load_fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


fixture = load_fixture('viewer_fixture', 'test-remote-viewer-e2e.py')
fault_proxy = load_fixture('recovery_fault_proxy', 'remote-fault-proxy.py')


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def wait_for(label, test, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if result := test():
            return result
        time.sleep(.02)
    raise RuntimeError(f'timed out waiting for {label}')


def text(path):
    return path.read_text(encoding='utf-8', errors='replace')


def records(path, kind, *, field='type'):
    for line in text(path).splitlines(keepends=True):
        if line.startswith('{') and line.endswith('\n'):
            record = json.loads(line)
            if record.get(field) == kind:
                yield record


class RecoveryProxy(fault_proxy.Proxy):
    def __init__(self, upstream, stall_attempt):
        super().__init__(upstream)
        self.accepted = 0
        self.stall_attempt = stall_attempt
        self.stalled_retry_started = False
        self.stalled_retry_closed = False
        self.cut_downstream = None

    async def _session(self, client):
        self.accepted += 1
        if self.stall_attempt is None:
            await self._directional_session(client)
            return
        if self.accepted != self.stall_attempt:
            await super()._session(client)
            return
        # Authentication is server-first. Hold the accepted socket without
        # sending its first bytes until the viewer closes the connection.
        loop = asyncio.get_running_loop()
        self.stalled_retry_started = True
        try:
            while await loop.sock_recv(client, 4096):
                pass
        except ConnectionResetError:
            pass
        self.stalled_retry_closed = True

    async def _directional_session(self, client):
        loop = asyncio.get_running_loop()
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as upstream:
            upstream.setblocking(False)
            async with asyncio.timeout(self.connect_timeout):
                await loop.sock_connect(upstream, self.upstream)

            def finish_writes(target):
                with suppress(OSError):
                    target.shutdown(socket.SHUT_WR)

            async def pump(source, target, policy):
                await fault_proxy.forward(
                    lambda size: loop.sock_recv(source, size),
                    lambda data: loop.sock_sendall(target, data),
                    lambda: finish_writes(target),
                    policy, self.chunk_bytes,
                )

            fin_requested = False

            async def to_viewer():
                try:
                    await pump(upstream, client, self.policies[1])
                except asyncio.CancelledError:
                    if not fin_requested:
                        raise
                    client.shutdown(socket.SHUT_WR)

            downstream = asyncio.create_task(to_viewer())
            upload = asyncio.create_task(pump(client, upstream, self.policies[0]))

            async def cut_downstream():
                nonlocal fin_requested
                assert not downstream.done(), 'proxy downstream closed before the fixture fault'
                fin_requested = True
                downstream.cancel()
                # Await the actual FIN. The upload pump keeps receiving
                # until the viewer closes its own write side.
                await downstream

            self.cut_downstream = cut_downstream
            try:
                await asyncio.gather(upload, downstream)
            finally:
                if self.cut_downstream is cut_downstream:
                    self.cut_downstream = None
                for task in (upload, downstream):
                    task.cancel()
                await asyncio.gather(upload, downstream, return_exceptions=True)

    async def cut_connection(self):
        if self.stall_attempt is None:
            assert self.cut_downstream is not None, 'no active downstream for the fixture fault'
            await self.cut_downstream()
            return 'server_to_viewer_fin'
        [session] = self.sessions
        session.cancel()
        await asyncio.gather(session, return_exceptions=True)
        return 'both_proxy_pumps'

    async def accepted_connections(self):
        assert not self.accept_task.done(), 'fault proxy listener stopped'
        return self.accepted

    async def stalled_retry_state(self):
        return self.stalled_retry_started, self.stalled_retry_closed


@contextmanager
def running_recovery_proxy(host_port, *, stall_attempt=None):
    with asyncio.Runner() as runner:
        loop = runner.get_loop()
        proxy = RecoveryProxy(('127.0.0.1', host_port), stall_attempt)
        address, port = runner.run(proxy.start())
        thread = Thread(target=loop.run_forever, name='recovery-fault-proxy', daemon=True)
        thread.start()

        def call(operation):
            return asyncio.run_coroutine_threadsafe(operation(), loop).result(timeout=5)

        def cut():
            return call(proxy.cut_connection)

        def accepted():
            return call(proxy.accepted_connections)

        def stalled():
            return call(proxy.stalled_retry_state)

        try:
            yield address, port, cut, accepted, stalled
        finally:
            try:
                call(proxy.close)
            finally:
                loop.call_soon_threadsafe(loop.stop)
                thread.join(timeout=5)
                assert not thread.is_alive(), 'fault proxy worker did not stop'


def check_clipboard_recovery(viewer, host, viewer_log, host_log, windows, env, cut, accepted):
    broad = 'connected; view-only; clipboard text + images + files, to host'
    narrowed = 'clipboard restriction confirmed: text + images, to host'

    def presented_sessions():
        return {report['session_id'] for report in records(viewer_log, 'jwm_remote_viewer_stats')
                if report['acked'] > 0}

    wait_for('initial file-enabled clipboard session', lambda: broad in text(viewer_log))
    [first_session] = wait_for('initial acknowledged presentation', presented_sessions)
    assert accepted() == 1
    assert cut() == 'server_to_viewer_fin'
    wait_for('recovered file-enabled clipboard session', lambda: text(viewer_log).count(broad) == 2)
    [window] = wait_for('recovered viewer window', windows)
    subprocess.run(['xdotool', 'windowfocus', '--sync', window], env=env, check=True, timeout=5)
    subprocess.run(['xdotool', 'key', 'ctrl+super+alt+Pause'], env=env, check=True, timeout=5)
    wait_for('both clipboard restriction fences', lambda: narrowed in text(viewer_log))
    wait_for('recovered acknowledged presentation', lambda: len(presented_sessions()) == 2)
    assert accepted() == 2
    assert cut() == 'server_to_viewer_fin'
    # An incorrect second retry has a two-second backoff and reaches the same
    # live listener. The restricted command should exit before any third attempt.
    assert viewer.wait(timeout=10) == 1
    [terminal] = records(viewer_log, 'jwm_remote_command_result')
    assert terminal['stage'] == 'session' and terminal['outcome'] == 'error', terminal
    assert terminal['category'] == 'protocol_io' and terminal['io_kind'] in {
        'unexpected_eof', 'connection_reset', 'connection_aborted'
    }, terminal
    assert terminal['cleanup'] != 'incomplete' and not terminal['reconnect_certified'], terminal
    sessions = presented_sessions()
    assert len(sessions) == 2 and first_session in sessions, sessions
    wait_for('matching host session telemetry', lambda: sessions == {
        report['session_id'] for report in records(host_log, 'jwm_remote_host_stats')
    })
    assert text(viewer_log).count('transport lost; reconnect attempt ') == 1
    assert host.poll() is None and accepted() == 2
    print(json.dumps({'scenario': 'clipboard-recovery', 'session_ids': sorted(sessions),
                      'accepted_connections': accepted(), 'confirmed_clipboard': narrowed,
                      'fault': 'server_to_viewer_fin', 'terminal': terminal}, sort_keys=True))


def request_window_close(display_name, xid):
    from Xlib import X, display
    from Xlib.protocol import event

    connection = display.Display(display_name)
    try:
        window = connection.create_resource_object('window', int(xid))
        window.send_event(event.ClientMessage(
            window=window,
            client_type=connection.intern_atom('WM_PROTOCOLS'),
            data=(32, [connection.intern_atom('WM_DELETE_WINDOW'), X.CurrentTime, 0, 0, 0]),
        ))
        connection.sync()
    finally:
        connection.close()


def check_stalled_retry_close(viewer, host, viewer_log, host_log, windows, window_title,
                              client_display, cut, accepted, stalled):
    def acknowledged_session():
        viewer_sessions = {row['session_id'] for row in records(viewer_log, 'jwm_remote_viewer_stats')
                           if row['acked'] > 0}
        host_sessions = {row['session_id'] for row in records(host_log, 'jwm_remote_host_stats')
                         if row['drawn_acks'] > 0}
        return viewer_sessions & host_sessions

    [session] = wait_for('matching initial acknowledged presentation', acknowledged_session)
    [window] = wait_for('initial viewer window', windows)
    assert accepted() == 1
    assert cut() == 'both_proxy_pumps'
    wait_for('second accepted socket with authentication bytes withheld',
             lambda: accepted() == 2 and stalled() == (True, False)
             and text(viewer_log).count('connection opened; authenticating') == 2)
    assert windows() == [window] and 'disconnected' in window_title(window)

    began = time.monotonic_ns()
    request_window_close(client_display, window)
    assert viewer.wait(timeout=2) == 0, 'closing during the stalled retry should exit normally'
    elapsed_ms = (time.monotonic_ns() - began) / 1_000_000
    wait_for('viewer closed the withheld connection', lambda: stalled() == (True, True))
    assert accepted() == 2 and host.poll() is None and windows() == []
    print(json.dumps({'scenario': 'stalled-retry-close', 'initial_session_id': session,
                      'close_event': 'WM_DELETE_WINDOW', 'close_to_exit_ms': elapsed_ms,
                      'proxy_total_connections': accepted(), 'stalled_retry_socket_closed': True}))


MANAGED_SCENARIOS = (
    ('managed-window', 'Managed retry retained the window and refreshed the authenticated scope.'),
    ('managed-xembed', 'Managed retry retained the embedded child and retired it on parent destruction.'),
    ('managed-disconnect', 'Managed disconnect interrupted stalled retry authentication.'),
    ('managed-stdin-close', 'Managed stdin EOF cancelled the pending retry.'),
    ('managed-default', 'Managed sessions default to zero retries.'),
)


def check_managed(binary, scenario, private, cleanup, key, source_display, client_display):
    from Xlib import X, display

    env = {name: value for name, value in os.environ.items()
           if name not in ('DISPLAY', 'XAUTHORITY', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET')
           and not name.startswith('JWM_REMOTE_')}
    env.update(DISPLAY=client_display, XDG_RUNTIME_DIR=str(private), XDG_CACHE_HOME=str(private / 'cache'))
    connection = display.Display(client_display)
    cleanup.callback(connection.close)
    screen = connection.screen()
    parent = screen.root
    embedded = scenario == 'managed-xembed'
    if embedded:
        parent = screen.root.create_window(30, 30, 640, 400, 0, screen.root_depth,
                                           X.InputOutput, X.CopyFromParent)
        parent.map()
        connection.sync()

    def start(name, args, *, stdin=subprocess.DEVNULL):
        stdout = cleanup.enter_context((private / f'{name}.stdout').open('wb'))
        stderr = cleanup.enter_context((private / f'{name}.stderr').open('wb'))
        process = subprocess.Popen(args, env=env, stdin=stdin, stdout=stdout, stderr=stderr)
        cleanup.callback(stop, process)
        return process

    host = start('host', [binary, 'host', '--display', source_display, '--listen', '127.0.0.1:0',
                          '--key-file', str(key), '--capture-source', 'root', '--fixed-jpeg-quality',
                          '--stats-json'])
    host_log = private / 'host.stderr'
    port = wait_for('managed host listener', lambda: re.search(r'listening on 127\.0\.0\.1:(\d+)', text(host_log)))[1]
    stall_attempt = 3 if embedded else 2 if scenario == 'managed-disconnect' else None
    address, port, cut, accepted, stalled = cleanup.enter_context(
        running_recovery_proxy(int(port), stall_attempt=stall_attempt))
    args = [binary, 'session', f'{address}:{port}', '--display', client_display, '--key-file', str(key), '--jpeg']
    if embedded:
        args.extend(['--x11-parent-window', str(parent.id)])
    else:
        args.extend(['--window-size', '640x400'])
    if scenario != 'managed-default':
        args.extend(['--reconnect-attempts', '2'])
    viewer = start('viewer', args, stdin=subprocess.PIPE)
    events = private / 'viewer.stdout'

    def snapshots():
        return list(records(events, 'snapshot', field='event'))

    def snapshot(label, state, *, after=0, ready=lambda _: True):
        def matching():
            return next((event for event in snapshots()[after:]
                         if event['state'] == state and ready(event)), None)
        event = wait_for(label, matching)
        assert event['protocol'] == 1, event
        return event

    def command(request, action, **fields):
        message = {'request': request, 'command': action, **fields}
        viewer.stdin.write(json.dumps(message).encode() + b'\n')
        viewer.stdin.flush()
        result = wait_for(f'managed {action} command result', lambda: next((event for event in
            records(events, 'command_result', field='event') if event['request'] == request), None), timeout=3)
        assert result['protocol'] == 1, result
        return result

    def windows():
        result = subprocess.run(['xdotool', 'search', '--name', 'JWM Remote'], env=env,
                                capture_output=True, text=True, check=False, timeout=3)
        return [int(xid) for xid in result.stdout.splitlines()]

    def check_window(window, size, *, disconnected):
        assert windows() == [window.id], 'the managed viewer XID was destroyed or replaced'
        assert window.query_tree().parent.id == parent.id, 'the viewer moved out of its container'
        geometry = window.get_geometry()
        assert (geometry.width, geometry.height) == size, geometry
        assert ('disconnected' in window.get_wm_name()) == disconnected, window.get_wm_name()
        if embedded:
            atom = connection.intern_atom('_XEMBED_INFO')
            info = window.get_full_property(atom, atom)
            assert info is not None and list(info.value) == [0, 1], info

    def check_reset(event):
        assert event['failure'] is None and all(event[key] is None for key in
            ('catalog', 'metrics', 'clipboard', 'input_grabbed', 'wayland_input', 'wayland_gestures')), event
        assert not event['wayland_source_control'], event

    def check_view_only(event):
        assert event['catalog'] and event['input_grabbed'] is None and event['wayland_input'] is None, event
        assert event['clipboard'] == {'mode': None, 'direction': None,
                                      'file_limit_bytes': '0', 'transfer': None}, event

    def presented(event):
        metrics = event['metrics']
        catalog = event['catalog']
        return (catalog and metrics and metrics['session_id'] == catalog['session_id']
                and metrics['acked_frames'] > 0 and not metrics['final'])

    def check_closed(connections):
        assert viewer.wait(timeout=3) == 0, 'managed close did not complete promptly'
        event = snapshots()[-1]
        assert event['state'] == 'closed' and event['failure'] is None and event['catalog'] is None, event
        assert accepted() == connections and not windows() and host.poll() is None

    try:
        recover = scenario in ('managed-window', 'managed-xembed')
        initial = snapshot('first managed viewing snapshot', 'viewing',
                           ready=presented if recover else lambda event: event['catalog'])
        check_view_only(initial)
        if recover:
            old = initial['catalog']['session_id']
            wait_for('initial acknowledged host presentation', lambda: any(
                row['session_id'] == old and row['drawn_acks'] > 0
                for row in records(host_log, 'jwm_remote_host_stats')))
        [xid] = wait_for('owned managed viewer', windows)
        window = connection.create_resource_object('window', xid)
        check_window(window, (640, 400), disconnected=False)
        if recover:
            if embedded:
                parent.configure(width=930, height=530)
            window.configure(width=930, height=530)
            connection.sync()
            check_window(window, (930, 530), disconnected=False)
        assert accepted() == 1
        after = len(snapshots())
        cut()

        if scenario == 'managed-default':
            assert viewer.wait(timeout=8) == 1
            updates = snapshots()[after:]
            assert not any(event['state'] == 'waiting_to_retry' for event in updates), updates
            terminal = updates[-1]
            failure = terminal['failure']
            assert terminal['state'] == 'failed' and terminal['catalog'] is None, terminal
            assert failure['stage'] == 'session' and failure['cleanup'] == 'complete' \
                and failure['reconnect_certified'], failure
            assert accepted() == 1 and not windows() and host.poll() is None
        else:
            backoff = snapshot('managed backoff', 'waiting_to_retry', after=after)
            check_reset(backoff)
            if scenario == 'managed-stdin-close':
                viewer.stdin.close()
                check_closed(1)
            elif scenario == 'managed-disconnect':
                wait_for('second socket with authentication withheld', lambda: stalled() == (True, False))
                check_reset(snapshot('managed retry authentication', 'connecting', after=after))
                check_window(window, (640, 400), disconnected=True)
                assert command(1, 'disconnect')['accepted']
                check_closed(2)
                wait_for('disconnect closed the withheld socket', lambda: stalled() == (True, True))
            else:
                check_window(window, (930, 530), disconnected=True)
                recovered = snapshot('fresh authenticated managed presentation', 'viewing', after=after,
                                     ready=lambda event: presented(event) and event['catalog']['session_id'] != old)
                check_view_only(recovered)
                check_window(window, (930, 530), disconnected=False)
                assert accepted() == 2
                new = recovered['catalog']['session_id']
                wait_for('fresh acknowledged host presentation', lambda: any(
                    row['session_id'] == new and row['drawn_acks'] > 0
                    for row in records(host_log, 'jwm_remote_host_stats')))
                keys = ('session_id', 'source_epoch', 'source_id')
                stale, fresh = ({key: event['catalog'][key] for key in keys} for event in (initial, recovered))
                assert all(stale[key] == fresh[key] for key in ('source_epoch', 'source_id')), (stale, fresh)
                result = command(1, 'select_source', selection=stale)
                assert not result['accepted'] and result['error'] == 'invalid_or_stale_selection', result
                result = command(2, 'select_source', selection=fresh)
                assert result['accepted'] and result['error'] is None, result
                if embedded:
                    after = len(snapshots())
                    cut()
                    check_reset(snapshot('embedded viewer second backoff', 'waiting_to_retry', after=after))
                    wait_for('third socket with authentication withheld', lambda: stalled() == (True, False))
                    check_reset(snapshot('embedded viewer retry authentication', 'connecting', after=after))
                    check_window(window, (930, 530), disconnected=True)
                    parent.destroy()
                    connection.sync()
                    check_closed(3)
                    wait_for('parent destruction closed the withheld socket', lambda: stalled() == (True, True))
                else:
                    assert command(3, 'disconnect')['accepted']
                    check_closed(2)
        print(json.dumps({'scenario': scenario, 'accepted_connections': accepted(),
                          'terminal': snapshots()[-1]}))
    except BaseException:
        for path in (host_log, events, private / 'viewer.stderr'):
            print(path.name, text(path))
        raise


def test(binary, scenario):
    with tempfile.TemporaryDirectory(prefix='jwm-recovery-test-') as directory, ExitStack() as cleanup:
        private = Path(directory)
        key = private / 'key'
        subprocess.run([binary, 'keygen', '--output', str(key)], check=True)
        source_x, source_display = fixture.launch()
        cleanup.callback(stop, source_x)
        client_x, client_display = fixture.launch()
        cleanup.callback(stop, client_x)
        if scenario.startswith('managed-'):
            return check_managed(binary, scenario, private, cleanup, key, source_display, client_display)
        clipboard = scenario == 'clipboard'
        stalled_retry = scenario == 'stalled-retry-close'
        env = os.environ | {'DISPLAY': client_display, 'XDG_RUNTIME_DIR': directory,
                            'XDG_CACHE_HOME': str(private / 'cache'),
                            'JWM_REMOTE_KEEP_WINDOW': '0' if clipboard else '1'}

        def start(args, name):
            log = private / name
            output = cleanup.enter_context(log.open('wb'))
            process = subprocess.Popen(args, env=env, stdout=output, stderr=subprocess.STDOUT)
            cleanup.callback(stop, process)
            return process, log

        def window_title(window):
            return subprocess.check_output(['xdotool', 'getwindowname', window], env=env, text=True)

        def windows():
            result = subprocess.run(['xdotool', 'search', '--name', 'JWM Remote'],
                                    env=env, capture_output=True, text=True, check=False)
            return result.stdout.splitlines()

        clipboard_policy = ['--clipboard-direction', 'to-host', '--clipboard-file-limit', '1M']
        host, host_log = start(
            [binary, 'host', '--display', source_display, '--listen', '127.0.0.1:0',
             '--key-file', str(key), '--capture-source', 'root', '--fixed-jpeg-quality']
            + (['--stats-json'] if clipboard or stalled_retry else [])
            + (['--allow-clipboard'] + clipboard_policy if clipboard else []),
            'host',
        )
        host_port = wait_for('owned listener', lambda: re.search(r'listening on 127\.0\.0\.1:(\d+)', text(host_log)))
        if clipboard:
            address, port, cut, accepted, _ = cleanup.enter_context(running_recovery_proxy(int(host_port[1])))
            viewer, viewer_log = start(
                [binary, 'connect', f'{address}:{port}', '--key-file', str(key), '--view-only', '--jpeg',
                 '--clipboard', '--escape-key', 'Pause', '--reconnect-attempts', '2', '--stats-json',
                 '--diagnostics-json'] + clipboard_policy,
                'viewer',
            )
            try:
                check_clipboard_recovery(viewer, host, viewer_log, host_log, windows, env, cut, accepted)
            except BaseException:
                for path in (host_log, viewer_log):
                    print(path.name, text(path))
                raise
            return
        if stalled_retry:
            address, port, cut, accepted, stalled = cleanup.enter_context(
                running_recovery_proxy(int(host_port[1]), stall_attempt=2))
            viewer, viewer_log = start(
                [binary, 'connect', f'{address}:{port}', '--key-file', str(key), '--view-only', '--jpeg',
                 '--reconnect-attempts', '2', '--stats-json'],
                'viewer',
            )
            try:
                check_stalled_retry_close(viewer, host, viewer_log, host_log, windows, window_title,
                                          client_display, cut, accepted, stalled)
            except BaseException:
                for path in (host_log, viewer_log):
                    print(path.name, text(path))
                raise
            return
        proxy, proxy_log = start(
            ['python3', str(HERE / 'remote-fault-proxy.py'), '--upstream-host', '127.0.0.1',
             '--upstream-port', host_port[1], '--session-seconds', '4'],
            'proxy',
        )
        wait_for('fault proxy', lambda: any(
            line.startswith('{') for line in text(proxy_log).splitlines()))
        address, port = json.loads(text(proxy_log).splitlines()[0])['listening'][:2]
        assert address == '127.0.0.1'
        args = [binary, 'connect', f'{address}:{port}', '--key-file', str(key),
                '--view-only', '--jpeg', '--reconnect-attempts', '2', '--toggle']
        viewer, viewer_log = start(args, 'viewer')
        try:
            wait_for('initial presentation', lambda: 'connected;' in text(viewer_log))
            [window] = wait_for('viewer window', windows)
            subprocess.run(['xdotool', 'windowsize', window, '930', '530'], env=env, check=True)
            wait_for('disconnect indicator', lambda: 'disconnected' in window_title(window))
            if scenario == 'close':
                subprocess.run(args, env=env, check=True, timeout=5)
                assert viewer.wait(timeout=5) == 0
                assert text(viewer_log).count('connected;') == 1
                return
            wait_for('fresh authenticated session', lambda: text(viewer_log).count('connected;') >= 2)
            assert windows() == [window]
            measured = subprocess.check_output(['xdotool', 'getwindowgeometry', '--shell', window],
                                               env=env, text=True)
            assert 'WIDTH=930\n' in measured and 'HEIGHT=530\n' in measured
            assert 'disconnected' not in window_title(window)
            subprocess.run(args, env=env, check=True, timeout=5)
            assert viewer.wait(timeout=5) == 0
        except BaseException:
            for path in (host_log, proxy_log, viewer_log):
                print(path.name, text(path))
            raise
        assert host.poll() is None and proxy.poll() is None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path, help='path to a built jwm-remote')
    parser.add_argument('--scenario', choices=('all', 'managed', 'window', 'close', 'clipboard',
                        'stalled-retry-close', *(name for name, _ in MANAGED_SCENARIOS)), default='all')
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    for scenario, result in (
        ('window', 'X11 recovery retained the window and geometry.'),
        ('close', 'Closing the X11 recovery window cancelled the next attempt.'),
        ('stalled-retry-close', 'Closing the retained window cancelled the in-flight authentication.'),
        ('clipboard', 'A confirmed clipboard restriction prevented a broader automatic reconnect.'),
        *MANAGED_SCENARIOS,
    ):
        if args.scenario in ('all', scenario) or (args.scenario == 'managed' and scenario.startswith('managed-')):
            test(binary, scenario)
            print(result)


if __name__ == '__main__':
    main()
