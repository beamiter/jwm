#!/usr/bin/env python3
"""Private X11 acceptance for sparse workspaces and the sidebar mode shortcut."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/debug')
    parser.add_argument('--compat-helper', type=Path, help='bind Super Shift B to an existing spawn action for older JWM builds')
    parser.add_argument('--backend', choices=['xcb', 'x11rb'], default='x11rb')
    parser.add_argument('--bar-bin', type=Path, default=ROOT / 'bars/tao_glow_bar/target/debug/tao_glow_bar')
    parser.add_argument('--shell-bin', type=Path, default=ROOT / 'crates/jwm_shell/target/release/jwm-shell')
    parser.add_argument('--evidence-dir', type=Path, default=ROOT / 'artifacts/shared-shell/sidebar-mode')
    args = parser.parse_args()
    evidence = (args.evidence_dir / args.backend).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    result = {'status': 'running', 'backend': args.backend, 'binding': 'compat-helper' if args.compat_helper else 'native', 'steps': [], 'fixture_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    children = []
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='jwm-sidebar-mode-') as directory:
        work = Path(directory)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE='1', GDK_BACKEND='x11', JWM_SHELL_BIN=str(args.shell_bin.resolve()), JWM_COMPOSITOR_API='glx', JWM_DEBUG_KEYS='1')
        for key in ['DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'JWM_SOCKET', 'JWM_BACKEND', 'JWM_DESKTOP_SHELL', 'JWM_BAR_LAYOUT']:
            env.pop(key, None)
        for key, folder in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'), ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_PICTURES_DIR', 'pictures')]:
            (work / folder).mkdir(mode=0o700)
            env[key] = str(work / folder)
        (work / 'bin').mkdir()
        (work / 'bin/tao_glow_bar').symlink_to(args.bar_bin.resolve())
        env['PATH'] = str(work / 'bin') + os.pathsep + env.get('PATH', '')
        env['XBAR_CONFIG'] = str(work / 'xbar.toml')
        (work / 'xbar.toml').write_text('theme="dark"\n')
        log = (evidence / 'session.log').open('w')

        def run(*argv):
            return subprocess.check_output(list(map(str, argv)), env=env, text=True, stderr=subprocess.STDOUT, timeout=20)

        def spawn(*argv, **kwargs):
            child = subprocess.Popen(list(map(str, argv)), env=env, stdout=log, stderr=log, start_new_session=True, **kwargs)
            children.append(child)
            return child

        def wait_for(predicate, label, timeout=30):
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError, StopIteration, AssertionError, subprocess.SubprocessError) as error:
                    last = str(error) + ('; output=' + str(error.output) if isinstance(error, subprocess.CalledProcessError) else '')
                time.sleep(.08)
            raise AssertionError(f'{label}; last={last}')

        def msg(command, values=None):
            argv = [args.bin_dir / 'jwm-tool', 'msg', command, '--raw']
            if values is not None:
                argv += ['--args', json.dumps(values)]
            reply = json.loads(run(*argv))
            assert reply.get('success', True), reply
            return reply.get('data', reply)

        def state():
            return msg('get_desktop_shell')

        def record(step):
            result['steps'].append(step)
            print('PASS:', step, flush=True)

        def visible(window):
            return 'IsViewable' in run('xwininfo', '-id', window)

        def check_identity():
            current = state()
            assert current['host']['pid'] == host, current
            assert current['host']['top_bars'][0]['pid'] == top, current
            assert current['outputs'][0]['visible'], current
            return current

        def modes(left, right, bottom):
            current = check_identity()
            result['last_observed_modes'] = current['config']
            return current if [current['config'][key] for key in ['left', 'right', 'bottom']] == [left, right, bottom] else None

        def saved_modes():
            run(args.bin_dir / 'jwm', '--backend', args.backend, '--check-config')
            table = re.search(r'(?ms)^\[desktop_shell\]\s*\n(.*?)(?=^\[|\Z)', config.read_text()).group(1)
            values = {}
            for key in ['enabled', 'left', 'right', 'bottom']:
                raw = re.search(r'(?m)^' + key + r'\s*=\s*([^#\n]+)', table).group(1).strip()
                values[key] = raw == 'true' if key == 'enabled' else raw.strip("\"'")
            return values

        def ipc_toggle():
            if args.compat_helper:
                run(args.compat_helper.resolve(), '--tool', args.bin_dir / 'jwm-tool')
            else:
                msg('toggle_desktop_shell_mode')

        def shortcut():
            run('xdotool', 'key', '--clearmodifiers', '--delay', '80', 'Super_L+Shift_L+b')

        def configuration(key, value):
            if args.compat_helper:
                # A pending file watcher must not overwrite an intentionally
                # transient set_config override between shortcut presses.
                # Model a user edit, then synchronously adopt that revision.
                field = key.split('.', 1)[1]
                text = config.read_text()
                section = re.search(r'(?ms)^\[desktop_shell\]\s*\n(?P<body>.*?)(?=^\[|\Z)', text)
                body, count = re.subn(r'(?m)^' + field + r'\s*=.*$', field + '=' + json.dumps(value), section['body'])
                assert count == 1
                config.write_text(text[:section.start('body')] + body + text[section.end('body'):])
                run(args.bin_dir / 'jwm', '--backend', args.backend, '--check-config')
                msg('reload_config')
            else:
                msg('set_config', {'key': key, 'value': value})

        try:
            read_fd, write_fd = os.pipe()
            try:
                spawn('Xvfb', '-displayfd', write_fd, '-screen', '0', '1280x800x24', '-nolisten', 'tcp', '-noreset', pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                number = bytearray()
                deadline = time.monotonic() + 10
                while b'\n' not in number:
                    assert select.select([read_fd], [], [], max(0, deadline - time.monotonic()))[0]
                    chunk = os.read(read_fd, 32)
                    assert chunk
                    number.extend(chunk)
                env['DISPLAY'] = ':' + number.decode().strip()
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
            run(args.bin_dir / 'jwm', '--backend', args.backend, '--gen-config')
            config = work / 'config/jwm/config_x11.toml'
            text = config.read_text()
            text = re.sub(r'(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)', '[status_bar]\nname="tao_glow_bar"\nshow_bar=true\nposition="top"\nlayout="single"\n\n', text)
            text = re.sub(r'(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)', '', text)
            text += '\n# sidebar mode acceptance comment\n[desktop_shell]\nenabled=true\n'
            if args.compat_helper:
                text += 'left="hover"\nright="hover"\nbottom="hover"\n'
                text = ''.join(block for block in re.split(r'(?m)(?=^\[)', text) if not (block.startswith('[[keybindings.keys]]') and 'toggle_desktop_shell_mode' in block))
                text += '\n[[keybindings.keys]]\nmodifier=["Mod4","Shift"]\nkey="b"\nfunction="spawn"\nargument=' + json.dumps([str(args.compat_helper.resolve()), '--tool', str((args.bin_dir / 'jwm-tool').resolve())]) + '\n'
            for key, value in {'wallpaper': json.dumps(str(ROOT / 'assets/wallpapers/08-cloud-sea.png')), 'wallpaper_effect': '"off"', 'wallpaper_colors': 'false'}.items():
                text = re.sub(r'(?m)^' + key + r' = .*$', key + ' = ' + value, text)
            config.write_text(text)
            (evidence / 'config_x11.toml').write_text(text)
            run(args.bin_dir / 'jwm', '--backend', args.backend, '--check-config')
            spawn('dbus-run-session', '--', args.bin_dir / 'jwm', '--backend', args.backend)
            initial = wait_for(lambda: (snapshot if (snapshot := state())['host']['ready'] and snapshot['host']['top_bars'] and len(snapshot['host']['panels']) == 3 else None), 'private shell and top ready', 60)
            host = initial['host']['pid']
            top = initial['host']['top_bars'][0]['pid']
            result['host_pid'], result['top_pid'] = host, top
            wm_pid = int(next(line.split(':', 1)[1] for line in Path(f'/proc/{host}/status').read_text().splitlines() if line.startswith('PPid:')))
            result['binary_sha256'] = {name: hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest() for name, pid in [('jwm', wm_pid), ('host', host), ('bar', top)]}
            result['binary_sha256']['jwm_tool'] = hashlib.sha256((args.bin_dir / 'jwm-tool').read_bytes()).hexdigest()
            assert modes('hover', 'hover', 'hover'), initial
            run('xdotool', 'mousemove', '640', '400')
            wait_for(lambda: all(not visible(panel['window']) for panel in check_identity()['host']['panels']), 'default hover panels hidden')
            result['x11_modifiers'] = run('xmodmap', '-pm')
            hover_area = msg('get_monitors')[0]
            record(('compatibility explicit hover' if args.compat_helper else 'omitted edge modes default to hover') + '; pointer away hides all panels')
            # A real focused client supplies the keyboard focus. Xvfb starts
            # with no client focus, which does not deliver passive key grabs.
            spawn('xmessage', '-name', 'SparseWorkspace1', '-title', 'SparseWorkspace1', 'SparseWorkspace1')
            first_window = wait_for(lambda: next((window for window in msg('get_windows') if window['name'] == 'SparseWorkspace1'), None), 'focused real keyboard client')
            msg('focus_window', {'id': first_window['id']})
            native_focus = wait_for(lambda: run('xdotool', 'search', '--onlyvisible', '--name', '^SparseWorkspace1$').splitlines()[0], 'native XID of keyboard focus client')
            run('xdotool', 'windowfocus', '--sync', native_focus)
            wait_for(lambda: check_identity()['outputs'][0]['snapshot']['client_name'] == 'SparseWorkspace1', 'focused client snapshot before keyboard injection')
            result['keyboard_focus'] = run('xdotool', 'getwindowfocus').strip()
            run('xdotool', 'key', '--clearmodifiers', 'Shift_L')
            msg('get_config_status')
            shortcut()
            pinned = wait_for(lambda: modes('always', 'always', 'always'), 'Super Shift B pins all edges')
            wait_for(lambda: all(visible(panel['window']) for panel in check_identity()['host']['panels']), 'pinned panels mapped')
            pinned_area = msg('get_monitors')[0]
            assert pinned_area['ww'] < hover_area['ww'] and pinned_area['wh'] < hover_area['wh'], (pinned_area, hover_area)
            assert '# sidebar mode acceptance comment' in config.read_text()
            wait_for(lambda: saved_modes()['left'] == 'always', 'successful helper/file mode commit')
            record('physical Super Shift B pins three edges, reserves workarea and persists without replacing host/top')
            shortcut()
            wait_for(lambda: modes('hover', 'hover', 'hover'), 'second shortcut restores hover')
            wait_for(lambda: all(not visible(panel['window']) for panel in check_identity()['host']['panels']), 'hover hides after shortcut')
            if args.compat_helper:
                wait_for(lambda: saved_modes()['left'] == 'hover', 'second helper committed hover before mixed user edit')
                msg('reload_config')
            configuration('desktop_shell.right', 'disabled')
            configuration('desktop_shell.bottom', 'always')
            wait_for(lambda: modes('hover', 'disabled', 'always'), 'mixed source modes')
            shortcut()
            wait_for(lambda: modes('always', 'disabled', 'always'), 'mixed set converges pinned')
            ipc_toggle()
            wait_for(lambda: modes('hover', 'disabled', 'hover'), 'IPC returns active edges to hover')
            saved = saved_modes()
            assert [saved[key] for key in ['enabled', 'left', 'right', 'bottom']] == [True, 'hover', 'disabled', 'hover'], saved
            msg('reload_config')
            wait_for(lambda: modes('hover', 'disabled', 'hover'), 'persisted mode survives reload')
            record('mixed modes, disabled edge, IPC and reload preserve enabled ownership, top visibility and both process identities')
            # Real clients establish sparse occupancy, including a minimized client.
            windows = [first_window['id']]
            for number in [3, 8]:
                msg('view', {'tag': 1 << (number - 1)})
                title = f'SparseWorkspace{number}'
                spawn('xmessage', '-name', title, '-title', title, title)
                window = wait_for(lambda: next((window for window in msg('get_windows') if window['name'] == title), None), title)
                windows.append(window['id'])
                if number == 3:
                    msg('focus_window', {'id': window['id']})
                    msg('minimize')
                    wait_for(lambda: next((item for item in msg('get_windows') if item['id'] == window['id'] and item['is_minimized']), None), 'real minimized workspace-3 client')
            msg('view', {'tag': 1 << 4})
            def sparse_selected(expected):
                snapshot = check_identity()['outputs'][0]['snapshot']
                tags = snapshot['tags']
                assert len(tags) == 9, tags
                occupied = [index + 1 for index, tag in enumerate(tags) if tag['occupied']]
                selected = [index + 1 for index, tag in enumerate(tags) if tag['selected']]
                return snapshot if occupied == [1, 3, 8] and selected == expected else None
            result['sparse_snapshot'] = wait_for(lambda: sparse_selected([5]), 'occupied 1/3/8 union empty active 5')
            msg('toggleview', {'tag': 1 << 7})
            result['multi_selected_snapshot'] = wait_for(lambda: sparse_selected([5, 8]), 'multiple actual selected tags')
            assert all(any(window['id'] == wanted for window in msg('get_windows')) for wanted in windows)
            record('real clients publish sparse original tags 1/3/8, empty active 5 and selected union 5/8 in complete nine-tag model')
            result['final_state'] = check_identity()
            result['status'] = 'passed'
        except Exception as error:
            result.update(status='failed', reason=str(error))
            if 'config' in locals() and config.is_file():
                (evidence / 'failed_config_x11.toml').write_text(config.read_text())
            raise
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                        child.wait(timeout=5)
                    except (ProcessLookupError, subprocess.TimeoutExpired):
                        try:
                            os.killpg(child.pid, signal.SIGKILL)
                            child.wait(timeout=10)
                        except ProcessLookupError:
                            pass
            log.close()
            result['duration_seconds'] = round(time.monotonic() - started, 3)
            (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
