#!/usr/bin/env python3
"""Deterministic managed X11 window:500x500 changing scanlines at60Hz.

Only the caller-selected DISPLAY is used. The parent fixture supplies an owned
Xvfb server; no user display is selected here. Unlike x11perf, this producer has
no autocalibration/visibility wait between repeated benchmark windows.
"""

import ctypes as C
import ctypes.util
import json
import os
import signal
import time


def main():
    x = C.CDLL(ctypes.util.find_library("X11"))
    ptr = C.c_void_p
    integer = C.c_int
    ulong = C.c_ulong
    uint = C.c_uint

    def f(name, result, *arguments):
        value = getattr(x, name)
        value.restype = result
        value.argtypes = arguments
        return value

    open_display = f("XOpenDisplay", ptr, C.c_char_p)
    close_display = f("XCloseDisplay", integer, ptr)
    root_window = f("XDefaultRootWindow", ulong, ptr)
    create = f(
        "XCreateSimpleWindow",
        ulong,
        ptr,
        ulong,
        integer,
        integer,
        uint,
        uint,
        uint,
        ulong,
        ulong,
    )
    create_gc = f("XCreateGC", ptr, ptr, ulong, ulong, ptr)
    set_color = f("XSetForeground", integer, ptr, ptr, ulong)
    fill = f("XFillRectangle", integer, ptr, ulong, ptr, integer, integer, uint, uint)
    copy = f(
        "XCopyArea",
        integer,
        ptr,
        ulong,
        ulong,
        ptr,
        integer,
        integer,
        uint,
        uint,
        integer,
        integer,
    )
    expose = f("XSetGraphicsExposures", integer, ptr, ptr, integer)
    flush = f("XFlush", integer, ptr)
    pending = f("XPending", integer, ptr)
    next_event = f("XNextEvent", integer, ptr, ptr)
    atom = f("XInternAtom", ulong, ptr, C.c_char_p, integer)
    prop = f(
        "XChangeProperty",
        integer,
        ptr,
        ulong,
        ulong,
        ulong,
        integer,
        integer,
        ptr,
        integer,
    )
    name = f("XStoreName", integer, ptr, ulong, C.c_char_p)
    map_window = f("XMapWindow", integer, ptr, ulong)
    select_input = f("XSelectInput", integer, ptr, ulong, C.c_long)
    destroy = f("XDestroyWindow", integer, ptr, ulong)
    free_gc = f("XFreeGC", integer, ptr, ptr)

    class Hint(C.Structure):
        _fields_ = [("res_name", C.c_char_p), ("res_class", C.c_char_p)]

    set_class = f("XSetClassHint", integer, ptr, ulong, C.POINTER(Hint))
    display = open_display(None)
    if not display:
        raise RuntimeError("owned DISPLAY unavailable")
    window = gc = None
    stop = False

    def stopping(_number, _frame):
        nonlocal stop
        stop = True

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, stopping)
    try:
        window = create(display, root_window(display), 10, 10, 600, 600, 0, 0, 0)
        gc = create_gc(display, window, 0, None)
        if not window or not gc:
            raise RuntimeError("X11 workload allocation failed")
        name(display, window, b"Controlled scroll500 workload")
        hint = Hint(b"controlled-scroll", b"PerfScrollFixture")
        set_class(display, window, C.byref(hint))
        pid = ulong(os.getpid())
        prop(
            display,
            window,
            atom(display, b"_NET_WM_PID", 0),
            atom(display, b"CARDINAL", 0),
            32,
            0,
            C.byref(pid),
            1,
        )
        normal = ulong(atom(display, b"_NET_WM_WINDOW_TYPE_NORMAL", 0))
        prop(
            display,
            window,
            atom(display, b"_NET_WM_WINDOW_TYPE", 0),
            atom(display, b"ATOM", 0),
            32,
            0,
            C.byref(normal),
            1,
        )
        select_input(display, window, (1 << 17) | (1 << 15))
        expose(display, gc, 0)
        map_window(display, window)
        for row in range(500):
            color = ((row * 17 % 256) << 16) | ((row * 7 % 256) << 8) | (row * 29 % 256)
            set_color(display, gc, color)
            fill(display, window, gc, 0, row, 500, 1)
        flush(display)
        print(
            json.dumps(
                {
                    "workload": "managed-scroll500-v2",
                    "pid": os.getpid(),
                    "window": window,
                    "region": [0, 0, 500, 500],
                    "producer_hz": 60,
                    "clock": "monotonic",
                    "managed": True,
                }
            ),
            flush=True,
        )
        deadline = time.monotonic()
        counter = 0
        event = (C.c_long * 32)()
        while not stop:
            for _ in range(64):
                if not pending(display):
                    break
                next_event(display, C.byref(event))
            copy(display, window, window, gc, 0, 1, 500, 499, 0, 0)
            color = (
                ((counter * 13 % 256) << 16)
                | ((counter * 37 % 256) << 8)
                | (counter * 71 % 256)
            )
            set_color(display, gc, color)
            fill(display, window, gc, 0, 499, 500, 1)
            flush(display)
            counter += 1
            deadline = max(deadline + 1 / 60, time.monotonic())
            time.sleep(max(0, deadline - time.monotonic()))
    finally:
        if gc:
            free_gc(display, gc)
        if window:
            destroy(display, window)
        close_display(display)


if __name__ == "__main__":
    main()
