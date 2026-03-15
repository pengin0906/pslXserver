#!/usr/bin/env python3
"""Test: does BS correctly erase CJK characters sent via virtual keycodes?"""
import os, time
from Xlib import X, display
from Xlib.ext import xtest

d = display.Display(os.environ.get("DISPLAY", ":0"))
first_kc = d.display.info.min_keycode
mapping = d.get_keyboard_mapping(first_kc, d.display.info.max_keycode - first_kc + 1)

keysym_to_kc = {}
for kc_offset, syms in enumerate(mapping):
    kc = first_kc + kc_offset
    for sym in syms:
        if sym != 0 and sym not in keysym_to_kc:
            keysym_to_kc[sym] = kc

bs_kc = keysym_to_kc.get(0xFF08)  # BackSpace
enter_kc = keysym_to_kc.get(0xFF0D)

def send_unicode_char(codepoint, keycode=200):
    unicode_keysym = 0x01000000 | codepoint
    d.change_keyboard_mapping(keycode, [(unicode_keysym,)])
    d.sync()
    time.sleep(0.05)
    xtest.fake_input(d, X.KeyPress, keycode)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, keycode)
    d.sync()
    time.sleep(0.03)

def send_bs(count):
    for _ in range(count):
        xtest.fake_input(d, X.KeyPress, bs_kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, bs_kc)
        d.sync()
        time.sleep(0.03)

def type_ascii(s):
    for ch in s:
        kc = keysym_to_kc.get(ord(ch))
        if kc:
            xtest.fake_input(d, X.KeyPress, kc)
            d.sync()
            xtest.fake_input(d, X.KeyRelease, kc)
            d.sync()
            time.sleep(0.02)

# Test 1: Type 'ちに' via virtual keycodes, then BS twice, then 'AB'
print("Test 1: ちに + 2xBS + AB (expect 'AB')")
send_unicode_char(ord('ち'), 200)  # ち
send_unicode_char(ord('に'), 201)  # に
time.sleep(0.2)
send_bs(2)
time.sleep(0.2)
type_ascii("AB")
xtest.fake_input(d, X.KeyPress, enter_kc)
d.sync()
xtest.fake_input(d, X.KeyRelease, enter_kc)
d.sync()
time.sleep(0.3)

# Test 2: Send 'あいう' then 3 BS, then 'XY'
print("Test 2: あいう + 3xBS + XY (expect 'XY')")
send_unicode_char(ord('あ'), 200)
send_unicode_char(ord('い'), 201)
send_unicode_char(ord('う'), 202)
time.sleep(0.2)
send_bs(3)
time.sleep(0.2)
type_ascii("XY")
xtest.fake_input(d, X.KeyPress, enter_kc)
d.sync()
xtest.fake_input(d, X.KeyRelease, enter_kc)
d.sync()
time.sleep(0.3)

# Test 3: Same but using keycode 59 directly (what send_backspaces uses)
print("Test 3: 漢字 + 2xBS(keycode59) + OK (expect 'OK')")
send_unicode_char(ord('漢'), 200)
send_unicode_char(ord('字'), 201)
time.sleep(0.2)
# BS using keycode 59 directly
for _ in range(2):
    xtest.fake_input(d, X.KeyPress, 59)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, 59)
    d.sync()
    time.sleep(0.03)
time.sleep(0.2)
type_ascii("OK")
xtest.fake_input(d, X.KeyPress, enter_kc)
d.sync()
xtest.fake_input(d, X.KeyRelease, enter_kc)
d.sync()

print("\nCheck xterm — each line should show only the ASCII part:")
print("  AB")
print("  XY")
print("  OK")
d.close()
