#!/usr/bin/env python3
"""Send a keysym to the focused X11/XWayland window via XTEST.
Usage: xtest_key.py <keysym-hex> [--shift]   e.g. 0xff09 (Tab), 0x20 (Space), 0x3f (?)

Resolves the keysym through the actual keyboard mapping (XGetKeyboardMapping)
and presses the modifier level it lives at — XKeysymToKeycode alone can return
a keycode where the keysym sits at a shifted level, which then sends the WRONG
key (e.g. F4 arriving as F6). ctypes restype/argtypes are required on 64-bit."""
import ctypes, ctypes.util, sys

def main():
    ks = int(sys.argv[1], 16)
    force_shift = "--shift" in sys.argv
    x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
    xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XGetKeyboardMapping.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
    x11.XGetKeyboardMapping.restype = ctypes.POINTER(ctypes.c_ulong)
    x11.XFree.argtypes = [ctypes.c_void_p]
    xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
    d = x11.XOpenDisplay(None)
    if not d:
        sys.exit("no X display (set DISPLAY=:0)")

    first, last = 8, 255
    # winit maps F-keys/navigation by evdev scancode, NOT the X keymap — on
    # keymaps with a non-standard F-row the mapping lookup sends the wrong
    # key (F4 at kc 72 arrives as F6). Use the standard evdev+8 codes.
    overrides = {0xFFBE + i: 67 + i for i in range(12)}  # F1..F12 -> 67..78
    overrides.update({0xFF51: 113, 0xFF52: 111, 0xFF53: 114, 0xFF54: 116})  # arrows
    if ks in overrides:
        kc, level = overrides[ks], 0
        found = (kc, level)
    else:
        per = ctypes.c_int(0)
        syms = x11.XGetKeyboardMapping(d, first, last - first + 1, ctypes.byref(per))
        if not syms:
            sys.exit("XGetKeyboardMapping failed")
        n = per.value
        # Find (keycode, level) for the keysym; prefer level 0, then 1 (Shift).
        found = None
        for level in (0, 1, 2, 3):
            for kc in range(first, last + 1):
                if level < n and syms[(kc - first) * n + level] == ks:
                    found = (kc, level)
                    break
            if found:
                break
        x11.XFree(syms)
    if not found:
        sys.exit(f"keysym {hex(ks)} not found in keyboard mapping")
    kc, level = found
    shift = force_shift or level == 1
    # Levels 2/3 need AltGr (Mode_switch) — press it too if present.
    altgr = None
    if level >= 2:
        altgr = 108  # typical Mode_switch keycode; best effort
    sh = None
    if shift:
        # Shift_L keycode: find via mapping (keysym 0xffe1 at level 0).
        syms2 = x11.XGetKeyboardMapping(d, first, last - first + 1, ctypes.byref(per))
        for kc2 in range(first, last + 1):
            if syms2[(kc2 - first) * per.value] == 0xFFE1:
                sh = kc2
                break
        x11.XFree(syms2)
        if sh is None:
            sh = 50  # fallback Shift_L
    if altgr:
        xtst.XTestFakeKeyEvent(d, altgr, True, 0)
    if sh:
        xtst.XTestFakeKeyEvent(d, sh, True, 0)
    xtst.XTestFakeKeyEvent(d, kc, True, 0)
    xtst.XTestFakeKeyEvent(d, kc, False, 0)
    if sh:
        xtst.XTestFakeKeyEvent(d, sh, False, 0)
    if altgr:
        xtst.XTestFakeKeyEvent(d, altgr, False, 0)
    x11.XFlush(d)
    x11.XCloseDisplay(d)
    print(f"sent keysym {hex(ks)} (kc {kc}, level {level}){' +shift' if shift else ''}")

if __name__ == "__main__":
    main()
