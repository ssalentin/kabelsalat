#!/usr/bin/env python3
"""Click at X screen coordinates via XTEST (move + left button press/release).

Usage: xtest_click.py X Y   — coordinates in the X (XWayland) screen space.
Callers must check `hyprctl activewindow` first (AGENTS.md rule 24).
"""
import ctypes
import sys
import time


def main():
    x, y = int(sys.argv[1]), int(sys.argv[2])
    xlib = ctypes.CDLL("libX11.so.6")
    xtst = ctypes.CDLL("libXtst.so.6")
    xlib.XOpenDisplay.restype = ctypes.c_void_p
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XFlush.argtypes = [ctypes.c_void_p]
    xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
    xtst.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
    d = xlib.XOpenDisplay(None)
    if not d:
        sys.exit("cannot open X display")
    xtst.XTestFakeMotionEvent(d, -1, x, y, 0)
    xlib.XFlush(d)
    time.sleep(0.05)
    xtst.XTestFakeButtonEvent(d, 1, 1, 0)
    xlib.XFlush(d)
    time.sleep(0.05)
    xtst.XTestFakeButtonEvent(d, 1, 0, 0)
    xlib.XFlush(d)
    xlib.XCloseDisplay(d)
    print(f"clicked {x},{y}")


if __name__ == "__main__":
    main()
