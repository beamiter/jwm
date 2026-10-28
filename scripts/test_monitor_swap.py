#!/usr/bin/env python3
"""Exercise the real X11 WM on two private RandR monitors; never use the user's display.
Requires built jwm, Xvfb (RandR 1.5), xrandr, dbus-run-session, libX11 and libXtst.
"""
import ctypes
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]

class ClassHint(ctypes.Structure):
    _fields_ = [('res_name', ctypes.c_char_p), ('res_class', ctypes.c_char_p)]


class MonitorSwap(unittest.TestCase):
    backend = os.environ.get('JWM_SWAP_TEST_BACKEND', 'x11rb')
    compositor = os.environ.get('JWM_SWAP_TEST_COMPOSITOR', '0') == '1'

    def stop(self, process, group=False):
        if process.poll() is None:
            if group:
                os.killpg(process.pid, signal.SIGTERM)
            else:
                process.terminate()
            try:
                process.wait(4)
            except subprocess.TimeoutExpired:
                if group:
                    os.killpg(process.pid, signal.SIGKILL)
                else:
                    process.kill()
                process.wait(4)

    def setUp(self):
        self.assertIn(self.backend, ('x11rb', 'xcb'))
        self.directory = tempfile.TemporaryDirectory(prefix='jwm-monitor-swap-')
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.log = open(self.path / 'wm.log', 'w+')
        self.addCleanup(self.log.close)
        self.server = subprocess.Popen([
            'Xvfb', '-displayfd', '1', '-noreset', '-screen', '0', '1920x600x24'],
            stdout=subprocess.PIPE, stderr=self.log)
        self.addCleanup(self.stop, self.server)
        display = ':' + self.server.stdout.readline().decode().strip()
        self.server.stdout.close()
        self.env = {**os.environ, 'DISPLAY': display, 'WAYLAND_DISPLAY': '',
                    'JWM_BACKEND': self.backend, 'JWM_COMPOSITOR': str(int(self.compositor)), 'RUST_LOG': 'warn'}
        for name in ['CONFIG_HOME', 'DATA_HOME', 'STATE_HOME', 'CACHE_HOME', 'RUNTIME_DIR']:
            directory = self.path / name.lower()
            directory.mkdir(mode=0o700)
            self.env['XDG_' + name] = str(directory)
        self.endpoint = self.path / 'runtime_dir/jwm-ipc.sock'
        for args in [
            ['--setmonitor', 'JWM-left', '960/254x600/158+0+0', 'screen'],
            ['--setmonitor', 'JWM-right', '960/254x600/158+960+0', 'none'],
        ]:
            subprocess.run(['xrandr', *args], env=self.env, check=True,
                           stdout=self.log, stderr=self.log)
        self.binary = Path(os.environ.get('JWM_SWAP_TEST_BINARY', ROOT / 'target/debug/jwm'))
        subprocess.run([str(self.binary), '--backend', self.backend, '--gen-config'],
                       env=self.env, check=True, stdout=self.log, stderr=self.log)
        config = self.path / 'config_home/jwm/config_x11.toml'
        text = config.read_text()
        def setting(section, key, value):
            nonlocal text
            pattern = r'(?ms)(^\[' + re.escape(section) + r'\]\n)(.*?)(?=^\[|\Z)'
            def replace(match):
                body, count = re.subn(r'(?m)^' + re.escape(key) + r' = .*$', key + ' = ' + value, match[2])
                self.assertEqual(count, 1, (section, key))
                return match[1] + body
            text, count = re.subn(pattern, replace, text)
            self.assertEqual(count, 1, section)
        setting('status_bar', 'name', json.dumps(str(self.path / 'absent-bar')))
        setting('status_bar', 'show_bar', 'false')
        setting('behavior', 'compositor', 'true' if self.compositor else 'false')
        if self.compositor:
            # Xvfb's Mesa EGL lacks image-pixmap import. GLX provides the
            # supported software-rendered compositor path for this fixture.
            setting('behavior', 'compositor_api', json.dumps(os.environ.get('JWM_SWAP_TEST_COMPOSITOR_API', 'glx')))
        setting('behavior', 'window_animation', 'false')
        setting('animation', 'enabled', 'false')
        setting('layout', 'persist_tags', 'false')
        config.write_text(text)
        self.wm = subprocess.Popen(['dbus-run-session', '--', str(self.binary), '--backend', self.backend],
                                   env=self.env, stdout=self.log, stderr=self.log, start_new_session=True)
        self.addCleanup(self.stop, self.wm, True)
        self.wait(lambda: len(self.query('get_monitors') or []) == 2)
        self.log.flush()
        self.assertEqual(self.query('get_status')['compositor_active'], self.compositor,
                         (self.path / 'wm.log').read_text()[-2500:])
        self.x = ctypes.CDLL('libX11.so.6')
        self.xtest = ctypes.CDLL('libXtst.so.6')
        self.x.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x.XOpenDisplay.restype = ctypes.c_void_p
        self.x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        self.x.XDefaultRootWindow.restype = ctypes.c_ulong
        self.x.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                               ctypes.c_uint, ctypes.c_uint, ctypes.c_uint,
                                               ctypes.c_ulong, ctypes.c_ulong]
        self.x.XCreateSimpleWindow.restype = ctypes.c_ulong
        self.x.XStoreName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_char_p]
        self.x.XSetClassHint.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ClassHint)]
        self.x.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x.XFlush.argtypes = [ctypes.c_void_p]
        self.x.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.x.XGetInputFocus.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int)]
        self.x.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                    ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
        self.x.XGetImage.restype = ctypes.c_void_p
        self.x.XGetPixel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        self.x.XGetPixel.restype = ctypes.c_ulong
        self.x.XDestroyImage.argtypes = [ctypes.c_void_p]
        self.x.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x.XKeysymToKeycode.restype = ctypes.c_uint
        self.x.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_uint)]
        self.x.XWarpPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong,
            ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint, ctypes.c_int, ctypes.c_int]
        self.xtest.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        self.display = self.x.XOpenDisplay(display.encode())
        self.assertTrue(self.display)
        self.addCleanup(self.x.XCloseDisplay, self.display)
        self.root = self.x.XDefaultRootWindow(self.display)
        self.native_windows = {}

    def request(self, kind, name, args=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(3)
            connection.connect(str(self.endpoint))
            connection.sendall(json.dumps({kind: name, 'args': args}).encode() + b'\n')
            with connection.makefile('rb') as reader:
                reply = json.loads(reader.readline())
        self.assertTrue(reply['success'], reply)
        return reply.get('data')

    def query(self, name):
        return self.request('query', name)

    def command(self, name, args=None):
        return self.request('command', name, args)

    def wait(self, predicate):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except (OSError, ValueError):
                pass
            time.sleep(.04)
        self.log.flush()
        try:
            details = subprocess.run(['xwininfo', '-root', '-tree'], env=self.env, capture_output=True, text=True).stdout
        except FileNotFoundError:
            details = 'xwininfo not installed'
        try:
            windows = repr(self.query('get_windows'))
        except (OSError, ValueError) as error:
            windows = f'IPC unavailable: {error}'
        self.fail('state did not converge\n' + details + '\n' + windows + '\n'
                  + (self.path / 'wm.log').read_text()[-4000:])

    def new_window(self, title, x, color=0x29465A):
        window = self.x.XCreateSimpleWindow(self.display, self.root, x, 120, 400, 280, 0, 0, color)
        self.x.XStoreName(self.display, window, title.encode())
        hint = ClassHint(b'jwm-swap-test', b'JwmSwapTest')
        self.x.XSetClassHint(self.display, window, ctypes.byref(hint))
        self.x.XMapWindow(self.display, window)
        self.x.XFlush(self.display)
        self.wait(lambda: any(w['name'] == title for w in self.query('get_windows')))
        identifier = next(w['id'] for w in self.query('get_windows') if w['name'] == title)
        # IPC deliberately exposes backend-neutral ids, not native XIDs.
        self.native_windows[identifier] = window
        return identifier

    def windows(self):
        return {w['id']: w for w in self.query('get_windows')}

    def profiles(self):
        fields = ['layout', 'm_fact', 'n_master', 'gap', 'num_clients']
        return {(w['monitor'], w['tag_mask']): tuple(w[key] for key in fields)
                for w in self.query('get_workspaces')}

    def focused_window(self):
        window, revert = ctypes.c_ulong(), ctypes.c_int()
        self.x.XGetInputFocus(self.display, ctypes.byref(window), ctypes.byref(revert))
        return next((identifier for identifier, native in self.native_windows.items() if native == window.value), None)

    def pointer(self):
        root, child = ctypes.c_ulong(), ctypes.c_ulong()
        x, y, wx, wy, mask = ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_uint()
        self.x.XQueryPointer(self.display, self.root, ctypes.byref(root), ctypes.byref(child),
                             ctypes.byref(x), ctypes.byref(y), ctypes.byref(wx), ctypes.byref(wy), ctypes.byref(mask))
        return x.value, y.value

    def pixel(self, x, y):
        frame = self.x.XGetImage(self.display, self.root, x, y, 1, 1, ctypes.c_ulong(-1).value, 2)
        self.assertTrue(frame)
        try:
            return self.x.XGetPixel(frame, 0, 0) & 0xffffff
        finally:
            self.x.XDestroyImage(frame)

    def exchange_key(self):
        modifiers = [0xffe3, 0xffe9, 0xffe1]  # Control_L, Alt_L, Shift_L
        def key(sym, down):
            code = self.x.XKeysymToKeycode(self.display, sym)
            self.assertTrue(code)
            self.xtest.XTestFakeKeyEvent(self.display, code, down, 0)
        for modifier in modifiers:
            key(modifier, 1)
        key(ord('.'), 1)
        key(ord('.'), 0)
        for modifier in reversed(modifiers):
            key(modifier, 0)
        self.x.XFlush(self.display)

    def test_all_tags_layouts_windows_focus_and_keyboard_round_trip(self):
        monitors = sorted(self.query('get_monitors'), key=lambda m: m['x'])
        left, right = monitors[0]['num'], monitors[1]['num']
        if not monitors[0]['focused']:
            self.command('focusmon', {'value': -1})
        self.command('setlayout', {'layout': 'tile'})
        self.command('setmfact', {'value': 1.63})
        a = self.new_window('Swap A', 120, 0x872C39)
        self.command('view', {'tag': 4})
        self.command('setlayout', {'layout': 'monocle'})
        inactive = self.new_window('Swap inactive tag', 140)
        self.command('view', {'tag': 1})
        minimized = self.new_window('Swap minimized', 160)
        self.command('minimize')
        self.command('focusmon', {'value': 1})
        self.command('view', {'tag': 2})
        self.command('setlayout', {'layout': 'float'})
        self.command('setmfact', {'value': 1.71})
        b = self.new_window('Swap B', 1080, 0x287956)
        self.command('focusmon', {'value': -1})
        self.command('focus_window', a)
        self.x.XWarpPointer(self.display, 0, self.root, 0, 0, 0, 0, 200, 200)
        self.x.XFlush(self.display)
        time.sleep(.1)
        before = self.windows()
        profile_before = self.profiles()
        pointer_before = self.pointer()
        self.assertFalse(before[b]['is_floating'])
        self.assertTrue(before[minimized]['is_minimized'])
        self.assertEqual(self.focused_window(), a)
        self.wait(lambda: self.pixel(200, 200) == 0x872C39)
        self.command('swapmon', {'monitor': right})
        self.wait(lambda: self.focused_window() == b)
        after = self.windows()
        for window in [a, inactive, minimized]:
            self.assertEqual(after[window]['monitor'], right)
            self.assertEqual(after[window]['tags'], before[window]['tags'])
        self.assertEqual(after[b]['monitor'], left)
        self.assertEqual(after[b]['tags'], 2)
        self.assertTrue(after[minimized]['is_minimized'])
        self.assertGreaterEqual(after[a]['x'], 960)
        self.assertLess(after[b]['x'], 960)
        self.wait(lambda: self.pixel(after[b]['x'] + 100, after[b]['y'] + 100) == 0x287956)
        self.assertEqual(self.pointer(), pointer_before)
        profiles = self.profiles()
        for (monitor, tags), profile in profile_before.items():
            self.assertEqual(profiles[(right if monitor == left else left, tags)], profile)
        focused = [m for m in self.query('get_monitors') if m['focused']]
        self.assertEqual([m['num'] for m in focused], [left])
        self.exchange_key()
        self.wait(lambda: self.windows()[a]['monitor'] == left and self.focused_window() == a)
        self.assertEqual(self.profiles(), profile_before)
        restored = self.windows()
        for window in [a, inactive, minimized, b]:
            for key in ['monitor', 'tags', 'is_minimized', 'is_floating', 'is_fullscreen']:
                self.assertEqual(restored[window][key], before[window][key], (window, key))
        self.assertEqual(self.pointer(), pointer_before)
        self.wait(lambda: self.pixel(200, 200) == 0x872C39)
        self.command('quit')
        self.wm.wait(5)
        self.assertEqual(self.wm.returncode, 0)


if __name__ == '__main__':
    unittest.main()
