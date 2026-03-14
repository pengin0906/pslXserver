#!/usr/bin/env python3
"""Real macOS IME test with slower timing and screenshot at each step."""
import subprocess, time, sys
from Quartz import (
    CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap,
    CGEventSetFlags,
)

def send_key(keycode, down=True):
    event = CGEventCreateKeyboardEvent(None, keycode, down)
    CGEventPost(kCGHIDEventTap, event)
    time.sleep(0.02)

def press_key(keycode):
    send_key(keycode, True)
    time.sleep(0.05)
    send_key(keycode, False)
    time.sleep(0.08)

KEYCODES = {
    'a': 0, 'b': 11, 'c': 8, 'd': 2, 'e': 14, 'f': 3, 'g': 5,
    'h': 4, 'i': 34, 'j': 38, 'k': 40, 'l': 37, 'm': 46, 'n': 45,
    'o': 31, 'p': 35, 'q': 12, 'r': 15, 's': 1, 't': 17, 'u': 32,
    'v': 9, 'w': 13, 'x': 7, 'y': 16, 'z': 6,
    ' ': 49, '\n': 36,
}
KANA_KEY = 104   # かな
EISU_KEY = 102   # 英数
RETURN_KEY = 36  # Enter
SPACE_KEY = 49   # Space

def type_romaji(text):
    for ch in text:
        kc = KEYCODES.get(ch.lower())
        if kc is not None:
            press_key(kc)
            time.sleep(0.08)  # slower for IME to process

def screenshot(name):
    subprocess.run(["screencapture", "-x", f"/tmp/pslx_{name}.png"], capture_output=True)
    print(f"  Screenshot: /tmp/pslx_{name}.png")

# Activate Xserver
print("Activating Xserver...")
subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(1.5)

# First switch to English to ensure clean state
print("Switching to 英数 mode...")
press_key(EISU_KEY)
time.sleep(0.5)

# Type "echo " in ASCII mode first
print("Typing 'echo ' in ASCII mode...")
for ch in "echo ":
    kc = KEYCODES.get(ch)
    if kc: press_key(kc)
    time.sleep(0.05)
time.sleep(0.3)
screenshot("step0_echo")

# Switch to Japanese IME
print("\nSwitching to かな mode...")
press_key(KANA_KEY)
time.sleep(0.8)

# Test 1: Type "nihon" → にほん
print("\n=== Test: typing 'nihon' ===")
print("Typing n...")
press_key(KEYCODES['n']); time.sleep(0.15)
print("Typing i...")
press_key(KEYCODES['i']); time.sleep(0.15)
print("Typing h...")
press_key(KEYCODES['h']); time.sleep(0.15)
print("Typing o...")
press_key(KEYCODES['o']); time.sleep(0.15)
print("Typing n...")
press_key(KEYCODES['n']); time.sleep(0.15)

time.sleep(0.5)
screenshot("step1_preedit")
print("  Expected preedit: にほん")

# Press Space to convert
print("\nPressing Space to convert...")
press_key(SPACE_KEY)
time.sleep(1.0)
screenshot("step2_convert")
print("  Expected conversion: 日本")

# Press Enter to commit
print("\nPressing Enter to commit...")
press_key(RETURN_KEY)
time.sleep(0.5)
screenshot("step3_commit")

# Switch back to English and press Enter to execute
print("\nSwitching to 英数 and pressing Enter...")
press_key(EISU_KEY)
time.sleep(0.3)
press_key(RETURN_KEY)
time.sleep(0.5)
screenshot("step4_final")

print("\n=== Test completed ===")
print("Check screenshots for each step.")
