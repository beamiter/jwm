#!/usr/bin/env python3
"""Owned double-Xvfb follow-size integration. Real sockets only in permitted CI.

Reuses the resize fixture, not a user display. Tests native source geometry and
actual decoded viewer pixels; does not manufacture presentation/ACK events.
--self-test performs only pure parsing/math and starts no processes or sockets.
"""
import argparse
import ctypes as C
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

SPEC = importlib.util.spec_from_file_location('resize_fixture', Path(__file__).with_name('test-remote-window-resize-e2e.py'))
BASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BASE)
require, wait = BASE.require, BASE.wait


class Hints(C.Structure):
    _fields_ = [('flags', C.c_long)] + [(name, C.c_int) for name in (
        'x y width height min_width min_height max_width max_height width_inc height_inc '
        'min_aspect_x min_aspect_y max_aspect_x max_aspect_y base_width base_height win_gravity').split()]


class Native(BASE.XWindows):
    def __init__(self, display):
        super().__init__(display)
        p, u, i = C.c_void_p, C.c_ulong, C.c_int
        signatures = {
            'XResizeWindow': ([p, u, C.c_uint, C.c_uint], i),
            'XWarpPointer': ([p, u, u, i, i, C.c_uint, C.c_uint, i, i], i),
            'XSetWindowBackground': ([p, u, u], i), 'XClearWindow': ([p, u], i),
            'XSelectInput': ([p, u, C.c_long], i), 'XPending': ([p], i), 'XNextEvent': ([p, p], i),
            'XQueryTree': ([p, u, C.POINTER(u), C.POINTER(u), C.POINTER(C.POINTER(u)), C.POINTER(C.c_uint)], i),
            'XGetImage': ([p, u, i, i, C.c_uint, C.c_uint, u, i], p),
            'XGetPixel': ([p, i, i], u), 'XDestroyImage': ([p], i),
            'XSetWMNormalHints': ([p, u, C.POINTER(Hints)], i),
        }
        for name, (arguments, result) in signatures.items():
            fn = getattr(self.lib, name)
            fn.argtypes, fn.restype = arguments, result

    def resize(self, window, width, height):
        self.lib.XResizeWindow(self.conn, window, width, height)
        self.lib.XSync(self.conn, 0)

    def paint(self, window, rgb):
        self.lib.XSetWindowBackground(self.conn, window, rgb)
        self.lib.XClearWindow(self.conn, window)
        self.lib.XSync(self.conn, 0)

    def children(self):
        root, parent, pointer, count = C.c_ulong(), C.c_ulong(), C.POINTER(C.c_ulong)(), C.c_uint()
        require(self.lib.XQueryTree(self.conn, self.root, C.byref(root), C.byref(parent), C.byref(pointer), C.byref(count)), 'query owned viewer tree')
        try:
            return [pointer[index] for index in range(count.value)]
        finally:
            if pointer:
                self.lib.XFree(pointer)

    def pixel(self, window):
        geometry = self.geometry(window)
        image = self.lib.XGetImage(self.conn, window, geometry['width'] // 2, geometry['height'] // 2, 1, 1, C.c_ulong(-1).value, 2)
        require(image, 'read actual viewer pixels')
        try:
            return self.lib.XGetPixel(image, 0, 0) & 0xffffff
        finally:
            self.lib.XDestroyImage(image)

    def watch_input(self, window):
        self.lib.XSelectInput(self.conn, window, 1 | 2 | 4 | 8 | 64)
        self.lib.XSync(self.conn, 0)

    def input_events(self):
        result = []
        self.lib.XSync(self.conn, 0)
        while self.lib.XPending(self.conn):
            event = (C.c_long * 24)()
            self.lib.XNextEvent(self.conn, C.byref(event))
            kind = C.cast(C.byref(event), C.POINTER(C.c_int))[0]
            if kind in (2, 3, 4, 5, 6):
                result.append(kind)
        return result

    def maximum_size(self, window, width, height):
        hints = Hints()
        hints.flags = (1 << 4) | (1 << 5)
        # Deliberately not fixed-size: native resize rejects fixed clients
        # before applying hints. A larger request should clamp to current size.
        hints.min_width = hints.min_height = 100
        hints.max_width = width
        hints.max_height = height
        self.lib.XSetWMNormalHints(self.conn, window, C.byref(hints))
        self.lib.XSync(self.conn, 0)


def similar_pixel(actual, expected):
    return all(abs(((actual >> shift) & 255) - ((expected >> shift) & 255)) <= 12 for shift in (0, 8, 16))


def request_count(text):
    return len(re.findall(r'follow request [1-9][0-9]* ', text))


def logical_window_for_native(context, xid):
    matches = [window for window in context.get('windows', [])
               if type(window.get('native_window_id')) is int and window['native_window_id'] == xid]
    require(len(matches) == 1, 'owned native XID must map to exactly one JWM window')
    logical = int(matches[0]['id'])
    require(logical > 0, 'invalid JWM logical window identity')
    return logical


def run(args):
    work = Path(tempfile.mkdtemp(prefix='jwm-follow-size-e2e-'))
    work.chmod(0o700)
    children, connections = BASE.FIXTURE.Children(), []
    report = dict(status='failed', directory=str(work), checks=[],
                  limitations=['owned Xvfb, x11rb, borderless floating clients, animations off',
                               'real decoded pixels; not physical display or deterministic event interleaving'])
    try:
        env = BASE.private_environment(work)
        (work / 'config/jwm').mkdir(mode=0o700)
        def command(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, stderr=subprocess.STDOUT, text=True, timeout=30)
        command(args.jwm, '--backend', 'x11rb', '--gen-config')
        configs = list((work / 'config/jwm').glob('config_*.toml'))
        require(configs, 'generated private configuration missing')
        for config in configs:
            text = config.read_text()
            for section, key, value, required in (
                ('appearance', 'border_px', '0', True), ('animation', 'enabled', 'false', True),
                ('behavior', 'window_animation', 'false', True), ('status_bar', 'show_bar', 'false', True),
                ('status_bar', 'name', '"/bin/true"', True), ('desktop_shell', 'enabled', 'false', False),
                ('behavior', 'compositor', 'false', True), ('behavior', 'wallpaper_effect', '"off"', False)):
                text = BASE.FIXTURE.edit_setting(text, section, key, value, required)
            config.write_text(text)
        command(args.jwm, '--backend', 'x11rb', '--check-config')
        host_dir, view_dir = work / 'host', work / 'viewer'
        host_dir.mkdir(); view_dir.mkdir()
        env['DISPLAY'] = children.display('Xvfb', env, host_dir)
        view_env = BASE.private_environment(view_dir)
        view_env['DISPLAY'] = children.display('Xvfb', view_env, view_dir)
        hx, vx = Native(env['DISPLAY']), Native(view_env['DISPLAY'])
        connections.extend([hx, vx])
        target, other = hx.create('Follow target', 40), hx.create('Unrelated client', 700)
        wm = children.spawn(['dbus-run-session', '--', args.jwm, '--backend', 'x11rb'], env, work / 'jwm.log')
        ipc = work / 'runtime/jwm-ipc.sock'
        wait(lambda: ipc.is_socket() and wm.poll() is None, 'private WM IPC')
        compositor = children.spawn(['xcompmgr', '-a'], env, work / 'compositor.log')
        wait(lambda: compositor.poll() is None and hx.compositor_owner(), 'owned compositor')
        hx.map(target); hx.map(other)
        nonce = secrets.token_bytes(32); hx.nonce(target, nonce)
        settled = [None, None]
        def managed():
            with BASE.IPC(ipc) as owner:
                grant = owner.command('remote_resize_grant', dict(window=target, challenge=list(nonce)))
                owner.command('remote_resize_revoke', dict(grant=grant['grant']))
            current = hx.geometry(target), hx.geometry(other)
            require(BASE.size(current[0]) == (grant['width'], grant['height']), 'native/model readiness mismatch')
            now = time.monotonic()
            if current != settled[0]:
                settled[:] = [current, now]
            return now - settled[1] >= 0.3
        wait(managed, 'stable managed source')
        hx.clear_nonce(target, nonce)
        key = work / 'remote.key'; command(args.remote, 'keygen', '--output', key)
        def start_host(label, allow):
            log = work / f'{label}-host.log'
            argv = [args.remote, 'host', '--listen', '127.0.0.1:0', '--key-file', str(key), '--window', str(target), '--once', '--fps', '10']
            if allow:
                argv.append('--allow-window-resize')
            child = children.spawn(argv, env, log)
            def ready():
                text = log.read_text(errors='replace')
                require(child.poll() is None, f'host failed: {text}')
                match = re.search(r'listening on (127\.0\.0\.1:\d+)', text)
                return match.group(1) if match else None
            return child, wait(ready, 'host readiness')
        host, address = start_host('allowed', True)
        # Place the test server's own pointer outside every tested window before
        # observing input, so geometry crossing a stationary pointer cannot be
        # confused with remote pointer motion. Never touches a user display.
        hx.lib.XWarpPointer(hx.conn, 0, hx.root, 0, 0, 0, 0, 1200, 700)
        hx.lib.XSync(hx.conn, 0)
        time.sleep(0.1)
        baseline, focus = hx.geometry(other), hx.focus()
        hx.watch_input(target); hx.watch_input(other); hx.input_events()
        log = work / 'allowed-viewer.log'
        viewer = children.spawn([args.remote, 'connect', address, '--key-file', str(key), '--view-only', '--follow-source-size', '--window-size', '400x300'], view_env, log)
        def viewer_window():
            windows = [window for window in vx.children() if vx.viewable(window)]
            require(viewer.poll() is None, log.read_text(errors='replace'))
            return windows[0] if len(windows) == 1 else None
        window = wait(viewer_window, 'actual viewer client window')
        def unchanged_other():
            require(hx.geometry(other) == baseline and hx.focus() == focus, 'follow changed unrelated geometry/focus')
            require(not hx.input_events(), 'view-only follow injected native input')
        def confirm(width, height, color):
            wait(lambda: BASE.size(hx.geometry(target)) == (width, height), 'source native target size')
            wait(lambda: f'follow confirmed Applied {width}x{height} geometry ' in log.read_text(errors='replace'), 'matching genuinely presented source geometry confirmed')
            hx.paint(target, color)
            wait(lambda: similar_pixel(vx.pixel(window), color), 'actual new source pixels reached viewer')
            require(viewer.poll() is None and host.poll() is None, 'follow session exited')
            unchanged_other()
        confirm(400, 300, 0xcc2211)
        for width, height, color in [(480, 320, 0x22cc44), (560, 360, 0x2244dd)]:
            vx.resize(window, width, height)
            confirm(width, height, color)
        report['checks'].append('continuous_native_resize_real_new_pixels_no_input_or_focus_change')
        # Real consecutive ConfigureNotify events. No count is inferred from
        # requested X operations; observed native requests are checked in logs.
        for width in range(580, 641, 10):
            vx.resize(window, width, 400)
        confirm(640, 400, 0xcc33cc)
        report['checks'].append('rapid_viewport_changes_converge_to_latest')
        hx.maximum_size(target, 640, 400)
        target_logical = [None]
        def hints_cached():
            with BASE.IPC(ipc) as query:
                query.sock.sendall(json.dumps({'query': 'get_execution_context'}).encode() + b'\n')
                context = query.response('get_execution_context')
                logical = logical_window_for_native(context, target)
                if target_logical[0] is None:
                    target_logical[0] = logical
                require(logical == target_logical[0], 'owned target logical instance changed')
                query.sock.sendall(json.dumps({'query': 'get_window', 'args': {'id': logical}}).encode() + b'\n')
                info = query.response('get_window')
                require(info.get('id') == logical, 'JWM query identity mismatch')
            hints = info.get('size_hints') or {}
            return (info.get('is_fixed') is False and hints.get('min_w') == 100
                    and hints.get('min_h') == 100 and hints.get('max_w') == 640
                    and hints.get('max_h') == 400)
        # XSync only orders requests on our connection. Wait for the actual
        # JWM-reported model hints before exercising the resize IPC path.
        wait(hints_cached, 'JWM nonfixed min/max hints cache updated')
        vx.resize(window, 700, 440)
        wait(lambda: 'follow stopped: resize Unchanged' in log.read_text(errors='replace'), 'hints refusal stops follow')
        require(BASE.size(hx.geometry(target)) == (640, 400), 'maximum hints were bypassed')
        before = request_count(log.read_text(errors='replace'))
        require(before >= 4, 'real follow requests were not observed')
        vx.resize(window, 720, 460)
        deadline = time.monotonic() + 1.0
        while time.monotonic() < deadline:
            require(request_count(log.read_text(errors='replace')) == before, 'stopped follow retried')
            require(BASE.size(hx.geometry(target)) == (640, 400), 'stopped follow mutated source')
            unchanged_other(); time.sleep(0.05)
        require(viewer.poll() is None, 'stopped follow should retain read-only session')
        report['checks'].append('maximum_hints_unchanged_stop_without_retry_and_keep_view_only')
        BASE.stop(viewer); host.wait(timeout=15)
        # Production follow sessions use native-size encoding (bounded by the
        # unchanged host max-width/frame caps). A second viewport-driven
        # geometry writer previously raced these real burst resize requests.
        # This log proves no encoded-bounds adjustment occurred, not that an
        # identical/no-op Viewport record was absent; the writer policy tests
        # cover zero Viewport emission, including after follow has stopped.
        require('encoding to fit' not in (work / 'allowed-host.log').read_text(errors='replace'),
                'follow session unexpectedly adjusted encoding viewport')
        report['checks'].append('no_viewport_driven_encoded_bounds_changes_entire_session')
        host, address = start_host('denied', False)
        denied_log = work / 'denied-viewer.log'
        denied = children.spawn([args.remote, 'connect', address, '--key-file', str(key), '--view-only', '--follow-source-size'], view_env, denied_log)
        require(denied.wait(timeout=15) != 0 and 'has not authorized native window resizing' in denied_log.read_text(errors='replace'), 'missing resize permission must reject before mutation')
        host.wait(timeout=15)
        require(BASE.size(hx.geometry(target)) == (640, 400), 'unapproved follow changed geometry')
        unchanged_other()
        report['checks'].append('absent_host_permission_rejected_without_resize')
        report['status'] = 'passed'
    except Exception as error:
        report['error'] = str(error)
    finally:
        for connection in reversed(connections):
            connection.close()
        children.close()
        if args.report:
            args.report.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
        if report['status'] == 'passed' and not args.keep:
            shutil.rmtree(work)
    return 0 if report['status'] == 'passed' else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jwm', type=lambda p: str(Path(p).resolve()))
    parser.add_argument('--remote', type=lambda p: str(Path(p).resolve()))
    parser.add_argument('--report', type=Path)
    parser.add_argument('--keep', action='store_true')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        require(similar_pixel(0x123456, 0x123456), 'pixel equality')
        require(not similar_pixel(0xff0000, 0x0000ff), 'pixel distinction')
        require(request_count('follow request 1 400x300 geometry 1\nfollow stopped: resize Unchanged\n') == 1, 'request record count')
        require(C.sizeof(Hints) == C.sizeof(C.c_long) + 18 * C.sizeof(C.c_int), 'XSizeHints ABI')
        context = {'windows': [{'id': '7', 'native_window_id': 0x200001}, {'id': '8', 'native_window_id': 0x200002}]}
        require(logical_window_for_native(context, 0x200001) == 7, 'native XID is not logical ID')
        for invalid in ({'windows': []}, {'windows': [context['windows'][0], context['windows'][0]]}):
            try:
                logical_window_for_native(invalid, 0x200001)
            except RuntimeError:
                pass
            else:
                raise AssertionError('missing/ambiguous native identity was accepted')
        print('self-test passed (no sockets/processes/files)')
        return 0
    require(args.jwm and args.remote, 'prebuilt --jwm and --remote required')
    for binary in (args.jwm, args.remote):
        require(Path(binary).is_file() and os.access(binary, os.X_OK), f'missing executable {binary}')
    for program in ('Xvfb', 'xcompmgr', 'dbus-run-session'):
        require(shutil.which(program), f'missing CI dependency {program}')
    return run(args)


if __name__ == '__main__':
    sys.exit(main())
