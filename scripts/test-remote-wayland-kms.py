#!/usr/bin/env python3
"""Verify a read-only remote viewer on disposable JWM KMS outputs."""

import argparse
from contextlib import ExitStack
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time
from typing import NamedTuple


spec = importlib.util.spec_from_file_location(
    'owned_x11', Path(__file__).with_name('test-remote-viewer-e2e.py'))
x11 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(x11)

FLIP = re.compile(
    r'output=(\S+) submission=(\d+) flip_ns=(\d+) sequence=(\d+) callbacks=(\d+) completed=true')
COMPOSITED = re.compile(
    r'output=(\S+) submission=(\d+) compositor_feedback_callbacks=(\d+) '
    r'visible_rect=\(([-\d, ]+)\) overlapping_cursor_elements=(\d+) '
    r'output_visible_rect=\(([-\d, ]+)\)')
DIRECT = re.compile(r'output=(\S+) submission=(\d+) direct_feedback_callbacks=(\d+)')
FEEDBACK = re.compile(r'<- wp_presentation_feedback@(\d+)\.presented, \(([0-9, ]+)\)')
DISCARDED = re.compile(r'<- wp_presentation_feedback@(\d+)\.discarded, \(\)')
FEEDBACK_REQUEST = re.compile(r'-> wp_presentation@\d+\.feedback\(wl_surface@(\d+), wp_presentation_feedback@(\d+)\)')
SURFACE_SCALE = re.compile(r'-> wl_surface@(\d+)\.set_buffer_scale\((\d+)\)')
SURFACE_COMMIT = re.compile(r'-> wl_surface@(\d+)\.commit\(\)')


def text(path):
    return path.read_text(encoding='utf-8', errors='replace')


def records(path):
    return [json.loads(line) for line in text(path).splitlines() if line.startswith('{') and line.endswith('}')]


def host_stats(path):
    return [row for row in records(path) if row.get('type') == 'jwm_remote_host_stats']


