#!/usr/bin/env python3
"""Simple IME test: type 2 kana chars, convert, commit."""
import subprocess, time
from Quartz import CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap

def press_key(keycode):
    e = CGEventCreateKeyboardEvent(None, keycode, True)
    CGEventPost(kCGHIDEventTap, e)
    time.sleep(0.05)
    e = CGEventCreateKeyboardEvent(None, keycode, False)
    CGEventPost(kCGHIDEventTap, e)
    time.sleep(0.1)

subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(1.5)

# Ensure English
press_key(102)  # 英数
time.sleep(0.5)

# Switch to Japanese kana
press_key(104)  # かな
time.sleep(0.8)

# Type 2 keys: 'a'(kc=0) → ち, 'i'(kc=34) → に
print("Typing 'a' (→ち)...")
press_key(0)
time.sleep(0.5)

print("Typing 'i' (→に)...")
press_key(34)
time.sleep(0.5)

print("Space to convert...")
press_key(49)
time.sleep(1.0)

print("Enter to commit...")
press_key(36)
time.sleep(0.5)

# Switch back to English
press_key(102)
time.sleep(0.3)

print("Enter to execute...")
press_key(36)
time.sleep(0.5)

subprocess.run(["screencapture", "-x", "/tmp/pslx_simple_ime.png"], capture_output=True)
print("Done. Check /tmp/pslx_preedit_debug.log")
