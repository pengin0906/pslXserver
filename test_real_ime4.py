#!/usr/bin/env python3
"""Test with kana input mode — the user's system uses かな入力 (Kotoeri.KanaTyping)."""
import subprocess, time
from Quartz import CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap

def send_key(keycode, down=True):
    event = CGEventCreateKeyboardEvent(None, keycode, down)
    CGEventPost(kCGHIDEventTap, event)
    time.sleep(0.02)

def press_key(keycode):
    send_key(keycode, True)
    time.sleep(0.05)
    send_key(keycode, False)
    time.sleep(0.1)

KANA_KEY = 104
EISU_KEY = 102
RETURN_KEY = 36
SPACE_KEY = 49
ESCAPE_KEY = 53
BS_KEY = 51

def screenshot(name):
    subprocess.run(["screencapture", "-x", f"/tmp/pslx_{name}.png"], capture_output=True)

# Activate Xserver
print("Activating Xserver...")
subprocess.run(["osascript", "-e", 'tell application "Xserver" to activate'], capture_output=True)
time.sleep(1.5)

# Switch to English first
press_key(EISU_KEY)
time.sleep(0.5)

# Switch to Japanese (kana mode)
print("Switching to かな input...")
press_key(KANA_KEY)
time.sleep(0.8)

# In kana input mode (JIS keyboard):
# 'a' key (keycode 0) = ち
# 'i' key (keycode 34) = に
# But actually for kana input, we need specific kana keys.
# Let me just type 3 characters slowly and check the preedit log.

# Type a single 'a' key → should produce ち (kana mode)
print("Test 1: Type single key 'a' (expect ち in kana mode)...")
press_key(0)  # 'a' key
time.sleep(1.0)
screenshot("kana_single")

# Escape to cancel
press_key(ESCAPE_KEY)
time.sleep(0.3)
press_key(BS_KEY)
time.sleep(0.3)

# Type 2 keys: 'a', 'i' → should produce ち, に
print("Test 2: Type 'a','i' slowly (expect ちに accumulating or replacing)...")
press_key(0)  # 'a' → ち
time.sleep(0.8)
screenshot("kana_2a")

press_key(34)  # 'i' → に
time.sleep(0.8)
screenshot("kana_2b")

# Escape and clean
press_key(ESCAPE_KEY)
time.sleep(0.3)
for _ in range(5):
    press_key(BS_KEY)
    time.sleep(0.05)
time.sleep(0.3)

# Type 3 keys slowly with screenshots between each
print("Test 3: Type 'k','a','k','i' (expect かき)...")
press_key(40)  # 'k' → か partial
time.sleep(0.5)
screenshot("kana_3a")
press_key(0)   # 'a' → か
time.sleep(0.5)
screenshot("kana_3b")
press_key(40)  # 'k' → き partial
time.sleep(0.5)
screenshot("kana_3c")
press_key(34)  # 'i' → き
time.sleep(0.5)
screenshot("kana_3d")

# Space to convert
print("Space to convert...")
press_key(SPACE_KEY)
time.sleep(1.0)
screenshot("kana_convert")

# Enter to commit
press_key(RETURN_KEY)
time.sleep(0.5)

# Enter again to execute
press_key(RETURN_KEY)
time.sleep(0.5)
screenshot("kana_final")

# Switch back to English
press_key(EISU_KEY)

print("\nDone. Check /tmp/pslx_preedit_debug.log for preedit state.")
