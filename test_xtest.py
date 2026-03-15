#!/usr/bin/env python3
"""Test XTEST extension and keyboard mapping via raw X11 protocol (python-xlib)."""
import subprocess, sys, os

# Check if python-xlib is installed
try:
    from Xlib import X, display, ext
    from Xlib.ext import xtest
except ImportError:
    print("Installing python-xlib...")
    subprocess.check_call([sys.executable, "-m", "pip", "install", "--break-system-packages", "python-xlib"])
    from Xlib import X, display, ext
    from Xlib.ext import xtest

d = display.Display(os.environ.get("DISPLAY", ":0"))
root = d.screen().root

# Check XTEST extension
print(f"XTEST available: {d.query_extension('XTEST')}")

# Get keyboard mapping
first_keycode = d.display.info.min_keycode
last_keycode = d.display.info.max_keycode
print(f"Keycode range: {first_keycode}-{last_keycode}")

mapping = d.get_keyboard_mapping(first_keycode, last_keycode - first_keycode + 1)

# Print a few key mappings (keycodes for common letters)
# macOS keycode + 8 = X11 keycode
# a=0+8=8, s=1+8=9, d=2+8=10, f=3+8=11, h=4+8=12
# q=12+8=20, w=13+8=21, e=14+8=22
test_keycodes = {
    8: "a (macOS 0)",
    9: "s (macOS 1)",
    10: "d (macOS 2)",
    11: "f (macOS 3)",
    20: "q (macOS 12)",
    21: "w (macOS 13)",
    22: "e (macOS 14)",
    23: "r (macOS 15)",
    26: "1 (macOS 18)",
    27: "2 (macOS 19)",
    28: "3 (macOS 20)",
    46: "0 (macOS 38 = j?)",
    # Number keys on Mac: 1=18, 2=19, 3=20, 4=21, 5=23, 6=22, 7=26, 8=28, 9=25, 0=29
}

print("\n--- Keyboard Mapping (keycode -> keysyms) ---")
for kc in range(first_keycode, min(last_keycode+1, 136)):
    syms = mapping[kc - first_keycode]
    # Filter out zero keysyms
    non_zero = [(i, s) for i, s in enumerate(syms) if s != 0]
    if non_zero:
        sym_strs = []
        for idx, s in non_zero:
            # Decode keysym
            if 0x20 <= s <= 0x7e:
                sym_strs.append(f"'{chr(s)}'")
            elif s >= 0x01000000:
                cp = s & 0xFFFFFF
                try:
                    sym_strs.append(f"U+{cp:04X}({chr(cp)})")
                except:
                    sym_strs.append(f"U+{cp:04X}")
            else:
                sym_strs.append(f"0x{s:04X}")
        label = test_keycodes.get(kc, "")
        print(f"  keycode {kc:3d}: {', '.join(sym_strs)}  {label}")

# Now test FakeInput - send "hello" to the focused window
print("\n--- Testing XTEST FakeInput ---")
# Keycodes for 'hello' on US keyboard (macOS keycode + 8):
# h=4+8=12, e=14+8=22, l=37+8=45, o=31+8=39
hello_keycodes = [12, 22, 45, 45, 39]  # h, e, l, l, o
for kc in hello_keycodes:
    xtest.fake_input(d, X.KeyPress, kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, kc)
    d.sync()
    import time
    time.sleep(0.05)

print("Sent 'hello' via XTEST FakeInput")
print("\n--- Testing numbers 0-9 ---")
# macOS number key keycodes: 0=29, 1=18, 2=19, 3=20, 4=21, 5=23, 6=22, 7=26, 8=28, 9=25
# X11 = macOS + 8
number_keycodes = [37, 26, 27, 28, 29, 31, 30, 34, 36, 33]  # 0-9
for kc in number_keycodes:
    xtest.fake_input(d, X.KeyPress, kc)
    d.sync()
    xtest.fake_input(d, X.KeyRelease, kc)
    d.sync()
    import time
    time.sleep(0.05)

print("Sent '0123456789' via XTEST FakeInput")

d.close()
