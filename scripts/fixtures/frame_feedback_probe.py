"""Bounded read-only XSync evidence for an owned fixture display."""
import ctypes
import json
import os
import re
import subprocess
import sys
from pathlib import Path
if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from fixtures.daemon_native_log import NativeLogEvidence

MODULE = 'jwm::backend::x11::compositor::frame_sync'
DEFAULT_FILTER = 'info,jwm=info,smithay=warn,libseat=warn,drm=warn'


def enable(env, bar):
    if bar.startswith('tauri_'):
        levels, slash, regex = env.get('RUST_LOG', DEFAULT_FILTER).partition('/')
        env['RUST_LOG'] = levels.rstrip(',') + ',' + MODULE + '=debug' + slash + regex


def identity(pid):
    try:
        stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return None if stat[0] == "Z" else (pid, stat[19])
    except (OSError, IndexError):
        return None


def pin_logs(pid, executable, started):
    expected = identity(pid)
    if expected is None or Path(f'/proc/{pid}/exe').resolve() != executable.resolve():
        raise ValueError('fixture JWM executable/identity mismatch')
    logs = NativeLogEvidence(pid, expected, executable.resolve(), identity, started)
    logs.discover()
    return logs


def collect(run, windows):
    # Query in an isolated, timeout-bounded process: a broken X connection must
    # not terminate the fixture or hide its original assertion.
    return json.loads(run(sys.executable, Path(__file__).resolve(), *map(str, windows)))


def counter_ids(properties):
    match = re.search(r'^_NET_WM_SYNC_REQUEST_COUNTER\([^)]*\)\s*=\s*(.*)$', properties, re.M)
    return [int(value.strip(), 0) for value in match[1].split(',')] if match else []


def snapshot(windows):
    def xprop(*args):
        return subprocess.check_output(['xprop', *args], text=True, stderr=subprocess.STDOUT, timeout=5)
    result = {'root': xprop('-root', '_NET_SUPPORTED', '_JWM_FRAME_SYNC_OWNER'), 'windows': {}}
    x11 = ctypes.CDLL('libX11.so.6')
    xext = ctypes.CDLL('libXext.so.6')
    class XSyncValue(ctypes.Structure):
        _fields_ = [('hi', ctypes.c_int32), ('lo', ctypes.c_uint32)]
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]; x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    xext.XSyncInitialize.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int)]
    xext.XSyncInitialize.restype = ctypes.c_int
    xext.XSyncQueryCounter.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(XSyncValue)]
    xext.XSyncQueryCounter.restype = ctypes.c_int
    display = x11.XOpenDisplay(os.environ['DISPLAY'].encode())
    if not display:
        raise RuntimeError('cannot open owned fixture display')
    try:
        major, minor = ctypes.c_int(), ctypes.c_int()
        if not xext.XSyncInitialize(display, ctypes.byref(major), ctypes.byref(minor)):
            raise RuntimeError('owned fixture display does not support XSync')
        result['sync_version'] = [major.value, minor.value]
        if major.value < 3:
            raise RuntimeError(f'unsupported XSync version {major.value}.{minor.value}')
        for window in windows:
            properties = xprop('-id', str(window), 'WM_PROTOCOLS', '_NET_WM_SYNC_REQUEST_COUNTER')
            entry = {'properties': properties, 'counters': []}
            result['windows'][str(window)] = entry
            for counter in counter_ids(properties)[:2]:
                value = XSyncValue()
                ok = xext.XSyncQueryCounter(display, counter, ctypes.byref(value))
                entry['counters'].append({'id': counter, 'value': (value.hi << 32) | value.lo if ok else None})
    finally:
        x11.XCloseDisplay(display)
    return result




def archive(logs, evidence, result):
    if logs is not None:
        try:
            logs.discover()
            result['frame_feedback_native_log'] = logs.archive(evidence, 'frame-feedback')
        except Exception as error:
            result['frame_feedback_native_log_error'] = str(error)
        finally:
            try:
                logs.close()
            except Exception as error:
                result['frame_feedback_native_log_close_error'] = str(error)


if __name__ == '__main__':
    print(json.dumps(snapshot([int(value, 0) for value in sys.argv[1:]])))
