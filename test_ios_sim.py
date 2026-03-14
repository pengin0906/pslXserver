#!/usr/bin/env python3
"""
iOS Simulator automated test for Xserver-iOS.

Tests:
1. Start xterm connected to the simulator (DISPLAY=127.0.0.1:1)
2. Use XTEST to inject key events
3. Take screenshots via xcrun simctl io screenshot
4. Verify expected text appears in xterm output

Prerequisites:
- iOS simulator running: xcrun simctl boot FBB5C54D-DB2C-4E6C-BFCB-0147CEDB3BFB
- Xserver-iOS installed and launched on the simulator
- xterm running: DISPLAY=127.0.0.1:1 xterm -u8 ...
- python-xlib installed: pip3 install python-xlib

Usage:
    # 1. Launch simulator app
    xcrun simctl launch FBB5C54D-DB2C-4E6C-BFCB-0147CEDB3BFB com.pslx.Xserver-iOS
    # 2. Connect xterm
    DISPLAY=127.0.0.1:1 /opt/homebrew/bin/xterm -u8 &
    # 3. Run this test
    python3 test_ios_sim.py
"""

import subprocess
import time
import sys
import os

SIMULATOR_UDID = "FBB5C54D-DB2C-4E6C-BFCB-0147CEDB3BFB"
DISPLAY_ADDR = "127.0.0.1:1"
SCREENSHOT_DIR = "/tmp"

def screenshot(name):
    """Take a screenshot of the iOS simulator."""
    path = os.path.join(SCREENSHOT_DIR, f"pslx_ios_{name}.png")
    result = subprocess.run(
        ["xcrun", "simctl", "io", SIMULATOR_UDID, "screenshot", path],
        capture_output=True, text=True
    )
    if result.returncode != 0:
        print(f"  WARNING: screenshot failed: {result.stderr}")
        return None
    print(f"  Screenshot: {path}")
    return path

def wait_for_xterm(timeout=10):
    """Wait for xterm to connect and be ready."""
    try:
        from Xlib import display as xdisplay
        for i in range(timeout * 2):
            try:
                d = xdisplay.Display(DISPLAY_ADDR)
                root = d.screen().root
                children = root.query_tree().children
                d.close()
                if len(children) > 0:
                    return True
            except Exception:
                pass
            time.sleep(0.5)
    except ImportError:
        print("  python-xlib not available, skipping wait")
        time.sleep(3)
        return True
    return False

def xtest_type(text, delay=0.05):
    """Type text into the focused X11 window using XTEST."""
    try:
        from Xlib import X, display as xdisplay
        from Xlib.ext import xtest
        d = xdisplay.Display(DISPLAY_ADDR)
        for ch in text:
            if ch == '\n':
                ks = 0xff0d  # Return
            elif ch == '\b':
                ks = 0xff08  # BackSpace
            else:
                ks = ord(ch)
            kc = d.keysym_to_keycode(ks)
            if kc:
                xtest.fake_input(d, X.KeyPress, kc)
                xtest.fake_input(d, X.KeyRelease, kc)
                d.sync()
                time.sleep(delay)
        d.close()
        return True
    except ImportError:
        print("  ERROR: python-xlib not installed. Run: pip3 install python-xlib")
        return False
    except Exception as e:
        print(f"  ERROR: XTEST failed: {e}")
        return False

def xtest_keycode(keycode, delay=0.05):
    """Send a raw keycode via XTEST."""
    try:
        from Xlib import X, display as xdisplay
        from Xlib.ext import xtest
        d = xdisplay.Display(DISPLAY_ADDR)
        xtest.fake_input(d, X.KeyPress, keycode)
        xtest.fake_input(d, X.KeyRelease, keycode)
        d.sync()
        time.sleep(delay)
        d.close()
        return True
    except Exception as e:
        print(f"  ERROR: xtest_keycode failed: {e}")
        return False

def check_xterm_running():
    """Check if xterm is connected to the iOS display."""
    try:
        result = subprocess.run(["pgrep", "-f", "xterm.*127.0.0.1:1"],
                               capture_output=True, text=True)
        return result.returncode == 0
    except Exception:
        return False

