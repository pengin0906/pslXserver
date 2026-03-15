#!/usr/bin/env python3
"""Test inline IME preedit + commit flow.

Simulates the full IME pipeline:
1. User types 'k','a','n','j','i' → preedit shows かんじ (incrementally)
2. User presses Space → preedit changes to 漢字 (conversion)
3. User presses Enter → committed text 漢字 replaces preedit

Since we can't directly inject DisplayEvents, we simulate the same effect
via XTEST by doing what the server's IME handler does:
- Preedit: type chars, track what's displayed
- Conversion: BS to erase preedit, type converted text
- Commit: BS to erase conversion, type final text

This matches the full erase+resend preedit model.
"""
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

enter_kc = keysym_to_kc.get(0xFF0D)
bs_kc = keysym_to_kc.get(0xFF08)

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

def send_backspaces(n):
    for _ in range(n):
        xtest.fake_input(d, X.KeyPress, bs_kc)
        d.sync()
        xtest.fake_input(d, X.KeyRelease, bs_kc)
        d.sync()
        time.sleep(0.02)

def send_unicode_string(text):
    for i, ch in enumerate(text):
        send_unicode_char(ord(ch), 200 + (i % 50))

def press_enter():
    xtest.fake_input(d, X.KeyPress, enter_kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, enter_kc)
    d.sync()
    time.sleep(0.05)

def type_ascii(s):
    for ch in s:
        kc = keysym_to_kc.get(ord(ch))
        if kc:
            xtest.fake_input(d, X.KeyPress, kc)
            d.sync()
            xtest.fake_input(d, X.KeyRelease, kc)
            d.sync()
            time.sleep(0.02)

print("=== Inline IME Simulation ===")
print()

# --- Simulation 1: かんじ → 漢字 ---
print("--- Sim 1: かんじ → 漢字 ---")

# Step 1: Preedit "か" (user typed 'ka')
print("  Preedit: か")
send_unicode_string("か")
time.sleep(0.3)

# Step 2: Preedit "かん" (user typed 'n')
# Full erase + resend (our new model)
print("  Preedit: かん")
send_backspaces(1)  # erase か
send_unicode_string("かん")
time.sleep(0.3)

# Step 3: Preedit "かんじ" (user typed 'ji')
print("  Preedit: かんじ")
send_backspaces(2)  # erase かん
send_unicode_string("かんじ")
time.sleep(0.3)

# Step 4: Convert → 漢字 (user pressed Space)
print("  Convert: 漢字")
send_backspaces(3)  # erase かんじ
send_unicode_string("漢字")
time.sleep(0.3)

# Step 5: Commit (user pressed Enter) — preedit becomes committed text
# In full erase model: erase 漢字, resend 漢字 as committed
print("  Commit: 漢字")
send_backspaces(2)  # erase 漢字
send_unicode_string("漢字")
press_enter()
time.sleep(0.3)

# --- Simulation 2: にほんご → 日本語 ---
print("--- Sim 2: にほんご → 日本語 ---")

preedit_stages = ["に", "にほ", "にほん", "にほんご"]
for stage in preedit_stages:
    prev_len = len(preedit_stages[preedit_stages.index(stage) - 1]) if preedit_stages.index(stage) > 0 else 0
    if prev_len > 0:
        send_backspaces(prev_len)
    send_unicode_string(stage)
    print(f"  Preedit: {stage}")
    time.sleep(0.2)

# Convert
send_backspaces(4)
send_unicode_string("日本語")
print("  Convert: 日本語")
time.sleep(0.2)

# Commit
send_backspaces(3)
send_unicode_string("日本語")
press_enter()
print("  Commit: 日本語")
time.sleep(0.3)

# --- Simulation 3: Multiple words ---
print("--- Sim 3: わたし は にほんじん です ---")

words = [
    ("わたし", "私"),
    ("は", "は"),
    ("にほんじん", "日本人"),
    ("です", "です"),
]

for hiragana, kanji in words:
    # Preedit
    send_unicode_string(hiragana)
    print(f"  Preedit: {hiragana}")
    time.sleep(0.2)
    # Convert
    send_backspaces(len(hiragana))
    send_unicode_string(kanji)
    print(f"  Convert: {kanji}")
    time.sleep(0.2)
    # Commit
    send_backspaces(len(kanji))
    send_unicode_string(kanji)
    print(f"  Commit: {kanji}")
    time.sleep(0.1)

press_enter()
time.sleep(0.3)

print()
print("=== All inline IME simulations completed ===")
print("Expected xterm output:")
print("  漢字")
print("  日本語")
print("  私は日本人です")

d.close()
