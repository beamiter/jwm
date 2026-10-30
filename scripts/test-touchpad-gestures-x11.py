#!/usr/bin/env python3
"""Exercise real X11 gesture input with a temporary uinput touchpad.

Run with --live in an active local JWM X11 session. The fixture briefly visits
an unused tag, creates its own window, then restores the original tag, focus and
pointer. Existing windows are never minimized. The short-lived root helper can
create only this virtual touchpad; the WM and IPC client remain unprivileged.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import select
import shutil
import struct
import subprocess
import sys
import time


def uinput_child():
    descriptor = os.open('/dev/uinput', os.O_WRONLY | os.O_CLOEXEC)
    name = f'JWM Gesture Test Touchpad {os.getpid()}'
    def ioctl(request, value):
        fcntl.ioctl(descriptor, request, value)
    def frame(events):
        now = time.monotonic()
        sec, usec = int(now), int(now % 1 * 1_000_000)
        os.write(descriptor, b''.join(struct.pack('llHHi', sec, usec, kind, code, value)
                                     for kind, code, value in [*events, (0, 0, 0)]))
    def release():
        frame([item for slot in range(3) for item in [(3, 0x2f, slot), (3, 0x39, -1)]]
              + [(1, 0x14e, 0), (1, 0x14a, 0)])
    try:
        for kind in (1, 3): ioctl(0x40045564, kind)  # UI_SET_EVBIT
        for key in (0x110, 0x145, 0x14d, 0x14e, 0x14f, 0x148, 0x14a):
            ioctl(0x40045565, key)  # UI_SET_KEYBIT: buttons/finger counts only
        for prop in (0, 2): ioctl(0x4004556e, prop)  # POINTER, BUTTONPAD
        for code, maximum, resolution in ((0, 10000, 100), (1, 6000, 100),
                (0x2f, 4, 0), (0x35, 10000, 100), (0x36, 6000, 100), (0x39, 65535, 0)):
            ioctl(0x40045567, code)
            ioctl(0x401c5504, struct.pack('H2xiiiiii', code, 0, 0, maximum, 0, 0, resolution))
        ioctl(0x405c5503, struct.pack('HHHH80sI', 3, 0x1234, 0x5678, 1, name.encode(), 0))
        ioctl(0x5501, 0)
        print(json.dumps({'created': name}), flush=True)
        for line in sys.stdin:
            request = json.loads(line)
            if request['op'] == 'release':
                release()
            elif request['op'] == 'swipe':
                points = [[3000, 2500], [4500, 2600], [6000, 2500]]
                events = [(1, 0x14a, 1), (1, 0x14e, 1)]
                for slot, (x, y) in enumerate(points):
                    events += [(3, 0x2f, slot), (3, 0x39, slot + 1), (3, 0x35, x), (3, 0x36, y)]
                events += [(3, 0, points[0][0]), (3, 1, points[0][1])]
                frame(events)
                time.sleep(0.06)
                for dx, dy in request['moves']:
                    origins = [point[:] for point in points]
                    for step in range(1, 17):
                        events = []
                        for slot, origin in enumerate(origins):
                            points[slot] = [round(origin[0] + dx * step / 16), round(origin[1] + dy * step / 16)]
                            x, y = points[slot]
                            events += [(3, 0x2f, slot), (3, 0x35, x), (3, 0x36, y)]
                        events += [(3, 0, points[0][0]), (3, 1, points[0][1])]
                        frame(events)
                        time.sleep(0.008)
            else:
                raise ValueError(request)
            print(json.dumps({'ok': request['op']}), flush=True)
    finally:
        try: release()
        finally:
            ioctl(0x5502, 0)  # UI_DEV_DESTROY
            os.close(descriptor)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--live', action='store_true', help='exercise the current desktop, restoring its state')
    parser.add_argument('--evidence-dir', type=Path, default=Path('artifacts/touchpad/live-x11'))
    args = parser.parse_args()
    if not args.live: parser.error('--live is required for the current-desktop hardware fixture')
    assert os.getuid() != 0, 'Run the fixture as the desktop user; only its uinput helper needs sudo'
    tool = shutil.which('jwm-tool')
    assert tool and shutil.which('xdotool') and shutil.which('xmessage')
    def msg(command, arguments=None):
        argv = [tool, 'msg', command, '--raw']
        if arguments is not None: argv += ['--args', json.dumps(arguments)]
        result = json.loads(subprocess.check_output(argv, text=True, timeout=5))
        assert result['success'], result
        return result.get('data')
    gesture = msg('get_gesture')
    assert gesture['backend'] in ('xcb', 'x11rb'), gesture
    assert gesture['runtime']['operational'], gesture
    bindings = {(item['fingers'], item['direction']): (item['function'], item['argument']) for item in gesture['bindings']}
    for direction, function, argument in [('left', 'loopview', -1), ('right', 'loopview', 1), ('up', 'minimize', 0), ('down', 'restore_preview', 0)]:
        assert bindings.get((3, direction)) == (function, argument), bindings
    shell = msg('get_desktop_shell')
    host_pid = shell['host']['pid']
    values = dict(item.split(b'=', 1) for item in Path(f'/proc/{host_pid}/environ').read_bytes().split(b'\0') if b'=' in item)
    env = dict(os.environ)
    for key in ('DISPLAY', 'XAUTHORITY', 'XDG_RUNTIME_DIR'):
        if key.encode() in values: env[key] = values[key.encode()].decode()
    def run(*argv):
        return subprocess.check_output(list(map(str, argv)), env=env, text=True, timeout=5).strip()
    def wait_for(probe, description):
        deadline = time.monotonic() + 7
        while time.monotonic() < deadline:
            value = probe()
            if value: return value
            time.sleep(0.05)
        raise AssertionError('Timed out: ' + description)
    def active(): return next(mon for mon in msg('get_monitors') if mon['focused'])
    original = active()
    before_windows = msg('get_windows')
    original_window = next((window for window in before_windows if window['id'] == original['selected_id']), None)
    count = len(next(output for output in shell['outputs'] if output['id'] == original['num'])['snapshot']['tags'])
    occupied = 0
    for window in before_windows:
        if window['monitor'] == original['num'] and not (window['is_dock'] or window['is_sticky']): occupied |= window['tags']
    unused = next((1 << n for n in range(count) if not occupied & (1 << n) and not original['active_tags'] & (1 << n)), None)
    assert unused, 'This fixture needs one unused tag'
    pointer = dict(re.findall(r'^(\w+)=(\d+)$', run('xdotool', 'getmouselocation', '--shell'), re.M))
    args.evidence_dir.mkdir(parents=True, exist_ok=True)
    helper_log = (args.evidence_dir / 'uinput.log').open('w')
    helper = subprocess.Popen(['sudo', '-n', sys.executable, str(Path(__file__).resolve()), '--uinput-child'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=helper_log, text=True)
    fixture = None
    record = {'backend': gesture['backend'], 'runtime': gesture['runtime'], 'steps': [], 'status': 'running'}
    def reply():
        assert select.select([helper.stdout], [], [], 7)[0], 'uinput helper timed out'
        line = helper.stdout.readline()
        assert line, f'uinput helper exited: {helper.poll()}'
        return json.loads(line)
    def command(op, **kwargs):
        helper.stdin.write(json.dumps({'op': op, **kwargs}) + '\n'); helper.stdin.flush()
        result = reply()
        assert result == {'ok': op}, result
    def swipe(moves):
        command('swipe', moves=moves)
        command('release')
    try:
        created = reply(); record['virtual_touchpad'] = created['created']
        baseline = gesture['runtime']['fallback_gesture_devices']
        wait_for(lambda: msg('get_gesture')['runtime']['fallback_gesture_devices'] > baseline, 'virtual touchpad discovery')
        msg('view', {'tag': unused})
        title = f'JWM touchpad acceptance {os.getpid()}'
        fixture = subprocess.Popen(['xmessage', '-name', 'JwmGestureFixture', '-title', title, '-buttons', 'Close:0', '-geometry', '640x360+100+100', 'Temporary gesture acceptance window'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        window = wait_for(lambda: next((w for w in msg('get_windows') if w['name'] == title), None), 'temporary test window')
        msg('focus_window', {'id': window['id']})
        index = unused.bit_length() - 1
        left = 1 << ((index - 1) % count)
        positions = [(original['x'] + original['w'] // 2, original['y'] + original['h'] // 2),
                     (original['x'] + 10, original['y'] + original['h'] // 2),
                     (original['x'] + original['w'] - 10, original['y'] + original['h'] // 2),
                     (original['x'] + original['w'] // 2, original['y'] + 10),
                     (original['x'] + original['w'] // 2, original['y'] + original['h'] - 10)]
        for x, y in positions:
            run('xdotool', 'mousemove', x, y); time.sleep(0.12)
            command('swipe', moves=[[-1200, 0]])
            assert active()['active_tags'] == unused, 'gesture committed before finger lift'
            command('release')
            wait_for(lambda: active()['active_tags'] == left, 'three-finger left to left tag')
            swipe([[1200, 0]])
            wait_for(lambda: active()['active_tags'] == unused, 'three-finger right to right tag')
            record['steps'].append({'pointer': [x, y], 'left_right': 'passed', 'release_only_commit': True})
        for moves in [[[-40, 0]], [[-1200, 0], [1100, 0]], [[500, 500]]]:
            swipe(moves); time.sleep(0.25)
            assert active()['active_tags'] == unused, ('cancelled swipe changed tag', moves)
        record['steps'].append({'short_reversal_diagonal_cancel': 'passed'})
        run('xdotool', 'mousemove', original['x'] + original['w'] // 2, original['y'] + original['h'] // 2)
        msg('focus_window', {'id': window['id']})
        swipe([[0, -1200]])
        wait_for(lambda: next((w for w in msg('get_windows') if w['id'] == window['id'] and w['is_minimized']), None), 'three-finger up minimizes only the fixture')
        swipe([[0, 1200]])
        overlay = wait_for(lambda: (value if (value := msg('get_system_ui')).get('active') else None), 'three-finger down opens restore preview')
        assert next(w for w in msg('get_windows') if w['id'] == window['id'])['is_minimized']
        record['steps'].append({'minimize_restore_preview': 'passed', 'overlay': overlay})
        if shutil.which('import'):
            time.sleep(0.3)
            run('import', '-window', 'root', args.evidence_dir / 'restore-preview.png')
        run('xdotool', 'key', 'Return')
        wait_for(lambda: next((w for w in msg('get_windows') if w['id'] == window['id'] and not w['is_minimized']), None), 'confirm restores the selected fixture')
        record['status'] = 'passed'
    finally:
        if helper.poll() is None:
            try: command('release')
            except (AssertionError, BrokenPipeError): pass
            helper.stdin.close()
            try: helper.wait(timeout=3)
            except subprocess.TimeoutExpired: helper.kill(); helper.wait(timeout=3)
        helper_log.close()
        if fixture is not None:
            if msg('get_system_ui').get('active'): run('xdotool', 'key', 'Escape')
            fixture.terminate()
            try: fixture.wait(timeout=3)
            except subprocess.TimeoutExpired: fixture.kill(); fixture.wait(timeout=3)
        msg('view', {'tag': original['active_tags']})
        run('xdotool', 'mousemove', pointer['X'], pointer['Y'])
        if original_window and any(w['id'] == original_window['id'] and w.get('pid') == original_window.get('pid') for w in msg('get_windows')):
            msg('focus_window', {'id': original_window['id']})
        record['restored_active_tags'] = active()['active_tags']
        record['original_active_tags'] = original['active_tags']
        (args.evidence_dir / 'result.json').write_text(json.dumps(record, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(record, ensure_ascii=False))


if __name__ == '__main__':
    if sys.argv[1:] == ['--uinput-child']: uinput_child()
    else: main()
