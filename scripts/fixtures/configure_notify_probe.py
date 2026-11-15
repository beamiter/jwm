"""Opt-in, post-failure single ConfigureNotify intervention on an owned top window.

Runs in a timeout-bounded subprocess: Xlib errors cannot kill the fixture.
Never changes geometry, paints, or converts the original acceptance failure.
"""
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

if __package__ in (None, ''):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from fixtures import frame_feedback_probe


class ConfigureEvent(C.Structure):
    _fields_ = [('type', C.c_int), ('serial', C.c_ulong), ('send_event', C.c_int),
                ('display', C.c_void_p), ('event', C.c_ulong), ('window', C.c_ulong),
                ('x', C.c_int), ('y', C.c_int), ('width', C.c_int), ('height', C.c_int),
                ('border_width', C.c_int), ('above', C.c_ulong), ('override_redirect', C.c_int)]


class XEvent(C.Union):
    _fields_ = [('configure', ConfigureEvent), ('pad', C.c_long * 24)]


def phase(raw, name):
    if name in ('READY', 'RENDERED'):
        return bool(re.search(r'_JWM_FRONTEND_' + name + r'\(CARDINAL\)\s*=\s*1\s*$', raw, re.M))
    # Stage reports carry nonempty UTF8_STRING diagnostic messages, not booleans.
    return bool(re.search(r'_JWM_FRONTEND_' + name + r'\(UTF8_STRING\)\s*=\s*".+"\s*$', raw, re.M))


def stalled(raw, counters, xid):
    return (phase(raw, 'READY') and phase(raw, 'DOM_READY')
            and not phase(raw, 'RAF_FIRST') and not phase(raw, 'RENDERED')
            and counters['windows'][str(xid)]['counters'][1]['value'] == 1)


def process(pid):
    return {'identity': frame_feedback_probe.identity(pid),
            'exe': str(Path(f'/proc/{pid}/exe').resolve(strict=True))}


def command(*argv):
    return subprocess.check_output(list(map(str, argv)), text=True, stderr=subprocess.STDOUT, timeout=5)


def validate(target):
    current = process(target['pid'])
    if list(current['identity'] or []) != list(target['identity']) or current['exe'] != target['exe']:
        raise ValueError('frontend process identity changed')
    xid = target['xid']
    raw = command('xprop', '-id', xid, '_NET_WM_PID', '_JWM_FRONTEND_READY')
    if not re.search(r'_NET_WM_PID\([^)]*\)\s*=\s*' + str(target['pid']) + r'\s*$', raw, re.M):
        raise ValueError('window PID mismatch')
    if not re.search(r'_JWM_FRONTEND_READY\([^)]*\)\s*=\s*1\s*$', raw, re.M):
        raise ValueError('not the verified frontend surface')
    raw = command('xwininfo', '-id', xid, '-tree', '-stats')
    root = re.search(r'Root window id:\s*(0x[0-9a-f]+)', raw)
    parent = re.search(r'Parent window id:\s*(0x[0-9a-f]+)', raw)
    if not root or not parent or root[1] != parent[1] or 'Map State: IsViewable' not in raw or 'Override Redirect State: no' not in raw:
        raise ValueError('window is not the mapped managed root child')
    def value(label):
        match = re.search(re.escape(label) + r':\s*(-?\d+)', raw)
        if not match:
            raise ValueError('missing geometry ' + label)
        return int(match[1])
    geom = [value(label) for label in ('Absolute upper-left X', 'Absolute upper-left Y', 'Width', 'Height', 'Border width')]
    if not (geom[2] > 0 and geom[3] > 0):
        raise ValueError('invalid geometry')
    return geom


def make_event(display, xid, geometry):
    event = XEvent()
    event.configure = ConfigureEvent(22, 0, 1, display, xid, xid, *geometry, 0, 0)
    return event


def send_once(target, expected_geometry):
    # Revalidate immediately before sending; no persistent or global state is changed.
    if validate(target) != expected_geometry:
        raise ValueError('geometry changed before intervention')
    lib = C.CDLL('libX11.so.6')
    lib.XOpenDisplay.argtypes = [C.c_char_p]; lib.XOpenDisplay.restype = C.c_void_p
    lib.XSendEvent.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_long, C.POINTER(XEvent)]
    lib.XSendEvent.restype = C.c_int
    lib.XSync.argtypes = [C.c_void_p, C.c_int]
    lib.XCloseDisplay.argtypes = [C.c_void_p]
    display = lib.XOpenDisplay(os.environ['DISPLAY'].encode())
    if not display:
        raise RuntimeError('cannot open fixture display')
    try:
        event = make_event(display, target['xid'], expected_geometry)
        if not lib.XSendEvent(display, target['xid'], 0, 1 << 17, C.byref(event)):
            raise RuntimeError('XSendEvent failed')
        lib.XSync(display, 0)
    finally:
        lib.XCloseDisplay(display)