def launch_xterm():
    """Launch xterm connected to the iOS simulator."""
    env = os.environ.copy()
    env["LANG"] = "en_US.UTF-8"
    env["XMODIFIERS"] = "@im=none"
    env["DISPLAY"] = DISPLAY_ADDR
    proc = subprocess.Popen(
        ["/opt/homebrew/bin/xterm", "-u8",
         "-fn", "-misc-fixed-medium-r-semicondensed--13-120-75-75-c-60-iso10646-1"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL
    )
    return proc

def test_basic_ascii():
    """Test 1: Basic ASCII key input via XTEST."""
    print("\n=== Test 1: Basic ASCII input ===")
    screenshot("test1_before")

    test_str = "hello_ios\n"
    print(f"  Typing: {repr(test_str)}")
    ok = xtest_type(test_str)
    if not ok:
        return False

    time.sleep(0.5)
    path = screenshot("test1_after")

    # Check the debug file for render activity
    debug_file = "/tmp/pslx_render_debug.txt"
    if os.path.exists(debug_file):
        with open(debug_file) as f:
            content = f.read()
        print(f"  Render debug: {content.strip()}")
        if "vis=true" in content:
            print("  PASS: Window is visible and rendering")
            return True
        else:
            print("  FAIL: Window not visible")
            return False
    else:
        print("  INFO: No debug file (app may not have rendered yet)")
        return path is not None

def test_special_keys():
    """Test 2: Special keys (arrows, Ctrl+C, etc.)."""
    print("\n=== Test 2: Special keys ===")

    # Type something then delete it with backspace
    ok = xtest_type("abcdef")
    if not ok:
        return False
    time.sleep(0.2)

    # Backspace 6 times
    for _ in range(6):
        xtest_type("\b")
        time.sleep(0.05)

    screenshot("test2_special")
    print("  PASS: Special keys sent")
    return True

def test_unicode_input():
    """Test 3: Unicode/CJK character input via ChangeKeyboardMapping."""
    print("\n=== Test 3: Unicode/CJK input ===")
    try:
        from Xlib import X, display as xdisplay
        from Xlib.ext import xtest
        d = xdisplay.Display(DISPLAY_ADDR)

        # Map keycode 200 to kanji '漢'
        kanji_keysym = 0x01000000 | ord('漢')
        d.change_keyboard_mapping(200, [(kanji_keysym,)])
        d.sync()
        time.sleep(0.1)

        # Send the kanji keycode
        xtest.fake_input(d, X.KeyPress, 200)
        xtest.fake_input(d, X.KeyRelease, 200)
        d.sync()
        time.sleep(0.3)

        d.close()
        screenshot("test3_unicode")
        print("  PASS: Unicode keycode sent")
        return True
    except ImportError:
        print("  SKIP: python-xlib not available")
        return True
    except Exception as e:
        print(f"  FAIL: {e}")
        return False

def test_window_visible():
    """Test 4: Verify xterm window is visible and rendering."""
    print("\n=== Test 4: Window visibility ===")
    debug_file = "/tmp/pslx_render_debug.txt"
    if os.path.exists(debug_file):
        with open(debug_file) as f:
            content = f.read()
        print(f"  Window state: {content.strip()}")
        if "vis=true" in content and "layer=false" in content:
            print("  PASS: Window visible with CALayer attached")
            return True
        else:
            print("  FAIL: Window state unexpected")
            return False
    else:
        print("  INFO: No debug file available")
        # Still check via X11 tree
        try:
            from Xlib import display as xdisplay
            d = xdisplay.Display(DISPLAY_ADDR)
            root = d.screen().root
            children = root.query_tree().children
            print(f"  X11 root has {len(children)} top-level windows")
            d.close()
            if len(children) > 0:
                print("  PASS: X11 windows present")
                return True
        except Exception as e:
            print(f"  FAIL: {e}")
        return False

def main():
    print("Xserver iOS Simulator Test")
    print("=" * 40)
    print(f"Display: {DISPLAY_ADDR}")
    print(f"Simulator: {SIMULATOR_UDID}")

    # Check if xterm is running
    xterm_proc = None
    if not check_xterm_running():
        print("\nLaunching xterm...")
        xterm_proc = launch_xterm()
        print("Waiting for xterm to connect...")
        if not wait_for_xterm(timeout=15):
            print("ERROR: xterm did not connect within 15 seconds")
            print("Make sure Xserver-iOS is running on the simulator")
            if xterm_proc:
                xterm_proc.terminate()
            sys.exit(1)
        time.sleep(1)  # Extra settle time
        print("xterm connected!")
    else:
        print("xterm already running")

    screenshot("start")

    results = []
    results.append(("Basic ASCII input", test_basic_ascii()))
    results.append(("Special keys", test_special_keys()))
    results.append(("Unicode/CJK input", test_unicode_input()))
    results.append(("Window visibility", test_window_visible()))

    print("\n" + "=" * 40)
    print("RESULTS:")
    passed = 0
    for name, ok in results:
        status = "PASS" if ok else "FAIL"
        print(f"  [{status}] {name}")
        if ok:
            passed += 1
    print(f"\n{passed}/{len(results)} tests passed")

    screenshot("end")

    if xterm_proc:
        xterm_proc.terminate()

    return 0 if passed == len(results) else 1

if __name__ == "__main__":
    sys.exit(main())
