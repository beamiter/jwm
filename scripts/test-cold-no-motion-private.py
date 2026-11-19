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


def cold_key_received(rows, key):
    return any(len(row) >= 7 and row[2] == 'key' and row[5] == str(key)
               and row[6] in ('Pressed', 'Value(Pressed)') for row in rows)


def pointer_click_received(rows):
    return all(any(len(row) >= 7 and row[2] == 'button' and row[5] == '272'
                   and row[6] in (state, f'Value({state})') for row in rows)
               for state in ('Pressed', 'Released'))


def assert_cold_no_motion(config, trial):
    assert config.get('motion_enabled') is False and config.get('animation_enabled') is False, 'cold disable not confirmed by product IPC'
    assert trial['before_term_pixels'] > 1000, 'terminal not visibly shown before hide'
    assert trial['hidden_observation_seconds'] <= .2, 'hidden state observation exceeded 200ms'
    assert trial['hidden_row']['tags'] == 0 and not trial['hidden_row']['is_focused'], 'hide did not release state/focus'
    frames = trial['frames']
    assert len(frames) >= 3, 'missing no-motion timeline'
    assert frames[0]['seconds'] <= .2, 'no clear-frame evidence within 200ms of hide key'
    assert frames[-1]['seconds'] >= 1.05, 'no-motion capture ended too early'
    assert all(frame['capture_start'] >= trial['hidden_observation_seconds'] for frame in frames), 'pre-confirmation capture is not no-residue proof'
    assert all(frame['total_term_pixels'] < 40 for frame in frames), 'terminal residue after confirmed hide'
    assert trial['final_row']['tags'] == 0 and not trial['final_row']['is_focused'], 'terminal reclaimed focus/state'
    assert any(row[2] == 'keyboard_leave' for row in trial['term_events']), 'terminal missing keyboard leave'
    assert not any(row[2] == 'keyboard_enter' for row in trial['term_events']), 'hidden terminal regained keyboard focus'
    assert cold_key_received(trial['background_events'], 46), 'background missing c acknowledgement'
    assert not cold_key_received(trial['term_events'], 46), 'hidden terminal received c'
    assert pointer_click_received(trial['background_events']), 'background missing complete pointer acknowledgement'
    assert trial['input_observation_seconds'] <= .3, 'input handoff evidence exceeded 300ms'
    assert not any(row[2] == 'button' for row in trial['term_events']), 'hidden terminal received pointer button'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', choices=['wayland-winit', 'wayland-x11'], default='wayland-winit')
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--client-bin', type=Path, default=ROOT / 'crates/jwm_shell/target/debug/examples/native_shell_client')
    parser.add_argument('--evidence-dir', type=Path, default=ROOT / 'artifacts/terminal-exit-wayland')
    parser.add_argument('--diagnose', action='store_true', help='Collect later cases after a failed invariant; never converts failure to success')
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--cold-no-motion', action='store_true', help='separate winit-only cold enabled=false acceptance; does not alter animated strict gates')
    args = parser.parse_args()
    if not args.self_test and not args.cold_no_motion:
        parser.error('isolated copied runner requires --cold-no-motion')
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Offline)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.cold_no_motion and args.backend != 'wayland-winit':
        parser.error('--cold-no-motion supports only wayland-winit')
    output = args.evidence_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {'status': 'running', 'backend': args.backend, 'transport': 'native wl_shm/xdg-shell',
              'cases': [], 'failures': [], 'limits': ['Above unsupported: no native xdg-shell above request/IPC',
                'No OR, password lock, DRM/KMS or physical GPU validation',
                'Pointer during-hide probe covers ordinary floating overlap; other cases check after-hide receipt',
                'Host XGetImage captures presented native composition, not XWayland clients']}
    report['mode'] = 'cold-enabled-false' if args.cold_no_motion else 'animated-strict'
    if args.cold_no_motion:
        report['limits'].append('No frame-request counter; no claim about redundant render requests. Cold disable only, not instant speed, zero duration or hot reload. Separate 200ms end-to-end hide and 300ms observed input bounds, not animated PiP early coverage. Residue checks cover sampled frames, not every presented frame; synchronous input receipts can leave a sampling gap.')
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
            # Normal private X server owns its display lock; nested Xwayland
            # must not mistake this host's socket for an unlocked stale one.
            display_number = next((number for number in range(180, 200)
                if not Path(f'/tmp/.X{number}-lock').exists()
                and not Path(f'/tmp/.X11-unix/X{number}').exists()), None)
            assert display_number is not None, 'no private X display slot'
            env['DISPLAY'] = f':{display_number}'
            host = spawn(['Xvfb', env['DISPLAY'], '-screen', '0', '1280x800x24',
                          '-nolisten', 'tcp', '-noreset'])
            host_lock = Path(f'/tmp/.X{display_number}-lock')
            host_socket = Path(f'/tmp/.X11-unix/X{display_number}')
            wait(lambda: host.poll() is None and host_lock.exists()
                 and int(host_lock.read_text().strip()) == host.pid and host_socket.is_socket(),
                 'owned Xvfb display lock/socket', timeout=10)
            run('xdotool', 'getdisplaygeometry')
            report['host_display'] = {'display': env['DISPLAY'], 'pid': host.pid,
                                      'lock_pid': int(host_lock.read_text().strip())}
            binary = args.bin_dir.resolve() / 'jwm'
            run(binary, '--backend', args.backend, '--gen-config')
            config = work / 'config/jwm/config_wayland.toml'
            text = config.read_text()
            for section, key, value, required in [('status_bar', 'show_bar', 'false', True),
                    ('status_bar', 'name', '"/bin/true"', True), ('desktop_shell', 'enabled', 'false', False),
                    ('animation', 'enabled', 'false' if args.cold_no_motion else 'true', True), ('animation', 'duration_ms', '1000', True),
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
            assert host.poll() is None and int(host_lock.read_text().strip()) == host.pid, 'host display ownership changed after nested Xwayland startup'
            if args.cold_no_motion:
                config_readback = msg('get_config', {'keys': ['motion_enabled', 'animation_enabled']})
                report['config_readback'] = config_readback
                assert config_readback.get('motion_enabled') is False and config_readback.get('animation_enabled') is False, 'cold disable not confirmed by product IPC'
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
            for kind in (('floating',) if args.cold_no_motion else ('floating', 'pip', 'fullscreen')):
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
                    if args.cold_no_motion:
                        case['no_motion_trials'] = []
                        for trial_index in range(2):
                            if trial_index:
                                show_start, _ = hotkey()
                                wait(lambda: native_row(msg('get_windows'), term_pid)['is_focused'], 'rapid show focused', timeout=.3)
                                offset = len(events(term_log))
                                run('xdotool', 'key', 'b')
                                wait(lambda: key_received(events(term_log)[offset:], 48), 'rapid show real keyboard receipt', timeout=.3)
                            term_now = native_row(msg('get_windows'), term_pid)
                            region = X11.overlap_during_capture(term_now, bg, 0, 0)
                            assert region, 'no shared terminal/background pointer target'
                            x, y = (region[0]+region[2])//2, (region[1]+region[3])//2
                            enter = len(events(term_log))
                            run('xdotool', 'mousemove', '--window', outer, 2, 2)
                            run('xdotool', 'mousemove', '--window', outer, x, y)
                            wait(lambda: any(row[2] == 'pointer_enter' for row in events(term_log)[enter:]), 'terminal pointer before cold hide')
                            before = capture.grab()
                            pending[f'cold-{trial_index}-before'] = before
                            before_pixels = X11.color_pixels(before, 'term')
                            term_offset, bg_offset = len(events(term_log)), len(events(bg_log))
                            started, finished = hotkey()
                            trial = {'name': 'hide' if not trial_index else 'rapid-show-hide', 'key_start': started,
                                     'key_finish': finished, 'before_term_pixels': before_pixels, 'frames': []}
                            case['no_motion_trials'].append(trial)
                            if trial_index:
                                trial['show_to_hide_seconds'] = finished-show_start
                                check(trial['show_to_hide_seconds'] <= .3, 'rapid show-hide exceeded 300ms')
                            trial['hidden_row'] = native_row(msg('get_windows'), term_pid)
                            trial['hidden_observation_seconds'] = time.monotonic()-started
                            for index in range(240):
                                begin = time.monotonic()
                                image = capture.grab()
                                end = time.monotonic()
                                name = f'cold-{trial_index}-{index:03}'
                                pending[name] = image
                                trial['frames'].append({'capture_start': begin-started, 'seconds': end-started,
                                                        'png': name+'.png', 'total_term_pixels': X11.color_pixels(image, 'term')})
                                if index == 0:
                                    # No pointer motion after hiding: prove stationary handoff.
                                    run('xdotool', 'click', '1')
                                    run('xdotool', 'key', 'c')
                                    wait(lambda: pointer_click_received(events(bg_log)[bg_offset:])
                                         and cold_key_received(events(bg_log)[bg_offset:], 46),
                                         'cold background keyboard and button receipts', timeout=.3)
                                    trial['input_observation_seconds'] = time.monotonic()-started
                                if end >= started + 1.15:
                                    break
                                time.sleep(.008)
                            trial['term_events'] = events(term_log)[term_offset:]
                            trial['background_events'] = events(bg_log)[bg_offset:]
                            trial['final_row'] = native_row(msg('get_windows'), term_pid)
                            try:
                                assert_cold_no_motion(config_readback, trial)
                            except AssertionError as error:
                                check(False, str(error))
                        continue
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
    def cold_trial(self):
        return {'before_term_pixels': 2000, 'hidden_observation_seconds': .04, 'input_observation_seconds': .19,
                'hidden_row': {'tags': 0, 'is_focused': False},
                'final_row': {'tags': 0, 'is_focused': False},
                'frames': [{'capture_start': t-.01, 'seconds': t, 'total_term_pixels': 0}
                           for t in [.06, .5, 1.15]],
                'term_events': [['201', '100', 'keyboard_leave']],
                'background_events': [['202', '101', 'button', '9', '100', '272', 'Pressed'],
                                      ['202', '102', 'button', '10', '101', '272', 'Released'],
                                      ['202', '102', 'key', '8', '22', '46', 'Pressed']]}

    def test_cold_disable_requires_product_readback_and_actual_evidence(self):
        good = {'motion_enabled': False, 'animation_enabled': False}
        assert_cold_no_motion(good, self.cold_trial())
        for config in [{}, {'motion_enabled': False}, {'motion_enabled': True, 'animation_enabled': False}]:
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(config, self.cold_trial())
        for field in ['term_events', 'background_events', 'frames']:
            trial = self.cold_trial()
            trial[field] = []
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(good, trial)

    def test_cold_residue_late_clear_and_preconfirmation_frames_fail(self):
        good = {'motion_enabled': False, 'animation_enabled': False}
        for mutate in [lambda t: t['frames'][1].update(total_term_pixels=40),
                       lambda t: t['frames'][0].update(seconds=.201),
                       lambda t: t['frames'][0].update(capture_start=.03),
                       lambda t: t['frames'][-1].update(seconds=1.04),
                       lambda t: t.update(before_term_pixels=0)]:
            trial = self.cold_trial()
            mutate(trial)
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(good, trial)

    def test_cold_partial_malformed_or_late_receipt_fails(self):
        good = {'motion_enabled': False, 'animation_enabled': False}
        for mutate in [lambda t: t.update(input_observation_seconds=.301),
                       lambda t: t['background_events'].pop(0),
                       lambda t: t['background_events'].__setitem__(0, ['202', '101', 'button'])]:
            trial = self.cold_trial()
            mutate(trial)
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(good, trial)

    def test_cold_unknown_states_and_wrong_codes_fail(self):
        good = {'motion_enabled': False, 'animation_enabled': False}
        for index, field, value in [(0, 6, 'UnknownPressed'), (1, 6, 'NotReleased'),
                                    (2, 6, 'UnknownPressed'), (0, 5, '273'), (2, 5, '47')]:
            trial = self.cold_trial()
            trial['background_events'][index][field] = value
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(good, trial)

    def test_cold_input_or_focus_reclaimed_fails(self):
        good = {'motion_enabled': False, 'animation_enabled': False}
        for event in [['201', '103', 'keyboard_enter'], ['201', '103', 'button'],
                      ['201', '103', 'key', '8', '22', '46', 'Pressed']]:
            trial = self.cold_trial()
            trial['term_events'].append(event)
            with self.assertRaises(AssertionError):
                assert_cold_no_motion(good, trial)

    def test_event_log(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'events'
            self.assertEqual(events(path), [])
            path.write_text('201 100 start\n201 101 keyboard_leave 5\n')
            self.assertEqual(events(path)[1][2], 'keyboard_leave')


if __name__ == '__main__':
    raise SystemExit(main())
