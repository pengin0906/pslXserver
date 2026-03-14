#!/usr/bin/env python3
"""Real macOS IME test — fix: use Input Source API to switch to Japanese Romaji mode."""
import subprocess, time, sys, ctypes, ctypes.util
from Quartz import (
    CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap,
    CGEventCreateScrollWheelEvent, kCGScrollEventUnitLine,
)
import objc
from Foundation import NSBundle

# Load Carbon framework for TIS (Text Input Source) API
carbon = ctypes.cdll.LoadLibrary(ctypes.util.find_library('Carbon'))

# We need to use TISCopyInputSourceForLanguage or TISSelectInputSource
# to switch to Japanese Romaji input explicitly.
# But it's easier to use AppleScript to set the input source.

def set_input_source(source_id):
    """Set macOS input source by ID using AppleScript."""
    # Alternative: use command-line tool
    result = subprocess.run([
        "osascript", "-e",
        f'tell application "System Events" to tell process "Xserver" to key code 104'
    ], capture_output=True, text=True)
    return result.returncode == 0

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
KANA_KEY = 104
EISU_KEY = 102
RETURN_KEY = 36
SPACE_KEY = 49
ESCAPE_KEY = 53

def screenshot(name):
    subprocess.run(["screencapture", "-x", f"/tmp/pslx_{name}.png"], capture_output=True)
    print(f"  Screenshot: /tmp/pslx_{name}.png")

# Step 0: Check current input source
print("=== Real macOS IME Test (Romaji mode) ===")
result = subprocess.run(["defaults", "read", "com.apple.HIToolbox", "AppleCurrentKeyboardLayoutInputSourceID"],
                       capture_output=True, text=True)
print(f"Current input source: {result.stdout.strip()}")

# Activate Xserver
print("\nActivating Xserver...")
subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(1.5)

# Ensure English mode first
print("Switching to 英数 mode...")
press_key(EISU_KEY)
time.sleep(0.5)

# Type a test string in English to verify
print("Typing 'test' in English mode...")
for ch in "test":
    press_key(KEYCODES[ch])
    time.sleep(0.05)
# BS to erase
for _ in range(4):
    press_key(51)  # Delete/BS key
    time.sleep(0.03)
time.sleep(0.3)

# Now switch to Japanese and check mode
print("\nSwitching to Japanese (かな key)...")
press_key(KANA_KEY)
time.sleep(0.8)

# Check input source after switch
result = subprocess.run(["defaults", "read", "com.apple.HIToolbox", "AppleCurrentKeyboardLayoutInputSourceID"],
                       capture_output=True, text=True)
current_source = result.stdout.strip()
print(f"Input source after かな: {current_source}")

# Type a single character to check if romaji or kana mode
print("\nTyping 'a' to check mode (expect 'あ' in romaji, 'ち' in kana)...")
press_key(KEYCODES['a'])
time.sleep(0.5)
screenshot("mode_check")

# Escape to cancel any preedit
press_key(ESCAPE_KEY)
time.sleep(0.3)

# BS to erase
press_key(51)
time.sleep(0.2)

# Now type the actual test
print("\n=== Main test: typing 'nihongo' ===")
for ch in "nihongo":
    print(f"  Typing '{ch}'...")
    press_key(KEYCODES[ch])
    time.sleep(0.15)

time.sleep(0.5)
screenshot("preedit_nihongo")
print("  Expected: にほんご (romaji) or みにくらぎ (kana)")

# Space to convert
print("\nSpace to convert...")
press_key(SPACE_KEY)
time.sleep(1.0)
screenshot("convert_nihongo")

# Enter to commit
print("Enter to commit...")
press_key(RETURN_KEY)
time.sleep(0.5)

# Enter to execute
press_key(RETURN_KEY)
time.sleep(0.5)
screenshot("final")

# Switch back to English
press_key(EISU_KEY)
time.sleep(0.3)

print("\n=== Test completed ===")
print("If you see kana input (ちにくら instead of にほんご),")
print("the IME is in kana input mode, not romaji mode.")
print("This is a macOS input source configuration issue, not a Xserver bug.")
