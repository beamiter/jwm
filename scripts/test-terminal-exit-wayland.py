#!/usr/bin/env python3
"""Diagnostic native Wayland exit fixture, on an owned Xvfb nested output.

A failing invariant always produces status=failure and exits nonzero, including
--diagnose. X is only the output host/input injector, never the client protocol.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import tempfile
import time
import traceback
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('exit_x11', ROOT / 'scripts/test-terminal-exit-x11.py')
X11 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(X11)


def events(path):
    if not path.exists():
        return []
    return [line.split() for line in path.read_text().splitlines() if len(line.split()) >= 3]


def key_received(rows, key):
    return any(row[2] == 'key' and len(row) >= 7 and row[5] == str(key)
               and 'Pressed' in row[6] for row in rows)


def native_row(rows, pid):
    return X11.producer_row(rows, {'pid': pid})


def first_capture_then_focus(grab, read_focus, clock=time.monotonic):
    """Keep IPC off the first capture path; timestamps bound the whole grab."""
    begin = clock()
    image = grab()
    end = clock()
    focus_row = read_focus()
    focus_observed = clock()
    return (begin, image, end), focus_row, focus_observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', choices=['wayland-winit', 'wayland-x11'], default='wayland-winit')
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--client-bin', type=Path, default=ROOT / 'crates/jwm_shell/target/debug/examples/native_shell_client')
    parser.add_argument('--evidence-dir', type=Path, default=ROOT / 'artifacts/terminal-exit-wayland')
    parser.add_argument('--diagnose', action='store_true', help='Collect later cases after a failed invariant; never converts failure to success')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Offline)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    output = args.evidence_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {'status': 'running', 'backend': args.backend, 'transport': 'native wl_shm/xdg-shell',
              'cases': [], 'failures': [], 'limits': ['Above unsupported: no native xdg-shell above request/IPC',
                'No OR, password lock, DRM/KMS or physical GPU validation',
                'Pointer during-hide probe covers ordinary floating overlap; other cases check after-hide receipt',
                'Host XGetImage captures presented native composition, not XWayland clients']}
    children, pending, capture = [], {}, None
    term_pid = None
    with tempfile.TemporaryDirectory(prefix='jwm-native-exit-') as directory:
        work = Path(directory)
        env = {k: v for k, v in os.environ.items() if not k.startswith(('JWM_', 'OPENAI_', 'PULSE_', 'PIPEWIRE_', 'WATCHDOG')) and k not in
               ('DISPLAY', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'WINIT_UNIX_BACKEND', 'NOTIFY_SOCKET',
                'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'DBUS_SYSTEM_BUS_ADDRESS')}
        for key, leaf in [('HOME', 'home'), ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'),
                          ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state')]:
            (work / leaf).mkdir(mode=0o700)
            env[key] = str(work / leaf)
        env.update(LIBGL_ALWAYS_SOFTWARE='1', WINIT_X11_SCALE_FACTOR='1',
                   DBUS_SYSTEM_BUS_ADDRESS='unix:path=' + str(work / 'absent-system-bus'))
        log = (output / 'session.log').open('w')
        def run(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, stderr=log, text=True, timeout=15)
        def spawn(argv, child_env=None, **kwargs):
            process = subprocess.Popen(list(map(str, argv)), env=child_env or env, stdout=log, stderr=log,
                                       start_new_session=True, **kwargs)
            children.append(process)
            return process
        def wait(predicate, label, timeout=15):
            deadline, last = time.monotonic() + timeout, None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    last = str(error)
                time.sleep(.02)
            raise AssertionError(f'{label}: timeout; last={last}')
        def msg(name, values=None):
            argv = [args.bin_dir.resolve() / 'jwm-tool', 'msg', name, '--raw']
            if values is not None:
                argv += ['--args', json.dumps(values)]
            response = json.loads(run(*argv))
            assert response.get('success', True), response
            return response.get('data', response)
        def check(condition, message):
            if not condition:
                report['failures'].append(message)
        def hotkey():
            start = time.monotonic()
            run('xdotool', 'key', '--clearmodifiers', 'alt+shift+n')
            return start, time.monotonic()
        try:
            for tool in ('Xvfb', 'xdotool', 'dbus-run-session'):
                assert shutil.which(tool), f'missing {tool}'
            for executable in (args.bin_dir / 'jwm', args.bin_dir / 'jwm-tool', args.client_bin):
                assert executable.is_file() and os.access(executable, os.X_OK), f'missing executable {executable}'
            read, write = os.pipe()
            try:
                spawn(['Xvfb', '-displayfd', write, '-screen', '0', '1280x800x24', '-nolisten', 'tcp', '-noreset'], pass_fds=(write,))
                os.close(write)
                write = None
                assert select.select([read], [], [], 10)[0], 'Xvfb readiness timeout'
                env['DISPLAY'] = ':' + os.read(read, 32).decode().strip()
            finally:
                os.close(read)
                if write is not None:
                    os.close(write)
            binary = args.bin_dir.resolve() / 'jwm'
            run(binary, '--backend', args.backend, '--gen-config')
            config = work / 'config/jwm/config_wayland.toml'
            text = config.read_text()
            for section, key, value, required in [('status_bar', 'show_bar', 'false', True),
                    ('status_bar', 'name', '"/bin/true"', True), ('desktop_shell', 'enabled', 'false', False),
                    ('animation', 'enabled', 'true', True), ('animation', 'duration_ms', '1000', True),
                    ('animation', 'speed', '"normal"', True), ('animation', 'easing', '"ease-out"', True)]:
                text = X11.FIXTURE.edit_setting(text, section, key, value, required)
            config.write_text(text)
            (output / 'config_wayland.toml').write_text(text)
            run(binary, '--backend', args.backend, '--check-config')
            spawn(['dbus-run-session', '--', binary, '--backend', args.backend])
            outer = int(wait(lambda: run('xdotool', 'search', '--name', 'JWM ').splitlines()[0], 'outer output'))
            run('xdotool', 'windowsize', outer, 1280, 800)
            run('xdotool', 'windowfocus', outer)
            wait(lambda: any(row['w'] == 1280 and row['h'] == 800 for row in msg('get_monitors')), 'physical nested output')
            wayland = wait(lambda: next((p.name for p in (work / 'runtime').glob('wayland-*') if p.is_socket()), None), 'owned native socket')
            capture = X11.RootCapture(env['DISPLAY'])
            capture.root = outer  # Capture output drawable, independent of host position/borders.
            report['outer_xid'] = outer
            report['wayland_socket'] = wayland
            report['client_environment'] = {'DISPLAY': None, 'WAYLAND_DISPLAY': wayland, 'XDG_RUNTIME_DIR': str(work / 'runtime')}
            term_log = output / 'term.events'
            term_log.write_text('')
            # env exec preserves PID; DISPLAY is absent in the real native client.
            command = ['/usr/bin/env', '-u', 'DISPLAY', 'WAYLAND_DISPLAY=' + wayland,
                       'JWM_TEST_EVENT_LOG=' + str(term_log), str(args.client_bin.resolve()),
                       '--title', 'NativeExitTerm', '--color', 'ef2020']
            msg('togglescratchpad', {'cmd': ['term'] + command})
            term_pid = int(wait(lambda: events(term_log), 'native terminal start')[0][0])
            term = wait(lambda: native_row(msg('get_windows'), term_pid), 'native terminal PID mapping')
            report['term'] = term
            assert term['is_scratchpad'] and msg('get_scratchpads').get('term') == term['id'], 'native scratchpad identity'
            time.sleep(1.2)
            for kind in ('floating', 'pip', 'fullscreen'):
                case = {'case': kind, 'frames': []}
                report['cases'].append(case)
                bg_log = output / (kind + '.events')
                bg_log.write_text('')
                native_env = dict(env, WAYLAND_DISPLAY=wayland, JWM_TEST_EVENT_LOG=str(bg_log))
                native_env.pop('DISPLAY', None)
                argv = [args.client_bin.resolve(), '--title', 'NativeExitBackground', '--color', '2020ef']
                if kind == 'fullscreen':
                    argv.append('--fullscreen')
                background = spawn(argv, native_env)
                try:
                    bg = wait(lambda: native_row(msg('get_windows'), background.pid), 'native background PID mapping')
                    msg('focus_window', {'id': bg['id']})
                    if kind == 'pip':
                        msg('togglepip')
                    elif kind == 'floating' and not bg['is_floating']:
                        msg('togglefloating')
                    time.sleep(1.2)
                    if native_row(msg('get_windows'), term_pid)['tags'] != 0:
                        hotkey(); time.sleep(1.2)
                    hotkey(); time.sleep(1.2)
                    term = native_row(msg('get_windows'), term_pid)
                    bg = native_row(msg('get_windows'), background.pid)
                    case.update(terminal=term, background=bg)
                    expected_flag = {'floating': 'is_floating', 'pip': 'is_pip', 'fullscreen': 'is_fullscreen'}[kind]
                    assert bg.get(expected_flag), f'{kind}: background state {expected_flag} not established'
                    assert term['is_focused'], 'terminal reveal focus'
                    # Real protocol ACK is prerequisite, not inferred from IPC focus.
                    key_offset = len(events(term_log))
                    run('xdotool', 'key', 'b')
                    wait(lambda: key_received(events(term_log)[key_offset:], 48), 'native terminal receives b')
                    pending[kind + '-before'] = capture.grab()
                    term_offset, bg_offset = len(events(term_log)), len(events(bg_log))
                    started, finished = hotkey()
                    case.update(key_start=started, key_finish=finished)
                    first_frame, focus_row, focus_observed = first_capture_then_focus(
                        capture.grab, lambda: native_row(msg('get_windows'), term_pid))
                    case['focus_observation_seconds'] = focus_observed-started
                    dispatch_bound = max(finished, focus_observed)-started
                    check(not focus_row['is_focused'], kind+': terminal focus remains after hide')
                    check(focus_observed-started <= .2, kind+': focus observation exceeded 200ms')
                    # Input probe during exit, without forcibly changing compositor focus.
                    deadline = started + 1.15
                    for index in range(240):
                        if index == 0:
                            begin, image, end = first_frame
                        else:
                            begin = time.monotonic()
                            image = capture.grab()
                            end = time.monotonic()
                        name = f'{kind}-{index:03}'
                        pending[name] = image
                        rect = X11.overlap_during_capture(term, bg, begin-started, end-started, dispatch_bound)
                        sample = {'seconds': end-started, 'capture_start': begin-started, 'png': name+'.png',
                                  'rect': rect, 'checked': False, 'total_term_pixels': X11.color_pixels(image, 'term')}
                        if rect and (rect[2]-rect[0])*(rect[3]-rect[1]) >= 400:
                            area = (rect[2]-rect[0])*(rect[3]-rect[1])
                            sample.update(checked=True, red_fraction=X11.color_pixels(image, 'term', rect)/area)
                            check(sample['red_fraction'] > .9, f'{kind}: occluded exit frame {index}')
                        case['frames'].append(sample)
                        if index == 0:
                            # Capture the earliest frame before another synchronous
                            # key command consumes the short PiP overlap interval.
                            run('xdotool', 'key', 'c')
                        if end >= deadline:
                            break
                        time.sleep(.008)
                    horizon = X11.overlap_horizon(term, bg, dispatch_bound)
                    case['horizon'] = horizon
                    try:
                        X11.assert_full_coverage(case['frames'], horizon, kind)
                    except AssertionError as error:
                        check(False, str(error))
                    term_events, bg_events = events(term_log)[term_offset:], events(bg_log)[bg_offset:]
                    case.update(term_events=term_events, background_events=bg_events)
                    check(any(row[2] == 'keyboard_leave' for row in term_events), kind+': no terminal keyboard leave')
                    check(not key_received(term_events, 46), kind+': hidden terminal received c')
                    check(key_received(bg_events, 46), kind+': background did not receive c during hide')
                    check(X11.color_pixels(image, 'term') < 40, kind+': terminal remains after completion')
                    click_offset = len(events(bg_log))
                    run('xdotool', 'mousemove', '--window', outer, bg['x']+bg['w']//2, bg['y']+bg['h']//2)
                    run('xdotool', 'click', '1')
                    wait(lambda: any(row[2] == 'button' for row in events(bg_log)[click_offset:]), 'native background pointer ACK')
                    case['pointer_ack_after_hide'] = events(bg_log)[click_offset:]
                    if kind == 'floating':
                        # Separate input trial: click where the terminal is visibly
                        # still sliding, but must already be ineligible for input.
                        hotkey(); time.sleep(1.2)
                        input_term = native_row(msg('get_windows'), term_pid)
                        input_bg = native_row(msg('get_windows'), background.pid)
                        region = X11.overlap_during_capture(input_term, input_bg, 0, .3)
                        assert region, 'no conservative visible region for pointer trial'
                        point = ((region[0]+region[2])//2, (region[1]+region[3])//2)
                        enter_offset = len(events(term_log))
                        run('xdotool', 'mousemove', '--window', outer, 2, 2)
                        run('xdotool', 'mousemove', '--window', outer, *point)
                        wait(lambda: any(row[2] == 'pointer_enter' for row in events(term_log)[enter_offset:]),
                             'terminal pointer focus before hiding')
                        toff, boff = len(events(term_log)), len(events(bg_log))
                        input_start, _ = hotkey()
                        # Deliberately do not move the pointer: hiding must retarget
                        # a stationary pointer, not rely on a later motion event.
                        run('xdotool', 'click', '1')
                        wait(lambda: any(row[2] == 'button' for row in events(bg_log)[boff:]),
                             'background pointer during terminal exit', timeout=.3)
                        input_elapsed = time.monotonic()-input_start
                        check(input_elapsed <= .3, 'pointer delivery was too late to prove exit-time handoff')
                        check(not any(row[2] == 'button' for row in events(term_log)[toff:]),
                              'hidden terminal received pointer button during exit')
                        case['pointer_during_hide'] = {'point': point, 'elapsed': input_elapsed,
                            'terminal_events': events(term_log)[toff:], 'background_events': events(bg_log)[boff:]}
                        pending['floating-pointer-during-hide'] = capture.grab()
                        time.sleep(1.2)
                    # Actual rapid hide/show reversal and native delivery after settle.
                    hotkey(); time.sleep(1.2)
                    hotkey(); time.sleep(.18); hotkey(); time.sleep(1.2)
                    offset = len(events(term_log))
                    run('xdotool', 'key', 'd')
                    wait(lambda: key_received(events(term_log)[offset:], 32), 'native key after reversal')
                    pending[kind+'-reversed'] = capture.grab()
                    case['reversal_key_received'] = True
                except Exception as error:
                    case['error'] = str(error)
                    report['failures'].append(kind+': '+str(error))
                    if not args.diagnose:
                        raise
                finally:
                    if background.poll() is None:
                        os.killpg(background.pid, signal.SIGTERM)
                        background.wait(timeout=5)
                    for name, image in pending.items():
                        image.save(output / (name+'.png'))
                    pending.clear()
                if report['failures'] and not args.diagnose:
                    break
        except Exception as error:
            report['failures'].append(str(error))
            report['traceback'] = traceback.format_exc()
        finally:
            # Scratchpad was spawned by the compositor, not subprocess.Popen.
            # Terminate only the PID authenticated by our private event log.
            if term_pid is not None:
                try:
                    os.kill(term_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if capture:
                report['capture_format'] = capture.metadata
                capture.close()
            for name, image in pending.items():
                image.save(output / (name+'.png'))
            for child in reversed(children):
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=4)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=4)
            log.close()
            report['status'] = 'failure' if report['failures'] else 'success'
            (output / 'report.json').write_text(json.dumps(report, indent=2)+'\n')
            print(json.dumps(report, indent=2))
    return 1 if report['failures'] else 0


class Offline(unittest.TestCase):
    def test_pid_not_window_id(self):
        self.assertEqual(native_row([{'id': 4, 'pid': 201}], 201)['id'], 4)
        self.assertIsNone(native_row([{'id': 201, 'pid': 99}], 201))
    def test_keyboard_ack(self):
        rows = [['201', '100', 'key', '8', '22', '48', 'Value(Pressed)']]
        self.assertTrue(key_received(rows, 48))
        self.assertFalse(key_received(rows, 46))
        self.assertFalse(key_received([['201', '100', 'keyboard_enter', '8']], 48))
    def test_coverage_is_strict(self):
        with self.assertRaises(AssertionError):
            X11.assert_full_coverage([{'checked': True, 'seconds': .2}], .8, 'offline')
    def test_first_capture_precedes_focus_without_extra_grab(self):
        calls = []
        ticks = iter([10.049, 10.085, 10.097])
        image, focus = object(), {'is_focused': False}
        def grab():
            calls.append('grab')
            return image
        def read_focus():
            calls.append('focus')
            return focus
        first, observed_focus, observed_at = first_capture_then_focus(
            grab, read_focus, lambda: next(ticks))
        self.assertEqual(calls, ['grab', 'focus'])
        self.assertEqual(first, (10.049, image, 10.085))
        self.assertIs(observed_focus, focus)
        self.assertEqual(observed_at, 10.097)
        # A grab beginning early but finishing late is NOT early evidence.
        with self.assertRaisesRegex(AssertionError, 'missing early'):
            X11.assert_full_coverage([
                {'checked': True, 'seconds': .085},
                {'checked': True, 'seconds': .15},
                {'checked': True, 'seconds': .23},
                {'checked': False, 'seconds': 1.15}], .248, 'offline')
    def test_slow_first_capture_counts_against_focus_deadline(self):
        ticks = iter([20.04, 20.201, 20.21])
        _, _, observed_at = first_capture_then_focus(
            lambda: object(), lambda: {'is_focused': False}, lambda: next(ticks))
        # The caller still measures from key start, not from capture completion.
        self.assertGreater(observed_at - 20., .2)
    def test_event_log(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'events'
            self.assertEqual(events(path), [])
            path.write_text('201 100 start\n201 101 keyboard_leave 5\n')
            self.assertEqual(events(path)[1][2], 'keyboard_leave')


if __name__ == '__main__':
    raise SystemExit(main())
