#!/usr/bin/env python3
"""Real mpv/Lua/X11 interaction regression on a private Xvfb (no sound output).
Requires mpv, ffmpeg, Xvfb and libX11/libXtst. Never uses the user's display.
"""
import ctypes
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class PlayerUI(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='jwm-ui-test-')
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.log = open(self.path / 'mpv.log', 'w+')
        self.addCleanup(self.log.close)
        self.xvfb = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '960x540x24'],
                                     stdout=subprocess.PIPE, stderr=self.log)
        self.addCleanup(self.stop, self.xvfb)
        display = ':' + self.xvfb.stdout.readline().decode().strip()
        self.xvfb.stdout.close()
        subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                        'color=c=0x14242e:s=640x360:r=24:d=30', '-c:v', 'mpeg4',
                        str(self.path / 'one.mp4')], check=True)
        (self.path / 'one.srt').write_text('1\n00:00:00,000 --> 00:00:29,000\nJWM subtitle test\n')
        self.mpv = subprocess.Popen([
            'mpv', '--no-config', '--no-terminal', '--ao=null', '--vo=x11',
            '--idle=yes', '--force-window=yes', '--geometry=960x540+0+0',
            '--load-scripts=no', '--osc=yes', '--hwdec=auto-safe', '--sub-auto=fuzzy',
            '--script-opts=osc-layout=bottombar,osc-seekbarstyle=bar',
            '--script=' + str(ROOT / 'bridge/src/player/ui.lua'),
            '--input-ipc-server=' + str(self.path / 'ipc'), str(self.path / 'one.mp4'),
            str(self.path / 'one.mp4')], env={**os.environ, 'DISPLAY': display},
            stdout=self.log, stderr=self.log)
        self.addCleanup(self.stop, self.mpv)
        self.sock = socket.socket(socket.AF_UNIX)
        self.addCleanup(self.sock.close)
        self.sock.settimeout(3)
        deadline = time.monotonic() + 5
        while True:
            try:
                self.sock.connect(str(self.path / 'ipc'))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() > deadline:
                    self.fail('mpv did not start')
                time.sleep(.02)
        self.reader = self.sock.makefile('rb')
        self.addCleanup(self.reader.close)
        self.serial = 0
        self.wait('duration', lambda value: value is not None)
        self.wait('track-list', lambda value: any(t['type'] == 'sub' for t in value or []))
        self.xlib = ctypes.CDLL('libX11.so.6')
        self.xtest = ctypes.CDLL('libXtst.so.6')
        self.xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.xlib.XOpenDisplay.restype = ctypes.c_void_p
        self.xlib.XFlush.argtypes = [ctypes.c_void_p]
        self.xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.xtest.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
        self.xtest.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        self.display = self.xlib.XOpenDisplay(display.encode())
        self.assertTrue(self.display)
        self.addCleanup(self.xlib.XCloseDisplay, self.display)
        time.sleep(.3)

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(3)

    def command(self, *args):
        self.serial += 1
        self.sock.sendall(json.dumps({'command': args, 'request_id': self.serial}).encode() + b'\n')
        while True:
            line = self.reader.readline()
            self.assertTrue(line, 'mpv IPC disconnected')
            reply = json.loads(line)
            if reply.get('request_id') == self.serial:
                self.assertEqual(reply.get('error'), 'success', reply)
                return reply.get('data')

    def wait(self, prop, predicate):
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            try:
                value = self.command('get_property', prop)
            except AssertionError:
                value = None
            if predicate(value):
                return value
            time.sleep(.05)
        self.fail(f'{prop} did not reach expected state: {value}')

    def click(self, x, y):
        self.xtest.XTestFakeMotionEvent(self.display, -1, x, y, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.15)
        for down in (1, 0):
            self.xtest.XTestFakeButtonEvent(self.display, 1, down, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.15)

    def test_queue_subtitles_mouse_keyboard_and_decoder_fallback(self):
        self.command('script-message', 'jwm-queue')
        time.sleep(.1)
        self.command('keypress', 'DOWN')
        self.command('keypress', 'ENTER')
        self.wait('playlist-pos', lambda value: value == 1)
        self.command('keypress', 'ESC')
        time.sleep(.1)
        self.click(640, 30)  # Queue button
        self.click(700, 115)  # First queue entry
        self.wait('playlist-pos', lambda value: value == 0)
        self.click(750, 30)  # Subtitle panel
        self.click(700, 115)  # Off row
        self.wait('sid', lambda value: value is False or value == 'no')
        self.click(700, 145)  # SRT row
        self.wait('sid', lambda value: value == 1)
        self.click(880, 30)  # Software decode override
        self.wait('hwdec', lambda value: value == 'no')
        self.click(880, 30)
        self.wait('hwdec', lambda value: value == 'auto-safe')
        self.command('set_property', 'fullscreen', True)
        self.wait('fullscreen', bool)
        # The private X11 software sink must continue advancing after fallback.
        position = self.command('get_property', 'time-pos')
        time.sleep(.3)
        self.assertGreater(self.command('get_property', 'time-pos'), position)
        screenshot = os.environ.get('JWM_UI_SCREENSHOT')
        if screenshot:
            self.command('screenshot-to-file', screenshot, 'window')
        self.command('quit')
        self.mpv.wait(3)
        self.log.seek(0)
        log = self.log.read()
        self.assertNotIn('Lua error', log)
        self.assertNotIn('stack traceback', log)
        self.assertEqual(self.mpv.returncode, 0)


if __name__ == '__main__':
    unittest.main()