def snapshot(target, directory, name):
    geometry = validate(target)
    phases = command('xprop', '-id', target['xid'], *[
        '_JWM_FRONTEND_' + name for name in ('READY', 'RENDERED', 'BOOT', 'ERROR', 'DOM_READY',
        'VISIBILITY', 'NATIVE_DOM_READY', 'NATIVE_VISIBILITY', 'RAF_FIRST', 'RAF_SECOND', 'RENDER_REPORT_REJECTED')])
    counters = frame_feedback_probe.snapshot([target['xid']])
    image = directory / (name + '.png')
    command('import', '-window', 'root', image)
    if validate(target) != geometry:
        raise ValueError('geometry changed during capture')
    return {'time': time.monotonic(), 'geometry': geometry, 'phases': phases,
            'counters': counters, 'screenshot': str(image),
            'screenshot_sha256': hashlib.sha256(image.read_bytes()).hexdigest()}


def compare(before, after):
    from PIL import Image
    if before['geometry'] != after['geometry']:
        raise ValueError('cannot compare changed top geometry')
    x, y, w, h, _ = before['geometry']
    with Image.open(before['screenshot']) as first, Image.open(after['screenshot']) as second:
        if first.size != second.size:
            raise ValueError('display dimensions changed')
        if x < 0 or y < 0 or x+w > first.width or y+2*h > first.height:
            raise ValueError('top/background crop outside display')
        a, b = first.convert('RGB'), second.convert('RGB')
        def changed(box):
            return sum(max(abs(x-y) for x,y in zip(p,q)) >= 12
                       for p,q in zip(a.crop(box).getdata(), b.crop(box).getdata()))
        # Same top rectangle, plus an equal-size immediately adjacent background
        # strip. This is evidence, not an automatic successful-paint verdict.
        return {'top_changed_pixels': changed((x,y,x+w,y+h)),
                'background_changed_pixels': changed((x,y+h,x+w,y+2*h)),
                'top_rect': [x,y,w,h], 'background_rect': [x,y+h,w,h]}


def experiment(target, directory, take=snapshot, send=send_once, sleep=time.sleep, diff=compare):
    result = {'original_acceptance': 'failed', 'intervention': 'single synthetic ConfigureNotify',
              'target': target, 'sent_count': 0, 'rescue_observed': False, 'snapshots': {}}
    def save():
        (directory / 'probe.json').write_text(json.dumps(result, indent=2))
    try:
        result['snapshots']['before'] = take(target, directory, 'before'); save()
        before = result['snapshots']['before']
        if not stalled(before['phases'], before['counters'], target['xid']):
            result['not_sent_reason'] = 'initial sample is not the scoped first-frame stall'
            return result
        sleep(3)
        result['snapshots']['sham'] = take(target, directory, 'sham')
        result['sham_pixels'] = diff(result['snapshots']['before'], result['snapshots']['sham']); save()
        if result['snapshots']['before']['geometry'] != result['snapshots']['sham']['geometry']:
            raise ValueError('geometry changed during sham')
        sham = result['snapshots']['sham']
        if not stalled(sham['phases'], sham['counters'], target['xid']):
            result['not_sent_reason'] = 'first-frame stall changed during sham'
            return result
        before_counter = before['counters']['windows'][str(target['xid'])]['counters'][1]['id']
        if before_counter != sham['counters']['windows'][str(target['xid'])]['counters'][1]['id']:
            raise ValueError('extended counter identity changed during sham')
        result['send_attempted'] = True; save()
        send(target, result['snapshots']['sham']['geometry'])
        result['sent_count'] = 1; result['sent_at'] = time.monotonic(); save()
        sleep(3)
        result['snapshots']['after'] = take(target, directory, 'after')
        result['intervention_pixels'] = diff(result['snapshots']['sham'], result['snapshots']['after'])
        sham, after = result['snapshots']['sham'], result['snapshots']['after']
        def extended(sample):
            return sample['counters']['windows'][str(target['xid'])]['counters'][1]
        old, new = extended(sham), extended(after)
        result['rescue_observed'] = (not phase(sham['phases'], 'RENDERED') and old['value'] == 1
            and old['id'] == new['id'] and isinstance(new['value'], int) and new['value'] > 1
            and all(phase(after['phases'], name) for name in ('RAF_FIRST', 'RAF_SECOND', 'RENDERED'))
            and result['intervention_pixels']['top_changed_pixels'] > 5000)
    except Exception as error:
        result['error'] = str(error)
    finally:
        save()
    return result


def launch(target, evidence, env):
    directory = evidence / 'configure-notify-probe'
    directory.mkdir(exist_ok=True)
    request = directory / 'target.json'
    request.write_text(json.dumps(target))
    result = {'original_acceptance': 'failed'}
    try:
        completed = subprocess.run([sys.executable, str(Path(__file__).resolve()), str(request)],
                                   env=env, capture_output=True, text=True, timeout=35)
        result.update(returncode=completed.returncode, stdout=completed.stdout, stderr=completed.stderr)
    except (OSError, subprocess.TimeoutExpired) as error:
        result['error'] = str(error)
    if (directory / 'probe.json').exists():
        result['evidence'] = json.loads((directory / 'probe.json').read_text())
    return result


if __name__ == '__main__':
    request = Path(sys.argv[1])
    experiment(json.loads(request.read_text()), request.parent)
