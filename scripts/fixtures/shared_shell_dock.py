#!/usr/bin/env python3
"""Real EWMH dock with its type, geometry and strut set before MapWindow."""
import ctypes
import signal
import sys

x = ctypes.CDLL("libX11.so.6")
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XOpenDisplay.restype = ctypes.c_void_p
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x.XDefaultRootWindow.restype = ctypes.c_ulong
x.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                 ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong]
x.XCreateSimpleWindow.restype = ctypes.c_ulong
x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x.XInternAtom.restype = ctypes.c_ulong
x.XChangeProperty.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong,
                             ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
x.XStoreName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_char_p]
x.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XFlush.argtypes = [ctypes.c_void_p]
x.XDestroyWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XCloseDisplay.argtypes = [ctypes.c_void_p]
height = int(sys.argv[1])
display = x.XOpenDisplay(None)
assert display, "private X11 display unavailable"
window = x.XCreateSimpleWindow(display, x.XDefaultRootWindow(display), 0, 80, 60,
                              max(100, height - 160), 0, 0, 0xeeeeee)
def atom(name):
    return x.XInternAtom(display, name.encode(), False)
def property32(name, kind, values):
    data = (ctypes.c_ulong * len(values))(*values)
    x.XChangeProperty(display, window, atom(name), atom(kind), 32, 0, data, len(values))
x.XStoreName(display, window, b"SharedExternalDock")
property32("_NET_WM_WINDOW_TYPE", "ATOM", [atom("_NET_WM_WINDOW_TYPE_DOCK")])
property32("_NET_WM_STRUT", "CARDINAL", [90, 0, 0, 0])
property32("_NET_WM_STRUT_PARTIAL", "CARDINAL", [90, 0, 0, 0, 0, height - 1, 0, 0, 0, 0, 0, 0])
x.XMapWindow(display, window)
x.XFlush(display)
def finish(_signum, _frame):
    raise SystemExit(0)
signal.signal(signal.SIGTERM, finish)
try:
    while True:
        signal.pause()
finally:
    x.XDestroyWindow(display, window)
    x.XCloseDisplay(display)