def wait_for(label, children, predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for child in children:
            if child.poll() is not None:
                raise RuntimeError(f'{label}: child exited with {child.returncode}')
        result = predicate()
        if result:
            return result
        time.sleep(.05)
    raise RuntimeError(f'timed out waiting for {label}')


def stop(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)


def read_output(env, output, check_viewport):
    ppm = subprocess.check_output(['grim', '-t', 'ppm', '-o', output, '-'], env=env, timeout=5)
    magic, dimensions, depth, pixels = ppm.split(b'\n', 3)
    width, height = map(int, dimensions.split())
    if magic != b'P6' or depth != b'255' or len(pixels) != 3 * width * height:
        raise RuntimeError('invalid compositor PPM')
    if check_viewport:
        indices = [(x * width // 8, y * height // 3) for y in (1, 2) for x in (3, 5)]
    else:
        indices = [(x * width // 8, height // 2) for x in (3, 4, 5)]
    samples = [list(pixels[3 * (y * width + x):3 * (y * width + x + 1)]) for x, y in indices]
    return {'size': [width, height], 'rgb': samples}


def read_primary(binary, card, output, env):
    result = subprocess.run([binary, card, output], env=env, capture_output=True, text=True, timeout=5)
    if result.returncode == 75:
        return None
    if result.returncode != 0:
        raise RuntimeError(f'virtio primary readback on {output} failed: {result.stderr.strip()}')
    framebuffer = json.loads(result.stdout)
    if framebuffer['connector'] != output or not framebuffer.get('host_transfer'):
        raise RuntimeError('virtio primary readback did not identify the requested output and host transfer')
    return framebuffer


class WireFeedback(NamedTuple):
    requested: int
    committed: set
    presented: list
    discarded: int


def wire_feedback(viewer_log):
    scale = {}
    pending = {}
    committed = {}
    requested, discarded = 0, 0
    presented = []
    for line in viewer_log.splitlines():
        if match := FEEDBACK_REQUEST.search(line):
            surface, feedback = match.groups()
            requested += 1
            pending.setdefault(surface, []).append((feedback, requested))
        elif match := SURFACE_SCALE.search(line):
            surface, factor = match.groups()
            scale[surface] = int(factor)
        elif match := SURFACE_COMMIT.search(line):
            surface = match[1]
            for feedback, request in pending.pop(surface, ()):
                committed[feedback] = request, scale.get(surface, 1)
        elif match := FEEDBACK.search(line):
            sec_hi, sec_lo, ns, _, seq_hi, seq_lo, flags = map(int, match[2].split(','))
            timing = (((sec_hi << 32) | sec_lo) * 1_000_000_000 + ns, (seq_hi << 32) | seq_lo)
            request, buffer_scale = committed.pop(match[1], (None, None))
            presented.append((timing, flags, buffer_scale, request))
        elif match := DISCARDED.search(line):
            discarded += 1
            committed.pop(match[1], None)
    return WireFeedback(requested, {request for request, _ in committed.values()}, presented, discarded)


def verify_dark_feedback(wire, off_observed_ns, last_off_ns):
    if wire.discarded:
        raise RuntimeError('Wayland compositor discarded viewer feedback during the DPMS test')
    if any(off_observed_ns <= timing[0] <= last_off_ns for timing, _, _, _ in wire.presented):
        raise RuntimeError('viewer received presentation feedback timestamped while DRM DPMS was Off')


def cycle_idle_dpms(wlopm, status_path, output, env, host, viewer, viewer_log, host_log, publish_blue):
    children = (host, viewer)

    def status():
        return text(status_path).strip()

    def set_power(mode):
        subprocess.run([wlopm, '--' + mode.lower(), output], env=env, check=True, timeout=5)
        wait_for(f'{output} DRM DPMS {mode}', (), lambda: status() == mode, timeout=5)
        snapshot = subprocess.check_output([wlopm, '--json'], env=env, timeout=5)
        protocol_modes = [row.get('power-mode') for row in json.loads(snapshot) if row.get('output') == output]
        if protocol_modes != [mode.lower()]:
            raise RuntimeError(f'{output} fresh Wayland power mode {protocol_modes} differs from DRM DPMS {mode}')

    def acknowledged_idle():
        wire = wire_feedback(text(viewer_log))
        stats = host_stats(host_log)
        drawn = sum(row['drawn_acks'] for row in stats)
        if wire.requested == len(wire.presented) and not wire.discarded and drawn and stats[-1]['outstanding'] == 0:
            return drawn, len(wire.presented)
        return None

    def next_keepalive():
        wire = wire_feedback(text(viewer_log))
        if wire.discarded:
            raise RuntimeError('viewer feedback was discarded before DPMS Off')
        if wire.requested != len(wire.presented) or len(wire.presented) <= presented_before_idle:
            return False
        previous, latest = (entry[0][0] for entry in wire.presented[-2:])
        return latest - previous >= 3_000_000_000

    quiet_since, latest_request = time.monotonic(), None

    def drained():
        nonlocal quiet_since, latest_request
        wire = wire_feedback(text(viewer_log))
        if wire.discarded:
            raise RuntimeError('viewer feedback was discarded while settling before DPMS Off')
        now = time.monotonic()
        if wire.requested != latest_request or wire.requested != len(wire.presented):
            quiet_since, latest_request = now, wire.requested
        return now - quiet_since >= .5

    if status() != 'On':
        raise RuntimeError(f'{output} must start with DRM DPMS On')
    drawn, presented_before_idle = wait_for('a presented and acknowledged idle viewer', children, acknowledged_idle)
    # Freeze just after an unchanged-source keepalive, before the host can
    # prepare its next red keepalive and carry it across the power cut.
    wait_for('a fresh unchanged-source keepalive', children, next_keepalive)
    try:
        host.send_signal(signal.SIGSTOP)
        wait_for('owned host to stop', children,
                 lambda: os.waitid(os.P_PID, host.pid, os.WSTOPPED | os.WNOHANG), timeout=3)
        wait_for('wire feedback to drain with the host stopped', children, drained, timeout=4)
        try:
            set_power('Off')
            off_observed_ns = time.monotonic_ns()
            publish_blue()
            host.send_signal(signal.SIGCONT)

            def still_dark():
                if status() != 'Off':
                    raise RuntimeError(f'{output} left DRM DPMS Off before re-enable')
                now = time.monotonic_ns()
                wire = wire_feedback(text(viewer_log))
                verify_dark_feedback(wire, off_observed_ns, now)
                fresh_commits = sorted(request for request in wire.committed if request > latest_request)
                if fresh_commits and now - off_observed_ns >= 500_000_000:
                    return now, fresh_commits
                return None

            last_off_ns, pending = wait_for('fresh viewer commit and half a second in DRM DPMS Off',
                                            children, still_dark, timeout=4)
        finally:
            on_requested_ns = time.monotonic_ns()
            set_power('On')
    finally:
        host.send_signal(signal.SIGCONT)
    return {'states': ['On', 'Off', 'On'], 'off_observed_ns': off_observed_ns,
            'last_off_ns': last_off_ns, 'on_requested_ns': on_requested_ns,
            'on_observed_ns': time.monotonic_ns(), 'pending_feedback_before_on': len(pending),
            'pending_feedback_requests': pending, 'wayland_power_modes': ['off', 'on'],
            'drawn_acks_before_off': drawn, 'presented_while_off': 0}


def observe_device_loss(card, connector, host, viewer, viewer_log, host_log, events, publish_blue):
    children = (host, viewer)

    def settled():
        wire = wire_feedback(text(viewer_log))
        stats = host_stats(host_log)
        if wire.discarded:
            raise RuntimeError('viewer feedback was discarded before DRM device loss')
        drawn = sum(row['drawn_acks'] for row in stats)
        if drawn and stats[-1]['outstanding'] == 0 and wire.requested == len(wire.presented):
            return wire, drawn, sum(row['sent'] for row in stats)
        return None

    wait_for('an acknowledged frame before DRM device loss', children, settled)
    try:
        host.send_signal(signal.SIGSTOP)
        wait_for('owned host child to stop', children,
                 lambda: os.waitid(os.P_PID, host.pid, os.WSTOPPED | os.WNOHANG), timeout=3)
        before, drawn_before, sent_before = wait_for('pre-removal feedback to drain', children, settled, timeout=4)
        if not card.is_dir() or text(connector / 'status').strip() != 'connected':
            raise RuntimeError('the selected DRM card and connector must be present before arming')
        armed = time.monotonic_ns()
        print(f'JWM_KMS_DEVICE_LOSS_READY {card.name}', flush=True)
        wait_for('external removal of the DRM card and selected connector', children,
                 lambda: not card.exists() and not connector.exists(), timeout=15)
        removed = time.monotonic_ns()
        publish_blue()
    finally:
        host.send_signal(signal.SIGCONT)

    viewer_exit = viewer.wait(timeout=18)
    host_exit = host.wait(timeout=5)
    wire = wire_feedback(text(viewer_log))
    stats = host_stats(host_log)
    drawn = sum(row['drawn_acks'] for row in stats)
    sent = sum(row['sent'] for row in stats)
    terminal = records(events)[-1]
    failure = terminal.get('failure') or {}
    if wire.presented != before.presented or drawn != drawn_before:
        raise RuntimeError('DRM device loss produced presentation feedback or a drawn acknowledgement')
    if wire.requested <= before.requested or sent <= sent_before:
        raise RuntimeError('the viewer did not request presentation for a post-removal frame')
    if (viewer_exit != 1 or terminal.get('state') != 'failed'
            or failure.get('category') not in {'wayland_presentation_timeout', 'wayland_presentation_discarded'}
            or failure.get('reconnect_certified') is not False):
        raise RuntimeError(f'DRM device loss did not produce a terminal presentation failure: {failure}')
    return {'card': card.name, 'connector': connector.name, 'armed_ns': armed, 'device_gone_observed_ns': removed,
            'viewer_exit': viewer_exit, 'host_exit': host_exit, 'failure': failure,
            'drawn_acks_before': drawn_before, 'drawn_acks_after': drawn, 'presented_after_arming': 0,
            'post_removal_frames_sent': sent - sent_before, 'post_removal_feedback_requests':
            wire.requested - before.requested, 'discarded_feedback': wire.discarded}


def flip_matches(kms_log, wire):
    flips = {}
    for name, serial, ns, sequence, count in FLIP.findall(kms_log):
        if int(count) > 0:
            flips.setdefault((int(ns), int(sequence)), []).append((name, int(serial)))
    composited = {
        (name, int(serial)): {'visible_rect': list(map(int, rect.split(','))),
                              'output_visible_rect': list(map(int, local_rect.split(','))),
                              'overlapping_cursor_elements': int(overlap)}
        for name, serial, count, rect, overlap, local_rect in COMPOSITED.findall(kms_log)
        if int(count) > 0
    }
    direct = {(name, int(serial)) for name, serial, count in DIRECT.findall(kms_log) if int(count) > 0}
    for timing, flags, buffer_scale, request in wire.presented:
        candidates = flips.get(timing, [])
        if len(candidates) == 1:
            name, serial = candidates[0]
            match = {'output_name': name, 'submission': serial, 'monotonic_ns': timing[0],
                     'sequence': timing[1], 'flags': flags}
            if buffer_scale is not None:
                match['buffer_scale'] = buffer_scale
            if request is not None:
                match['feedback_request'] = request
            if (name, serial) in composited:
                match['composited'] = composited[name, serial]
            if (name, serial) in direct:
                match['direct'] = True
            yield match


def wm_request(path, **request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(path))
        connection.sendall((json.dumps(request) + '\n').encode())
        with connection.makefile('rb') as response:
            result = json.loads(response.readline(1024 * 1024))
    if not result['success']:
        raise RuntimeError(f'JWM IPC failed: {result.get("error")}')
    return result.get('data')


def named_monitors(ipc):
    return {entry['name']: entry for entry in wm_request(ipc, query='get_monitors') if entry.get('name')}


def wait_hotplug(action, status_path, drm_status, outputs, ipc, children):
    print(f'JWM_HOTPLUG_PHASE={action}', flush=True)

    def detected():
        if text(status_path).strip() != drm_status:
            return None
        monitors = named_monitors(ipc)
        return monitors if set(monitors) == outputs else None

    return wait_for(f'kernel and compositor connector {action}', children, detected, timeout=8)


def viewer_window(ipc, monitor, window_id=None):
    windows = [window for window in wm_request(ipc, query='get_windows')
               if window['class'] == 'jwm-remote-readonly']
    if len(windows) != 1:
        return None
    window = windows[0]
    if window_id is not None and window['id'] != window_id:
        raise RuntimeError('viewer window was replaced during the move')
    if (window['monitor'] == monitor['num'] and window['is_on_view'] and window['is_focused']
            and monitor['x'] <= window['x'] < window['x'] + window['w'] <= monitor['x'] + monitor['w']
            and monitor['y'] <= window['y'] < window['y'] + window['h'] <= monitor['y'] + monitor['h']):
        return window['id']
    return None


def matches_color(rgb, expected):
    return all(abs(actual - target) <= 8 for actual, target in zip(rgb, expected))


def matches_samples(samples, expected):
    return all(matches_color(rgb, target) for rgb, target in zip(samples, expected, strict=True))


def write_viewport_bitmap(path):
    # Match the owned Xvfb source and keep the two transitions away from the
    # sample positions, even after the viewer letterboxes its source.
    width, height = 1280, 800
    upper = '0x00,' * 88 + '0xff,' * 72
    lower = '0xff,' * 88 + '0x00,' * 72
    rows = (upper + '\n') * 480 + (lower + '\n') * 320
    path.write_text(f'#define source_width {width}\n#define source_height {height}\n'
                    f'static unsigned char source_bits[] = {{\n{rows}}};\n')


def exercise(binary, wayland_socket, output, kms_log, require_path, move_to_output=None, ipc=None,
             check_viewport=False, expected_scales=None, primary_reader=None, drm_card=None,
             wlopm=None, dpms_status=None, hotplug_status=None, device_loss_card=None):
    env = os.environ.copy()
    for name in ('DISPLAY', 'WAYLAND_SOCKET', 'WAYLAND_DISPLAY'):
        env.pop(name, None)
    env['WAYLAND_DISPLAY'] = str(wayland_socket)
    prior_log = kms_log.stat().st_size
    with tempfile.TemporaryDirectory(prefix='jwm-remote-kms-') as directory, ExitStack() as cleanup:
        private = Path(directory)
        source, source_display = x11.launch()
        cleanup.callback(stop, source)

        bitmap = private / 'viewport.xbm'
        if check_viewport:
            write_viewport_bitmap(bitmap)

        def paint(color, accent):
            options = (['-bitmap', str(bitmap), '-bg', color, '-fg', accent] if check_viewport
                       else ['-solid', color])
            subprocess.run(['xsetroot', '-display', source_display, *options], check=True, env=env, timeout=5)

        def start(args, name, stdin=None, process_env=env):
            stdout, stderr = private / (name + '.out'), private / (name + '.err')
            child = subprocess.Popen([binary, *args], env=process_env, stdin=stdin,
                                     stdout=cleanup.enter_context(stdout.open('wb')),
                                     stderr=cleanup.enter_context(stderr.open('wb')))
            cleanup.callback(stop, child)
            return child, stdout, stderr

        key = str(private / 'key')
        subprocess.run([binary, 'keygen', '--output', key], check=True, env=env, timeout=5)
        red, blue = [204, 34, 68], [34, 68, 204]
        green, yellow = [48, 176, 80], [223, 169, 34]
        paint('#cc2244', '#30b050')
        host, _, host_log = start(['host', '--display', source_display, '--capture-source', 'root',
                                   '--once', '--stats-json', '--fps', '15', '--listen', '127.0.0.1:0',
                                   '--key-file', key], 'host')
        viewer_log, events = private / 'viewer.err', private / 'viewer.out'
        try:
            address = wait_for('host listener', [host], lambda: re.search(
                r'listening on (127\.0\.0\.1:\d+)', text(host_log)))[1]
            viewer, events, viewer_log = start(
                ['session', address, '--viewer-backend', 'wayland', '--viewer-wayland-display', str(wayland_socket),
                 '--window-size', '640x400', '--jpeg', '--key-file', key], 'viewer',
                stdin=subprocess.PIPE, process_env=env | {'WAYLAND_DEBUG': 'client'})
            children = [host, viewer]
            wait_for('viewer presentation', children,
                     lambda: any(row.get('state') == 'viewing' for row in records(events)))
            monitors, window_id = {}, None
            if move_to_output:
                monitors = named_monitors(ipc)
                if output not in monitors or move_to_output not in monitors:
                    raise RuntimeError('both requested outputs must be active')
                if hotplug_status and set(monitors) != {output, move_to_output}:
                    raise RuntimeError('the hotplug fixture requires exactly two owned outputs')
                if expected_scales and [monitors[name]['scale'] for name in (output, move_to_output)] != expected_scales:
                    raise RuntimeError(f'outputs do not have the requested scales {expected_scales}')
                window_id = wait_for('viewer on initial output', children,
                                     lambda: viewer_window(ipc, monitors[output]))
            phases = []
            dpms = None
            hotplug = []
            cases = [
                (output, move_to_output, red, green),
                (move_to_output or output, output if move_to_output else None, blue, yellow),
            ]
            if hotplug_status:
                cases[1] = (move_to_output, None, blue, yellow)
                cases.append((output, move_to_output, red, green))
            if device_loss_card:
                cases = cases[:1]
            for location, other, color, accent in cases:
                expected = [color, accent, accent, color] if check_viewport else [color] * 3
                start_ns = 0
                if phases:
                    removed = hotplug_status and len(phases) == 1
                    if hotplug_status:
                        action, status = ('disconnect', 'disconnected') if removed else ('reconnect', 'connected')
                        outputs = {move_to_output} if removed else {output, move_to_output}
                        monitors = wait_hotplug(action, hotplug_status, status, outputs, ipc, children)
                        hotplug.append({'action': action, 'kernel': status, 'outputs': sorted(monitors),
                                        'observed_ns': time.monotonic_ns()})
                    if move_to_output:
                        if not removed:
                            wm_request(ipc, command='move_window_to_monitor',
                                       args={'window': window_id, 'monitor': monitors[location]['num']})
                        wm_request(ipc, command='focus_window', args={'id': window_id})
                        wait_for('same focused viewer on destination output', children,
                                 lambda: viewer_window(ipc, monitors[location], window_id))
                    if wlopm:
                        dpms = cycle_idle_dpms(wlopm, dpms_status, location, env, host, viewer, viewer_log,
                                               host_log, lambda: paint('#2244cc', '#dfa922'))
                        start_ns = dpms['on_requested_ns']
                    else:
                        start_ns = time.monotonic_ns()
                        if color == blue:
                            paint('#2244cc', '#dfa922')
                        else:
                            paint('#cc2244', '#30b050')

                def evidence():
                    wire = wire_feedback(text(viewer_log))
                    with kms_log.open('rb') as log:
                        log.seek(prior_log)
                        ordered = list(flip_matches(log.read().decode(errors='replace'), wire))
                    current = [match for match in ordered if match['monotonic_ns'] >= start_ns]
                    if dpms:
                        verify_dark_feedback(wire, dpms['off_observed_ns'], dpms['last_off_ns'])
                        completed = {match.get('feedback_request') for match in current}
                        if not set(dpms['pending_feedback_requests']).issubset(completed):
                            return None
                    if any(match['output_name'] != location for match in current):
                        raise RuntimeError(f'viewer feedback matched an output other than {location}')
                    if require_path == 'direct':
                        current = [match for match in current if match.get('direct')]
                    elif require_path:
                        current = [match for match in current if 'composited' in match]
                    if require_path == 'cursor-overlap':
                        current = [match for match in current
                                   if match['composited']['overlapping_cursor_elements'] > 0]
                    if expected_scales:
                        scale = math.ceil(monitors[location]['scale'])
                        current = [match for match in current if match.get('buffer_scale') == scale]
                    if not current or (dpms and current[-1].get('feedback_request') not in dpms['pending_feedback_requests']):
                        return None
                    image = read_output(env, location, check_viewport)
                    if not matches_samples(image['rgb'], expected):
                        return None
                    if any(match['flags'] != 0 for match in current):
                        raise RuntimeError('DRM feedback advertised unverified hardware flags')
                    phase = {'expected_rgb': expected if check_viewport else color,
                             'output': image, 'matching_flip': current[-1]}
                    if primary_reader:
                        framebuffer = read_primary(primary_reader, drm_card, location, env)
                        if framebuffer is None or not matches_samples(
                                [sample['rgb'] for sample in framebuffer['samples']], expected):
                            return None
                        phase['virtio_primary'] = framebuffer
                    if other:
                        other_image = read_output(env, other, check_viewport)
                        if any(matches_color(rgb, sample) for rgb in other_image['rgb']
                               for sample in (red, blue, green, yellow)):
                            return None
                        phase['other_output'] = {'name': other, **other_image}
                        if primary_reader:
                            peer = read_primary(primary_reader, drm_card, other, env)
                            if peer is None or any(matches_color(sample['rgb'], test_color)
                                                   for sample in peer['samples']
                                                   for test_color in (red, blue, green, yellow)):
                                return None
                            phase['other_virtio_primary'] = peer
                    if dpms:
                        # Pixel readback can service a newer commit. Wait for all
                        # viewer callbacks to match before selecting the latest flip.
                        wire = wire_feedback(text(viewer_log))
                        verify_dark_feedback(wire, dpms['off_observed_ns'], dpms['last_off_ns'])
                        if wire.requested != len(ordered) or len(wire.presented) != len(ordered):
                            return None
                    return phase

                phases.append(wait_for('rendered color and matching KMS completion', children, evidence))
            if device_loss_card:
                connector = device_loss_card.with_name(device_loss_card.name + '-' + output)
                loss = observe_device_loss(device_loss_card, connector, host, viewer, viewer_log,
                                           host_log, events, lambda: paint('#2244cc', '#dfa922'))
                return {'output': output, 'drawn_acks': loss['drawn_acks_after'],
                        'phases': phases, 'device_loss': loss}
            viewer.stdin.write(b'{"command":"disconnect","request":1}\n')
            viewer.stdin.flush()
            if viewer.wait(timeout=5) != 0 or host.wait(timeout=5) != 0:
                raise RuntimeError('host or viewer failed to disconnect cleanly')
            if records(events)[-1]['state'] != 'closed':
                raise RuntimeError('viewer did not publish a closed session')
            drawn = sum(row['drawn_acks'] for row in host_stats(host_log))
            if drawn < len(cases):
                raise RuntimeError('host did not receive the frame acknowledgements')
            result = {'output': output, 'drawn_acks': drawn, 'phases': phases}
            if move_to_output:
                result['window_move'] = {'window_id': window_id, 'from': output, 'to': move_to_output}
                result['output_scales'] = {name: monitors[name]['scale'] for name in (output, move_to_output)}
            if hotplug:
                result['hotplug'] = hotplug
            if dpms:
                if drawn <= dpms['drawn_acks_before_off']:
                    raise RuntimeError('the same host session did not acknowledge a frame after DPMS re-enable')
                if primary_reader:
                    before, after = (phase['virtio_primary']['crtc_id'] for phase in phases)
                    if before != after:
                        raise RuntimeError('the selected output changed CRTC across the DPMS cycle')
                    dpms['crtc_id'] = after
                result['dpms'] = dpms
            return result
        except (OSError, RuntimeError, subprocess.SubprocessError):
            for log in (host_log, viewer_log, events):
                if log.exists():
                    print(log.name, text(log)[-6000:], flush=True)
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path, help='built jwm-remote executable')
    parser.add_argument('--wayland-display', type=Path, required=True, help='socket of the owned KMS compositor')
    parser.add_argument('--output', required=True, help='owned Wayland output name')
    parser.add_argument('--kms-log', type=Path, required=True, help='compositor log with jwm::kms::presentation=debug')
    parser.add_argument('--move-to-output', help='move the same viewer to a second owned output before the blue frame')
    parser.add_argument('--ipc-socket', type=Path, help='IPC socket of the owned compositor for the window move')
    parser.add_argument('--check-viewport', action='store_true', help='check samples in all four source quadrants')
    parser.add_argument('--expect-output-scales', nargs=2, type=float, metavar=('START', 'END'),
                        help='verify output layout and committed viewer buffer scales before and after the move')
    parser.add_argument('--virtio-primary-rgb', type=Path, help='compiled test-virtio-drm-primary-rgb guest executable')
    parser.add_argument('--virtio-drm-card', type=Path, help='owned virtio GPU card for primary framebuffer readback')
    parser.add_argument('--wlopm', type=Path, help='owned wlopm executable for a DPMS off/on across the blue frame')
    parser.add_argument('--drm-dpms-status', type=Path, help='sysfs dpms file for the selected owned DRM connector')
    parser.add_argument('--hotplug-status', type=Path,
                        help='sysfs status of the initial connector, disconnected and reconnected by the host fixture')
    parser.add_argument('--device-loss-card', type=Path,
                        help='observe external removal of the owned DRM card at /sys/class/drm/cardN')
    parser.add_argument('--require-path', choices=('direct', 'composited', 'cursor-overlap'),
                        help='require direct rendering or an effects-composited frame with optional cursor overlap')
    args = parser.parse_args()
    if args.move_to_output and (not args.ipc_socket or args.move_to_output == args.output):
        parser.error('--move-to-output requires --ipc-socket and a different output name')
    if args.expect_output_scales and (not args.move_to_output or any(
            not math.isfinite(scale) or scale <= 0 for scale in args.expect_output_scales)):
        parser.error('--expect-output-scales requires --move-to-output and two positive scales')
    if (args.virtio_primary_rgb or args.virtio_drm_card) and not (
            args.virtio_primary_rgb and args.virtio_drm_card and args.check_viewport):
        parser.error('primary readback requires --virtio-primary-rgb, --virtio-drm-card and --check-viewport')
    if bool(args.wlopm) != bool(args.drm_dpms_status) or (args.wlopm and args.move_to_output):
        parser.error('DPMS mode requires both --wlopm and --drm-dpms-status and a single selected output')
    if args.hotplug_status and (not args.move_to_output or args.hotplug_status.name != 'status'
                                or not args.hotplug_status.parent.name.endswith('-' + args.output)):
        parser.error('--hotplug-status requires --move-to-output and the initial connector status file')
    if args.drm_dpms_status and (args.output.startswith('*') or args.drm_dpms_status.name != 'dpms'
                                 or not args.drm_dpms_status.parent.name.endswith('-' + args.output)):
        parser.error('--drm-dpms-status must identify the selected connector')
    if args.device_loss_card and (
            not re.fullmatch(r'/sys/class/drm/card[0-9]+', str(args.device_loss_card))
            or not args.device_loss_card.is_dir() or not re.fullmatch(r'[\w.-]+', args.output)
            or args.move_to_output or args.wlopm):
        parser.error('--device-loss-card requires a present DRM card and a single owned connector')
    primary_reader = args.virtio_primary_rgb.resolve() if args.virtio_primary_rgb else None
    print('JWM_REMOTE_KMS ' + json.dumps(exercise(str(args.binary.resolve()), args.wayland_display.resolve(),
                                                 args.output, args.kms_log, args.require_path,
                                                 args.move_to_output, args.ipc_socket,
                                                 args.check_viewport, args.expect_output_scales,
                                                 primary_reader, args.virtio_drm_card,
                                                 args.wlopm.resolve() if args.wlopm else None,
                                                 args.drm_dpms_status, args.hotplug_status, args.device_loss_card)))


if __name__ == '__main__':
    main()
