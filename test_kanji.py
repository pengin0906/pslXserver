#!/usr/bin/env python3
"""Test kanji/CJK input via XTEST by temporarily remapping keycodes to Unicode keysyms.

This simulates what the IME commit pipeline does:
1. Map virtual keycodes (200+) to Unicode keysyms (0x01000000 | codepoint)
2. Send KeyPress/KeyRelease via XTEST
3. xterm decodes Unicode keysym -> UTF-8 and displays the character
"""
import os, time
from Xlib import X, display, Xatom
from Xlib.ext import xtest

d = display.Display(os.environ.get("DISPLAY", ":0"))

first_kc = d.display.info.min_keycode
last_kc = d.display.info.max_keycode
mapping = d.get_keyboard_mapping(first_kc, last_kc - first_kc + 1)

# Build keysym -> keycode map for ASCII
keysym_to_kc = {}
for kc_offset, syms in enumerate(mapping):
    kc = first_kc + kc_offset
    for sym in syms:
        if sym != 0 and sym not in keysym_to_kc:
            keysym_to_kc[sym] = kc

enter_kc = keysym_to_kc.get(0xFF0D)
bs_kc = keysym_to_kc.get(0xFF08)

def type_ascii(s):
    for ch in s:
        kc = keysym_to_kc.get(ord(ch))
        if kc:
            xtest.fake_input(d, X.KeyPress, kc)
            d.sync()
            xtest.fake_input(d, X.KeyRelease, kc)
            d.sync()
            time.sleep(0.02)

def press_enter():
    if enter_kc:
        xtest.fake_input(d, X.KeyPress, enter_kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, enter_kc)
        d.sync()
        time.sleep(0.05)

def send_unicode_char(codepoint, keycode=200):
    """Send a Unicode character by remapping a keycode to Unicode keysym."""
    unicode_keysym = 0x01000000 | codepoint
    # ChangeKeyboardMapping: map keycode to the Unicode keysym
    # keysyms_per_keycode = 1 (we only set the first column)
    d.change_keyboard_mapping(keycode, [(unicode_keysym,)])
    d.sync()
    time.sleep(0.05)  # wait for xterm to process MappingNotify

    xtest.fake_input(d, X.KeyPress, keycode)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, keycode)
    d.sync()
    time.sleep(0.03)

def send_unicode_string(text):
    """Send a Unicode string character by character."""
    for i, ch in enumerate(text):
        cp = ord(ch)
        kc = 200 + (i % 50)  # Use keycodes 200-249
        send_unicode_char(cp, kc)

print("=== Test 1: Single kanji characters ===")
type_ascii("echo ")
# Send 漢字 (kanji)
send_unicode_string("漢字")
press_enter()
print("Sent: echo 漢字")
time.sleep(0.3)

print("=== Test 2: Hiragana ===")
type_ascii("echo ")
send_unicode_string("こんにちは")
press_enter()
print("Sent: echo こんにちは")
time.sleep(0.3)

print("=== Test 3: Katakana ===")
type_ascii("echo ")
send_unicode_string("カタカナ")
press_enter()
print("Sent: echo カタカナ")
time.sleep(0.3)

print("=== Test 4: Mixed ASCII and CJK ===")
type_ascii("echo ")
send_unicode_string("日本語")
type_ascii("test")
send_unicode_string("テスト")
press_enter()
print("Sent: echo 日本語testテスト")
time.sleep(0.3)

print("=== Test 5: Backspace with CJK (simulates preedit erase) ===")
# Type some hiragana, then BS to erase, then type kanji (like IME conversion)
send_unicode_string("かんじ")  # preedit: かんじ
time.sleep(0.1)
# Erase 3 chars with BS
for _ in range(3):
    xtest.fake_input(d, X.KeyPress, bs_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, bs_kc)
    d.sync()
    time.sleep(0.02)
# Type converted kanji
send_unicode_string("漢字")
press_enter()
print("Sent: かんじ + 3xBS + 漢字 (should show '漢字')")
time.sleep(0.3)

print("\n=== All kanji tests completed ===")
print("Expected xterm output:")
print("  漢字")
print("  こんにちは")
print("  カタカナ")
print("  日本語testテスト")
print("  漢字")

d.close()
