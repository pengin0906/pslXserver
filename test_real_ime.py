#!/usr/bin/env python3
"""Test REAL macOS IME kanji conversion through Xserver.

Uses CGEvent to send keystrokes at the macOS level, which go through:
  macOS keyDown → interpretKeyEvents → setMarkedText (preedit) → insertText (commit)
  → DisplayEvent::ImePreeditDraw / ImeCommit → xterm

This is the real IME pipeline test.
"""
import subprocess, time, os, sys

def cgevent_keystroke(keycode, flags=0):
    """Send a CGEvent keystroke via osascript/Python."""
    # Use Python + pyobjc to send CGEvents
    pass

# Check if we can use osascript for keystrokes
# First, bring Xserver to front
print("=== Real macOS IME Test ===")
print("Step 1: Activate Xserver window...")

subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(0.5)

# Use cliclick if available, otherwise fall back to osascript
# Check for cliclick
has_cliclick = subprocess.run(["which", "cliclick"], capture_output=True).returncode == 0

print("Step 2: Switch to Japanese IME (press かな key)...")
# かな key = keycode 104 on JIS keyboard
# Use AppleScript to send the key
# Actually, we need CGEvent for specific keycodes. Let's use Python Quartz.
try:
    import Quartz
    from Quartz import (
        CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap,
        CGEventSetFlags, kCGEventFlagMaskShift,
        CGEventSetIntegerValueField,
    )
    HAS_QUARTZ = True
except ImportError:
    print("Installing pyobjc-framework-Quartz...")
    subprocess.check_call([sys.executable, "-m", "pip", "install", "--break-system-packages", "pyobjc-framework-Quartz"])
    import Quartz
    from Quartz import (
        CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap,
        CGEventSetFlags, kCGEventFlagMaskShift,
        CGEventSetIntegerValueField,
    )
    HAS_QUARTZ = True

def send_key(keycode, flags=0, key_down=True):
    """Send a single CGEvent key press or release."""
    event = CGEventCreateKeyboardEvent(None, keycode, key_down)
    if flags:
        CGEventSetFlags(event, flags)
    CGEventPost(kCGHIDEventTap, event)
    time.sleep(0.01)

def press_key(keycode, flags=0):
    """Press and release a key."""
    send_key(keycode, flags, True)
    time.sleep(0.03)
    send_key(keycode, flags, False)
    time.sleep(0.03)

def type_romaji(text):
    """Type ASCII text using CGEvent (goes through macOS IME)."""
    # macOS virtual keycodes for common letters
    keycode_map = {
        'a': 0, 'b': 11, 'c': 8, 'd': 2, 'e': 14, 'f': 3, 'g': 5,
        'h': 4, 'i': 34, 'j': 38, 'k': 40, 'l': 37, 'm': 46, 'n': 45,
        'o': 31, 'p': 35, 'q': 12, 'r': 15, 's': 1, 't': 17, 'u': 32,
        'v': 9, 'w': 13, 'x': 7, 'y': 16, 'z': 6,
        ' ': 49, '\n': 36,
        '1': 18, '2': 19, '3': 20, '4': 21, '5': 23,
        '6': 22, '7': 26, '8': 28, '9': 25, '0': 29,
    }
    for ch in text:
        kc = keycode_map.get(ch.lower())
        if kc is not None:
            press_key(kc)
            time.sleep(0.05)

# Activate Xserver window
subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(1.0)

# Step 2: Switch to Japanese IME
# かな key = virtual keycode 104
print("Pressing かな key (keycode 104) to switch to Japanese IME...")
press_key(104)  # かな key
time.sleep(0.5)

# Step 3: Type romaji → should appear as hiragana preedit
print("Step 3: Typing 'kanji' (should show かんじ as preedit)...")
type_romaji("kanji")
time.sleep(1.0)

# Take screenshot of preedit state
print("  Taking screenshot of preedit state...")
subprocess.run(["screencapture", "-x", "/tmp/pslx_ime_preedit.png"], capture_output=True)

# Step 4: Press Space to convert → should show 漢字
print("Step 4: Pressing Space to convert (should show 漢字)...")
press_key(49)  # Space
time.sleep(1.0)

# Take screenshot of conversion state
print("  Taking screenshot of conversion state...")
subprocess.run(["screencapture", "-x", "/tmp/pslx_ime_convert.png"], capture_output=True)

# Step 5: Press Enter to commit
print("Step 5: Pressing Enter to commit...")
press_key(36)  # Return/Enter
time.sleep(0.5)

# Step 6: Press Enter again to execute command in shell
print("Step 6: Pressing Enter to execute in shell...")
press_key(36)
time.sleep(0.5)

# Take screenshot of final state
print("  Taking screenshot of committed state...")
subprocess.run(["screencapture", "-x", "/tmp/pslx_ime_commit.png"], capture_output=True)

# Step 7: Switch back to English
print("Step 7: Pressing 英数 key (keycode 102) to switch back to English...")
press_key(102)  # 英数 key
time.sleep(0.3)

# Step 8: Another test - type "nihongo" → 日本語
print("\nStep 8: Second test - typing 'nihongo'...")
press_key(104)  # かな
time.sleep(0.3)
type_romaji("nihongo")
time.sleep(0.5)
press_key(49)  # Space to convert
time.sleep(0.5)
press_key(36)  # Enter to commit
time.sleep(0.3)
press_key(36)  # Enter to execute
time.sleep(0.5)

subprocess.run(["screencapture", "-x", "/tmp/pslx_ime_final.png"], capture_output=True)

print("\n=== Real IME test completed ===")
print("Screenshots saved:")
print("  /tmp/pslx_ime_preedit.png  — hiragana preedit (かんじ)")
print("  /tmp/pslx_ime_convert.png  — kanji conversion (漢字)")
print("  /tmp/pslx_ime_commit.png   — committed text")
print("  /tmp/pslx_ime_final.png    — final state")

# Switch back to English
press_key(102)
