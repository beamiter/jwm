#!/usr/bin/env python3
"""Private X11 acceptance for real sparse top-bar pixels and original tag click bindings."""
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
    parser.add_argument('--bin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--backend', choices=['xcb', 'x11rb'], default='x11rb')
    parser.add_argument('--bar', default='tauri_leptos_bar')
    parser.add_argument('--theme', choices=['light', 'dark'], default='light')
    parser.add_argument('--first-tag-center', type=float, default=68, help='physical x of first numeric tag, relative to the actual top window')
    parser.add_argument('--tag-step', type=float, default=40)
    parser.add_argument('--expect-wm-sha256')
    parser.add_argument('--bar-bin', type=Path)
    parser.add_argument('--shell-bin', type=Path, default=ROOT / 'crates/jwm_shell/target/release/jwm-shell')
    parser.add_argument('--evidence-dir', type=Path, default=ROOT / 'artifacts/header-alignment/sparse-runtime')
    args = parser.parse_args()
    if args.bar_bin is None:
        args.bar_bin = (ROOT / 'target/tauri-release/release/tauri_leptos_bar') if args.bar == 'tauri_leptos_bar' else ROOT / 'bars' / args.bar / 'target/debug' / args.bar
    for binary in [args.bin_dir / 'jwm', args.bin_dir / 'jwm-tool', args.bar_bin, args.shell_bin]:
        assert binary.is_file() and os.access(binary, os.X_OK), binary
    evidence = (args.evidence_dir / f'{args.bar}-{args.backend}-{args.theme}').resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    result = {'status': 'running', 'backend': args.backend, 'bar': args.bar, 'theme': args.theme, 'steps': [], 'fixture_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    children = []
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='jwm-sparse-header-') as directory:
        work = Path(directory)
        env = dict(os.environ, LIBGL_ALWAYS_SOFTWARE='1', GDK_BACKEND='x11', JWM_SHELL_BIN=str(args.shell_bin.resolve()), JWM_COMPOSITOR_API='glx')
        for key in ['DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'JWM_SOCKET', 'JWM_BACKEND', 'JWM_DESKTOP_SHELL', 'JWM_BAR_LAYOUT', 'JWM_DAEMON_PID']:
            env.pop(key, None)
        for key, folder in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'), ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_PICTURES_DIR', 'pictures')]:
            (work / folder).mkdir(mode=0o700)
            env[key] = str(work / folder)
        (work / 'bin').mkdir()
        (work / 'bin' / args.bar).symlink_to(args.bar_bin.resolve())
        env['PATH'] = str(work / 'bin') + os.pathsep + env.get('PATH', '')
        env['XBAR_CONFIG'] = str(work / 'xbar.toml')
        (work / 'xbar.toml').write_text(f'theme="{args.theme}"\n')
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
                    last = str(error)
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
            text = re.sub(r'(?ms)^\[status_bar\]\n.*?(?=^\[|\Z)', f'[status_bar]\nname="{args.bar}"\nshow_bar=true\nposition="top"\nlayout="single"\n\n', text)
            text = re.sub(r'(?ms)^\[desktop_shell\]\n.*?(?=^\[|\Z)', '', text)
            text += '\n# sparse top-header acceptance\n[desktop_shell]\nenabled=true\n'
            config.write_text(text)
            run(args.bin_dir / 'jwm', '--backend', args.backend, '--check-config')
            spawn('dbus-run-session', '--', args.bin_dir / 'jwm', '--backend', args.backend)
            initial = wait_for(lambda: (snapshot if (snapshot := state())['host']['ready'] and snapshot['host']['top_bars'] and len(snapshot['host']['panels']) == 3 else None), 'private shell and top ready', 60)
            host = initial['host']['pid']
            top = initial['host']['top_bars'][0]['pid']
            result['host_pid'], result['top_pid'] = host, top
            def geometry(window):
                fields = dict(re.findall(r'^(\w+)=(\S+)$', run('xdotool', 'getwindowgeometry', '--shell', window), re.M))
                return tuple(int(fields[key]) for key in ['X', 'Y', 'WIDTH', 'HEIGHT'])
            def top_window():
                current = state()['host']['top_bars']
                if not current or not (candidate := current[0].get('pid')): return None
                for xid in run('xdotool', 'search', '--pid', candidate).split():
                    rect = geometry(xid)
                    if visible(xid) and rect[2] > 600 and 15 <= rect[3] <= 100:
                        return candidate, xid, rect
                return None
            top, xid, rect = wait_for(top_window, 'actual mapped top window', 60)
            result['top_pid'] = top
            wm_pid = int(next(line.split(':', 1)[1] for line in Path(f'/proc/{host}/status').read_text().splitlines() if line.startswith('PPid:')))
            result['binary_sha256'] = {name: hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest() for name, pid in [('jwm', wm_pid), ('host', host), ('bar', top)]}
            result['binary_sha256']['jwm_tool'] = hashlib.sha256((args.bin_dir / 'jwm-tool').read_bytes()).hexdigest()
            if args.expect_wm_sha256:
                assert result['binary_sha256']['jwm'] == args.expect_wm_sha256, result['binary_sha256']
            assert result['binary_sha256']['bar'] == hashlib.sha256(args.bar_bin.read_bytes()).hexdigest()
            assert result['binary_sha256']['host'] == hashlib.sha256(args.shell_bin.read_bytes()).hexdigest()
            result['top_window'], result['top_rect'] = xid, rect
            if args.bar.startswith('tauri_'):
                def rendered():
                    raw = run('xprop', '-id', xid, '_JWM_FRONTEND_READY', '_JWM_FRONTEND_RENDERED', '_JWM_FRONTEND_ERROR')
                    result['frontend_diagnostics'] = raw
                    return all(re.search(name + r'\([^)]*\)\s*=\s*1\s*$', raw, re.M) for name in ['_JWM_FRONTEND_READY', '_JWM_FRONTEND_RENDERED'])
                wait_for(rendered, 'actual Tauri frontend rendered acknowledgement', 60)
            run('xdotool', 'mousemove', '640', '400')
            record('fresh actual top, host and WM binary hashes recorded; real mapped frontend ready')
            def capture(name):
                source = Path(msg('take_screenshot_fullscreen')['path'])
                assert source.is_relative_to(work / 'pictures'), source
                wait_for(lambda: source.is_file() and source.stat().st_size > 32, 'compositor screenshot')
                destination = evidence / f'{name}.png'
                destination.write_bytes(source.read_bytes())
                from PIL import Image
                image = Image.open(destination).convert('RGB')
                assert image.size == (1280, 800), image.size
                image.crop((rect[0], rect[1], rect[0] + min(260, rect[2]), rect[1] + rect[3])).resize((1040, rect[3] * 4)).save(evidence / f'{name}-tags.png')
                result.setdefault('screenshots', []).append(str(destination.relative_to(ROOT)))
                return image
            def selected_pixels(image, selected, expected_count):
                # Sample each pill's top-left interior, away from its label.
                # This is the same opaque shared blue selection used by the
                # native scene and serialized TagPalette in real webviews.
                from statistics import median
                readings = []
                for slot in range(4):
                    center = rect[0] + args.first_tag_center + slot * args.tag_step
                    y = rect[1] + rect[3] / 2
                    pixels = [image.getpixel((round(center - 10 + dx), round(y - 8 + dy))) for dx in range(3) for dy in range(3)]
                    rgb = tuple(round(median(pixel[channel] for pixel in pixels)) for channel in range(3))
                    blue = rgb[0] < 90 and 45 < rgb[1] < 165 and rgb[2] > 175
                    gap_pixels = [image.getpixel((round(center + args.tag_step / 2 + dx), round(y - 8 + dy))) for dx in range(-1, 2) for dy in range(3)]
                    gap_rgb = tuple(round(median(pixel[channel] for pixel in gap_pixels)) for channel in range(3))
                    material_difference = max(abs(pill - gap) for pill, gap in zip(rgb, gap_rgb))
                    glyph_pixels = [image.getpixel((round(center + dx), round(y + dy))) for dx in range(-7, 8) for dy in range(-7, 8)]
                    glyph_ink = sum(max(abs(pixel[channel] - rgb[channel]) for channel in range(3)) >= 40 for pixel in glyph_pixels)
                    # A minimized occupied tag has a quieter material than a
                    # filled tag. Require both tint and actual digit ink;
                    # the empty fourth slot of a stale three-pill frame has
                    # neither, even when another slot happens to be blue.
                    present = material_difference >= 4 and glyph_ink >= 2
                    readings.append({'slot': slot, 'rgb': rgb, 'gap_rgb': gap_rgb, 'material_difference': material_difference, 'glyph_ink': glyph_ink, 'present': present, 'selected_blue': blue})
                # A stale three-pill 1/3/8 frame has blue slot 2 too. Require
                # the fourth occupied pill before accepting that slot as empty
                # active 5; after selecting 3, require the compact three pills.
                assert [row['slot'] for row in readings if row['present']] == list(range(expected_count)), readings
                assert [row['slot'] for row in readings if row['selected_blue']] == selected, readings
                return readings
            # Real clients establish sparse occupancy, including a minimized client.
            windows = []
            result['clients'] = []
            for number in [1, 3, 8]:
                msg('view', {'tag': 1 << (number - 1)})
                title = f'SparseWorkspace{number}'
                spawn('xmessage', '-name', title, '-title', title, title)
                window = wait_for(lambda: next((window for window in msg('get_windows') if window['name'] == title), None), title)
                windows.append(window['id'])
                result['clients'].append({'tag': number, 'window': window['id']})
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
            def painted(name, selected, expected_count=4):
                def sample():
                    image = capture(name)
                    return selected_pixels(image, selected, expected_count)
                return wait_for(sample, 'actual selected tag pixels ' + name, 30)
            result['sparse_pixels'] = painted('sparse-1-3-5-8-active5', [2])
            record('actual compositor pixels show four sparse pills and empty active tag 5 selection')
            def pointer_click(slot, button):
                if button in (4, 5):
                    # Separate the two intended gestures by the shared 250ms
                    # idle boundary, including WebKit pixel-wheel delivery.
                    time.sleep(.30)
                point = [round(rect[0] + args.first_tag_center + slot * args.tag_step), round(rect[1] + rect[3] / 2)]
                run('xdotool', 'mousemove', *point)
                time.sleep(.15)
                run('xdotool', 'click', button)
                time.sleep(.15)
                run('xdotool', 'mousemove', '640', '400')
                return point
            result['right_click'] = {'displayed_slot': 1, 'original_tag': 3, 'point': pointer_click(1, 3)}
            result['right_clicked_snapshot'] = wait_for(lambda: sparse_selected([3, 5]), 'actual right click adds original tag 3 to selected empty tag 5')
            result['right_clicked_pixels'] = painted('right-click-tag3-active3-5', [1, 2])
            record('actual right click on displayed tag 3 selects original tags 3 and 5 together')
            pointer_click(1, 3)
            wait_for(lambda: sparse_selected([5]), 'second actual right click removes original tag 3')
            pointer_click(3, 3)
            result['multi_selected_snapshot'] = wait_for(lambda: sparse_selected([5, 8]), 'multiple actual selected tags')
            result['multi_selected_pixels'] = painted('sparse-1-3-5-8-active5-8', [2, 3])
            record('actual compositor pixels highlight both selected tags 5 and 8')
            # The second displayed pill is original tag 3, not tag 2.
            click_x = round(rect[0] + args.first_tag_center + args.tag_step)
            click_y = round(rect[1] + rect[3] / 2)
            run('xdotool', 'mousemove', click_x, click_y)
            run('xdotool', 'click', '1')
            run('xdotool', 'mousemove', '640', '400')
            result['clicked_tag_snapshot'] = wait_for(lambda: sparse_selected([3]), 'second actual top pill changes WM to original tag 3')
            result['click'] = {'displayed_slot': 1, 'original_tag': 3, 'point': [click_x, click_y]}
            result['clicked_pixels'] = painted('clicked-original-tag3', [1], 3)
            record('actual pointer click on second displayed top pill selects original WM tag 3')
            pointer_click(0, 5)
            result['wheel_next_snapshot'] = wait_for(lambda: sparse_selected([8]), 'actual wheel down cycles original eligible tag 3 to 8')
            result['wheel_next_pixels'] = painted('wheel-next-original-tag8', [2], 3)
            pointer_click(0, 4)
            result['wheel_previous_snapshot'] = wait_for(lambda: sparse_selected([3]), 'actual wheel up cycles original eligible tag 8 to 3')
            result['wheel_previous_pixels'] = painted('wheel-previous-original-tag3', [1], 3)
            record('actual wheel uses the eligible original 1/3/8 sequence and cycles 3 to 8 to 3')
            assert all(any(window['id'] == wanted for window in msg('get_windows')) for wanted in windows)
            record('all three real clients including minimized tag-3 client survive actual header interactions')
            result['final_state'] = check_identity()
            result['status'] = 'passed'
        except Exception as error:
            result.update(status='failed', reason=str(error))
            try: result['failure_state'] = state()
            except Exception as state_error: result['failure_state_error'] = str(state_error)
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
