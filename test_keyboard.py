#!/usr/bin/env python3
"""Test keyboard input via XTEST - send ASCII chars and verify keyboard map."""
import os, time
from Xlib import X, display
from Xlib.ext import xtest

d = display.Display(os.environ.get("DISPLAY", ":0"))

# Get keyboard mapping
first_kc = d.display.info.min_keycode
mapping = d.get_keyboard_mapping(first_kc, d.display.info.max_keycode - first_kc + 1)

# Build reverse map: keysym -> keycode
keysym_to_kc = {}
for kc_offset, syms in enumerate(mapping):
    kc = first_kc + kc_offset
    for sym in syms:
        if sym != 0 and sym not in keysym_to_kc:
            keysym_to_kc[sym] = kc

print("=== Testing ASCII character input via XTEST ===")
test_chars = "abcdefghijklmnopqrstuvwxyz0123456789"
for ch in test_chars:
    sym = ord(ch)
    kc = keysym_to_kc.get(sym)
    if kc:
        xtest.fake_input(d, X.KeyPress, kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, kc)
        d.sync()
        time.sleep(0.02)
    else:
        print(f"  No keycode for '{ch}' (keysym 0x{sym:04X})")

# Send Enter
enter_kc = keysym_to_kc.get(0xFF0D)
if enter_kc:
    xtest.fake_input(d, X.KeyPress, enter_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, enter_kc)
    d.sync()

print(f"Sent: '{test_chars}' + Enter")
print(f"Check xterm - you should see: {test_chars}")

d.close()
