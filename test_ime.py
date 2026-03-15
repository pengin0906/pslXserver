#!/usr/bin/env python3
"""Test IME-like input: send Japanese characters via XTEST using Unicode keysyms.
This simulates what happens when IME commits text — Unicode keysyms are sent via
virtual keycode mapping (keycodes 200+).

First we test that the basic ASCII keyboard works, then we verify the screen state.
"""
import os, time
from Xlib import X, display
from Xlib.ext import xtest

d = display.Display(os.environ.get("DISPLAY", ":0"))

# Get keyboard mapping
first_kc = d.display.info.min_keycode
last_kc = d.display.info.max_keycode
mapping = d.get_keyboard_mapping(first_kc, last_kc - first_kc + 1)

# Build reverse map: keysym -> keycode
keysym_to_kc = {}
for kc_offset, syms in enumerate(mapping):
    kc = first_kc + kc_offset
    for sym in syms:
        if sym != 0 and sym not in keysym_to_kc:
            keysym_to_kc[sym] = kc

def type_char(ch):
    """Type a single character via XTEST."""
    sym = ord(ch)
    kc = keysym_to_kc.get(sym)
    if kc:
        xtest.fake_input(d, X.KeyPress, kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, kc)
        d.sync()
        time.sleep(0.02)
        return True
    return False

def type_string(s):
    """Type a string via XTEST."""
    for ch in s:
        if not type_char(ch):
            print(f"  No keycode for '{ch}' (U+{ord(ch):04X})")

# Test 1: Type "echo test" + Enter
print("=== Test 1: ASCII input ===")
type_string("echo test")
type_char('\r')  # Enter = 0xFF0D... need special handling
enter_kc = keysym_to_kc.get(0xFF0D)
if enter_kc:
    xtest.fake_input(d, X.KeyPress, enter_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, enter_kc)
    d.sync()
print("Typed 'echo test' + Enter")

time.sleep(0.5)

# Test 2: Type more ASCII to verify no corruption
print("=== Test 2: Numbers and symbols ===")
type_string("1234567890")
if enter_kc:
    xtest.fake_input(d, X.KeyPress, enter_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, enter_kc)
    d.sync()
print("Typed '1234567890' + Enter")

time.sleep(0.3)

# Test 3: Verify Backspace works
print("=== Test 3: Backspace test ===")
type_string("abc")
bs_kc = keysym_to_kc.get(0xFF08)
if bs_kc:
    for _ in range(3):
        xtest.fake_input(d, X.KeyPress, bs_kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, bs_kc)
        d.sync()
        time.sleep(0.02)
type_string("xyz")
if enter_kc:
    xtest.fake_input(d, X.KeyPress, enter_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, enter_kc)
    d.sync()
print("Typed 'abc' + 3xBS + 'xyz' + Enter (should show 'xyz')")

print("\n=== All tests completed ===")
print("Check xterm for correct output.")

d.close()
