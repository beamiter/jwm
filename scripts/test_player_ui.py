#!/usr/bin/env python3
"""Real mpv/Lua/X11 interaction regression on a private Xvfb (no sound output).
Requires mpv, ffmpeg, Xvfb and libX11/libXtst. Never uses the user's display.
"""
import ctypes
import json
import os
from pathlib import Path
import shutil
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
        self.xvfb = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '960x600x24'],
                                     stdout=subprocess.PIPE, stderr=self.log)
        self.addCleanup(self.stop, self.xvfb)
        display = ':' + self.xvfb.stdout.readline().decode().strip()
        self.display_name = display
        self.xvfb.stdout.close()
        subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                        'color=c=0x14242e:s=640x360:r=24:d=30', '-c:v', 'mpeg4',
                        str(self.path / 'one.mp4')], check=True)
        (self.path / 'one.srt').write_text('1\n00:00:00,000 --> 00:00:29,000\nJWM subtitle test\n')
        self.chooser_bin = self.path / 'bin'
        self.chooser_bin.mkdir()
        self.mpv = subprocess.Popen([
            'mpv', '--no-config', '--no-terminal', '--ao=null', '--vo=x11',
            '--idle=yes', '--force-window=yes', '--geometry=960x600+0+0', '--keepaspect-window=no', '--border=no',
            '--load-scripts=no', '--osc=no', '--osd-bar=no', '--hwdec=auto-safe', '--sub-auto=fuzzy',
            '--script=' + str(ROOT / 'bridge/src/player/ui.lua'),
            '--log-file=' + str(self.path / 'mpv-detail.log'),
            '--input-ipc-server=' + str(self.path / 'ipc'), str(self.path / 'one.mp4'),
            str(self.path / 'one.mp4')], env={**os.environ, 'DISPLAY': display,
                'PATH': str(self.chooser_bin) + os.pathsep + os.environ['PATH']},
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

    def drag(self, x, y, end_x, end_y):
        self.xtest.XTestFakeMotionEvent(self.display, -1, x, y, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.1)
        self.xtest.XTestFakeButtonEvent(self.display, 1, 1, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.1)
        self.xtest.XTestFakeMotionEvent(self.display, -1, end_x, end_y, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.1)
        self.xtest.XTestFakeButtonEvent(self.display, 1, 0, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.1)

    def assert_lua_healthy(self):
        log = (self.path / 'mpv-detail.log').read_text()
        self.assertNotIn('Lua error', log)
        self.assertNotIn('stack traceback', log)

    def move_pointer(self, x, y):
        self.xtest.XTestFakeMotionEvent(self.display, -1, x, y, 0)
        self.xlib.XFlush(self.display)
        time.sleep(.1)

    def bar_ink(self):
        # The generated video is dark and subtitles are disabled in the
        # visibility test. Bright pixels in these strips belong to the UI.
        # Clear transient OSD feedback before sampling the real framebuffer.
        self.command('show-text', '', 0)
        pixels = subprocess.check_output([
            'ffmpeg', '-v', 'error', '-f', 'x11grab', '-draw_mouse', '0',
            '-video_size', '960x600', '-i', self.display_name, '-frames:v', '1',
            '-pix_fmt', 'rgb24', '-f', 'rawvideo', '-'])
        self.assertEqual(len(pixels), 960 * 600 * 3)
        result = {}
        for bar, start, end in [('top', 0, 55), ('bottom', 540, 600)]:
            strip = pixels[start * 960 * 3:end * 960 * 3]
            result[bar] = sum(max(strip[i:i + 3]) > 100 for i in range(0, len(strip), 3))
        return result

    def test_bar_auto_hide_edges_manual_hide_and_restore(self):
        self.command('set_property', 'sub-visibility', False)
        self.command('set_property', 'pause', False)
        self.move_pointer(400, 250)
        time.sleep(3.4)
        self.assertEqual(self.bar_ink(), {'top': 0, 'bottom': 0})
        self.move_pointer(100, 20)
        ink = self.bar_ink()
        self.assertGreater(ink['top'], 100)
        self.assertEqual(ink['bottom'], 0)
        time.sleep(3.4)
        self.assertGreater(self.bar_ink()['top'], 100, 'hover pins the header')
        self.move_pointer(135, 568)
        self.assertGreater(self.bar_ink()['bottom'], 100)
        self.command('set_property', 'pause', True)
        self.command('keypress', 'h')
        self.click(34, 568)  # Hidden transport must not receive the click
        self.assertTrue(self.command('get_property', 'pause'))
        self.assertEqual(self.bar_ink(), {'top': 0, 'bottom': 0})
        self.command('keypress', 'h')
        self.assertTrue(all(value > 100 for value in self.bar_ink().values()))
        self.click(34, 568)
        self.wait('pause', lambda value: value is False)
        self.command('keypress', 'Ctrl+h')  # Auto -> Always
        self.move_pointer(400, 250)
        time.sleep(3.4)
        self.assertTrue(all(value > 100 for value in self.bar_ink().values()))
        self.command('keypress', 'Ctrl+h')  # Always -> Hidden
        self.assertEqual(self.bar_ink(), {'top': 0, 'bottom': 0})
        self.command('keypress', 'Ctrl+h')  # Hidden -> Auto
        self.command('set_property', 'fullscreen', True)
        time.sleep(3.4)
        self.assertEqual(self.bar_ink(), {'top': 0, 'bottom': 0})
        self.assert_lua_healthy()

    def test_idle_screen_and_local_file_chooser(self):
        # Private chooser stand-ins exercise the real async subprocess path
        # without showing a native dialog on the user's desktop.
        chosen = self.path / '歌曲 ${literal} "quoted".mp4'
        shutil.copyfile(self.path / 'one.mp4', chosen)
        zenity = self.chooser_bin / 'zenity'
        zenity.write_text('#!/usr/bin/python3\nimport sys\nsys.exit(127)\n')
        zenity.chmod(0o700)
        kdialog = self.chooser_bin / 'kdialog'
        kdialog.write_text('#!/usr/bin/python3\nimport sys\nsys.stdout.write(' + repr(str(chosen) + '\n') + ')\n')
        kdialog.chmod(0o700)
        self.command('stop')
        self.wait('idle-active', bool)
        time.sleep(.2)
        self.command('keypress', 'Ctrl+o')
        self.wait('path', lambda value: value == str(chosen))
        self.wait('idle-active', lambda value: value is False)
        self.assert_lua_healthy()

    def test_queue_subtitles_mouse_keyboard_and_decoder_fallback(self):
        self.command('script-message', 'jwm-queue')
        time.sleep(.1)
        self.command('keypress', 'DOWN')
        self.command('keypress', 'ENTER')
        self.wait('playlist-pos', lambda value: value == 1)
        self.command('keypress', 'ESC')
        time.sleep(.1)
        self.click(682, 28)  # Queue button
        self.click(700, 128)  # First queue entry
        self.wait('playlist-pos', lambda value: value == 0)
        self.click(750, 28)  # Subtitle panel
        self.click(700, 128)  # Off row
        self.wait('sid', lambda value: value is False or value == 'no')
        self.click(700, 160)  # SRT row
        self.wait('sid', lambda value: value == 1)
        self.click(828, 28)  # Settings panel
        self.click(700, 128)  # Software decode override
        self.wait('hwdec', lambda value: value in ('no', ['no']))
        self.click(700, 128)
        self.wait('hwdec', lambda value: value in ('auto-safe', ['auto-safe']))
        self.command('keypress', 'ESC')
        self.click(34, 568)  # Pause
        self.wait('pause', bool)
        self.click(487, 564)  # Middle of seek bar
        self.wait('time-pos', lambda value: value is not None and 14 < value < 16)
        self.drag(487, 564, 700, 450)  # Seek capture survives leaving the bar
        self.wait('time-pos', lambda value: value is not None and 25 < value < 28)
        self.click(487, 564)
        self.drag(135, 568, 190, 450)
        self.wait('volume', lambda value: value == 100)
        self.click(135, 568)  # Volume midpoint
        self.wait('volume', lambda value: value is not None and 48 < value < 52)
        self.click(76, 568)
        self.wait('mute', bool)
        self.click(886, 568)  # Speed panel
        self.click(700, 360)  # 1.25x
        self.wait('speed', lambda value: value == 1.25)
        self.command('keypress', 'ESC')
        self.click(34, 568)  # Resume
        self.wait('pause', lambda value: value is False)
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
        log = self.log.read() + (self.path / 'mpv-detail.log').read_text()
        self.assertNotIn('Lua error', log)
        self.assertNotIn('stack traceback', log)
        self.assertEqual(self.mpv.returncode, 0)

    def test_repeat_queue_order_shuffle_and_actual_reverse_playback(self):
        self.command('set_property', 'pause', True)
        self.command('seek', 8, 'absolute+exact')
        self.wait('time-pos', lambda v: v is not None and 7.8 < v < 8.2)
        self.command('loadfile', str(self.path / 'one.mp4'), 'append')
        original = self.command('get_property', 'playlist')
        ids = [entry['id'] for entry in original]
        current = next(entry['id'] for entry in original if entry.get('current'))
        self.click(210, 28)
        time.sleep(.1)
        self.command('keypress', 'r')
        self.wait('loop-file', lambda v: v == 'inf')
        self.command('keypress', 'r')
        self.wait('loop-playlist', lambda v: v == 'inf')
        self.command('keypress', 'r')
        self.wait('loop-file', lambda v: v in ('no', False))
        self.wait('loop-playlist', lambda v: v in ('no', False))
        self.command('keypress', 'Ctrl+r')
        self.wait('playlist', lambda v: [entry['id'] for entry in v or []] == ids[::-1])
        reordered = self.command('get_property', 'playlist')
        self.assertEqual(next(entry['id'] for entry in reordered if entry.get('current')), current)
        self.assertTrue(self.command('get_property', 'pause'))
        self.assertAlmostEqual(self.command('get_property', 'time-pos'), 8, delta=.2)
        self.command('keypress', 'Ctrl+r')
        self.wait('playlist', lambda v: [entry['id'] for entry in v or []] == ids)
        self.command('keypress', 'Ctrl+Shift+s')
        self.wait('shuffle', bool)
        self.assertEqual(sorted(entry['id'] for entry in self.command('get_property', 'playlist')), sorted(ids))
        self.command('keypress', 'Ctrl+Shift+s')
        self.wait('shuffle', lambda v: v is False)
        self.wait('playlist', lambda v: [entry['id'] for entry in v or []] == ids)
        self.command('keypress', 'Ctrl+Shift+r')
        self.wait('play-direction', lambda v: v == 'backward')
        screenshot = os.environ.get('JWM_PLAYBACK_SCREENSHOT')
        if screenshot:
            time.sleep(.25)  # Let the property observer redraw the paused OSD.
            self.command('screenshot-to-file', screenshot, 'window')
        self.command('set_property', 'pause', False)
        self.wait('time-pos', lambda v: v is not None and v < 7.5)
        self.command('keypress', 'Ctrl+Shift+r')
        self.wait('play-direction', lambda v: v == 'forward')
        position = self.command('get_property', 'time-pos')
        self.wait('time-pos', lambda v: v is not None and v > position + .3)
        # The same direction control must reverse audio, not only video frames.
        audio = self.path / 'reverse-audio.wav'
        subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                        'sine=frequency=440:duration=12', str(audio)], check=True)
        self.command('loadfile', str(audio), 'replace')
        self.wait('path', lambda v: v == str(audio))
        self.wait('duration', lambda v: v is not None and 11.8 < v < 12.2)
        self.wait('seekable', bool)
        self.command('set_property', 'pause', True)
        self.command('seek', 8, 'absolute+exact')
        self.wait('time-pos', lambda v: v is not None and 7.8 < v < 8.2)
        self.command('keypress', 'Ctrl+Shift+r')
        self.wait('play-direction', lambda v: v == 'backward')
        self.command('set_property', 'pause', False)
        self.wait('time-pos', lambda v: v is not None and v < 7.5)
        self.command('set_property', 'pause', True)
        self.command('keypress', 'Ctrl+Shift+r')
        self.wait('play-direction', lambda v: v == 'forward')
        self.command('seek', 0, 'absolute+exact')
        self.wait('time-pos', lambda v: v is not None and v < .1)
        self.command('keypress', 'Ctrl+Shift+r')
        self.wait('play-direction', lambda v: v == 'backward')
        self.wait('time-pos', lambda v: v is not None and v > 11)
        self.assertTrue(self.command('get_property', 'pause'))
        self.assert_lua_healthy()


if __name__ == '__main__':
    unittest.main()
