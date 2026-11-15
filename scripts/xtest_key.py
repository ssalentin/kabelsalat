#!/usr/bin/env python3
"""Send a keysym to the focused X11/XWayland window via XTEST.
Usage: xtest_key.py <keysym-hex> [--shift]   e.g. 0xff09 (Tab), 0x20 (Space), 0x3f (?)
NOTE: ctypes restype/argtypes below are required — without them XOpenDisplay's
pointer is truncated and the call segfaults on 64-bit."""
import ctypes, ctypes.util, sys

def main():
    ks = int(sys.argv[1], 16)
    shift = "--shift" in sys.argv
    x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
    xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XKeysymToKeycode.restype = ctypes.c_uint
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
    d = x11.XOpenDisplay(None)
    if not d:
        sys.exit("no X display (set DISPLAY=:0)")
    kc = x11.XKeysymToKeycode(d, ks)
    sh = x11.XKeysymToKeycode(d, 0xFFE1) if shift else 0
    if shift:
        xtst.XTestFakeKeyEvent(d, sh, True, 0)
    xtst.XTestFakeKeyEvent(d, kc, True, 0)
    xtst.XTestFakeKeyEvent(d, kc, False, 0)
    if shift:
        xtst.XTestFakeKeyEvent(d, sh, False, 0)
    x11.XFlush(d)
    x11.XCloseDisplay(d)
    print(f"sent keysym {hex(ks)} (kc {kc}){' +shift' if shift else ''}")

if __name__ == "__main__":
    main()
