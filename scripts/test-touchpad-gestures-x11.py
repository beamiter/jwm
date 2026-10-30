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
        frame([item for slot in range(5) for item in [(3, 0x2f, slot), (3, 0x39, -1)]]
              + [(1, key, 0) for key in (0x145, 0x14d, 0x14e, 0x14f, 0x148, 0x14a)])
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
                fingers = request.get('fingers', 3)
                assert fingers in (3, 4, 5)
                points = [[2500 + slot * 1000, 2500 + (slot % 2) * 100] for slot in range(fingers)]
                events = [(1, 0x14a, 1), (1, {3: 0x14e, 4: 0x14f, 5: 0x148}[fingers], 1)]
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
    parser.add_argument('--four-finger', action='store_true', help='also exercise window previews, expose and reversible show desktop')
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
    if args.four_finger:
        for direction, function, argument in [('left', 'window_switcher', -1), ('right', 'window_switcher', 1), ('up', 'toggle_expose', 0), ('down', 'show_desktop', 0)]:
            assert bindings.get((4, direction)) == (function, argument), bindings
    shell = msg('get_desktop_shell')
    host_pid = shell['host']['pid']
    if host_pid is None:
        pids = subprocess.check_output(['pgrep', '-u', str(os.getuid()), '-x', 'jwm'], text=True).split()
        assert len(pids) == 1, 'No unique active JWM process for the fixture environment'
        host_pid = int(pids[0])
    values = dict(item.split(b'=', 1) for item in Path(f'/proc/{host_pid}/environ').read_bytes().split(b'\0') if b'=' in item)
    env = dict(os.environ)
    for key in ('DISPLAY', 'XAUTHORITY', 'XDG_RUNTIME_DIR'):
        if key.encode() in values: env[key] = values[key.encode()].decode()
    def run(*argv, timeout=5):
        return subprocess.check_output(list(map(str, argv)), env=env, text=True, timeout=timeout).strip()
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
    if args.four_finger:
        assert not any(window['monitor'] == original['num'] and window['is_sticky']
                       and not (window['is_dock'] or window.get('is_desktop', False))
                       for window in before_windows), 'The four-finger fixture requires a workspace without existing sticky application windows'
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
    extra_fixtures = []
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
    def swipe(moves, fingers=3):
        command('swipe', moves=moves, fingers=fingers)
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
            run('import', '-limit', 'thread', '1', '-window', 'root', args.evidence_dir / 'restore-preview.png', timeout=20)
        run('xdotool', 'key', 'Return')
        wait_for(lambda: next((w for w in msg('get_windows') if w['id'] == window['id'] and not w['is_minimized']), None), 'confirm restores the selected fixture')
        if args.four_finger:
            fixture_windows = [window]
            for number, color in enumerate(('#b5d8f3', '#e9c89c')):
                extra_title = f'{title} window {number + 2}'
                process = subprocess.Popen(['xmessage', '-name', 'JwmGestureFixture', '-title', extra_title,
                    '-bg', color, '-buttons', 'Close:0', '-geometry', '640x360+100+100',
                    'Four-finger gesture acceptance window'], env=env,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                extra_fixtures.append(process)
                fixture_windows.append(wait_for(lambda: next((w for w in msg('get_windows') if w['name'] == extra_title), None), 'extra fixture window'))
            for x, y in positions:
                run('xdotool', 'mousemove', x, y)
                time.sleep(0.12)
                for dx in (-1200, 1200):
                    before = active()['selected_id']
                    command('swipe', moves=[[dx, 0]], fingers=4)
                    preview = wait_for(lambda: (value if (value := msg('get_gesture')).get('window_preview_active') else None), 'four-finger preview opens before release')
                    assert active()['selected_id'] == before, 'window switch committed before lift'
                    assert active()['active_tags'] == unused, 'window preview switched workspace'
                    assert preview['window_preview_target'] != before
                    if shutil.which('import') and (x, y) == positions[0] and dx < 0:
                        time.sleep(0.25)
                        run('import', '-limit', 'thread', '1', '-window', 'root', args.evidence_dir / 'window-switcher.png', timeout=20)
                    command('release')
                    wait_for(lambda: active()['selected_id'] == preview['window_preview_target'], 'finger lift switches to the previewed window')
                    wait_for(lambda: not msg('get_system_ui')['active'], 'window preview closes')
                record['steps'].append({'pointer': [x, y], 'four_finger_preview_switch': 'passed'})
            before = active()['selected_id']
            swipe([[-1200, 0], [1100, 0]], fingers=4)
            wait_for(lambda: not msg('get_system_ui')['active'], 'reversal cancels window preview')
            assert active()['selected_id'] == before
            record['steps'].append({'four_finger_pull_back_cancel': 'passed'})
            swipe([[0, -1200]], fingers=4)
            wait_for(lambda: msg('get_effects')['expose'], 'four-finger up enters expose')
            swipe([[-1200, 0]], fingers=4)
            swipe([[1200, 0]], fingers=4)
            assert msg('get_effects')['expose']
            swipe([[0, 1200]], fingers=4)
            wait_for(lambda: not msg('get_effects')['expose'], 'four-finger down confirms expose selection')
            assert active()['active_tags'] == unused
            swipe([[0, -1200]], fingers=4)
            wait_for(lambda: msg('get_effects')['expose'], 'reopen expose')
            before = active()['selected_id']
            swipe([[0, -1200]], fingers=4)
            wait_for(lambda: not msg('get_effects')['expose'], 'four-finger up cancels expose')
            assert active()['selected_id'] == before
            record['steps'].append({'four_finger_expose_browse_confirm_cancel': 'passed'})
            msg('focus_window', {'id': fixture_windows[0]['id']})
            msg('minimize')
            wait_for(lambda: next(w for w in msg('get_windows') if w['id'] == fixture_windows[0]['id'])['is_minimized'], 'fixture starts pre-minimized')
            msg('focus_window', {'id': fixture_windows[1]['id']})
            before_focus = active()['selected_id']
            owned = {w['id'] for w in fixture_windows}
            assert active()['active_tags'] == unused, 'show desktop fixture left its isolated tag'
            assert all(w['id'] in owned for w in msg('get_windows')
                       if w['monitor'] == original['num'] and w['is_on_view']
                       and not (w['is_dock'] or w.get('is_desktop', False) or w.get('is_status_bar', False))), 'Another application entered the test view'
            before_minimized = {w['id']: w['is_minimized'] for w in msg('get_windows') if w['id'] in owned}
            command('swipe', moves=[[0, 1200]], fingers=4)
            assert not msg('get_gesture')['showing_desktop'], 'show desktop committed before finger lift'
            command('release')
            wait_for(lambda: msg('get_gesture')['showing_desktop'], 'four-finger down shows desktop')
            assert all(w['is_minimized'] for w in msg('get_windows') if w['id'] in owned)
            swipe([[0, 1200]], fingers=4)
            wait_for(lambda: not msg('get_gesture')['showing_desktop'], 'four-finger down restores desktop snapshot')
            after_minimized = {w['id']: w['is_minimized'] for w in msg('get_windows') if w['id'] in owned}
            assert after_minimized == before_minimized, (before_minimized, after_minimized)
            assert active()['selected_id'] == before_focus
            record['steps'].append({'four_finger_show_desktop_exact_restore': 'passed'})
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
            if msg('get_system_ui').get('active') or msg('get_effects')['expose']:
                run('xdotool', 'key', 'Escape')
            if args.four_finger and msg('get_gesture').get('showing_desktop'):
                msg('show_desktop')
            for process in [*reversed(extra_fixtures), fixture]:
                process.terminate()
                try: process.wait(timeout=3)
                except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=3)
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
