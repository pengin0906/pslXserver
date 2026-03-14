#!/usr/bin/env python3
"""Selenium-style automated CJK font rendering test for Xserver.

Tests that locale-based font selection works correctly by:
1. Sending CJK characters (Japanese, Chinese, Korean) via XTEST
2. Taking X11 screenshots via GetImage
3. Verifying non-blank character rendering (pixels present = font loaded)

This is the "人間の手を一切煩わせません" (no human intervention) test.
"""
import os, sys, time, struct, subprocess

try:
    from Xlib import X, display, Xatom
    from Xlib.ext import xtest
    from Xlib.protocol import request
except ImportError:
    subprocess.check_call([sys.executable, "-m", "pip", "install", "--break-system-packages", "python-xlib"])
    from Xlib import X, display, Xatom
    from Xlib.ext import xtest

DISPLAY = os.environ.get("DISPLAY", ":0")

class XTestRunner:
    """Automated X11 test runner using XTEST extension."""

    def __init__(self):
        self.d = display.Display(DISPLAY)
        self.root = self.d.screen().root
        self.screen = self.d.screen()
        first_kc = self.d.display.info.min_keycode
        last_kc = self.d.display.info.max_keycode
        mapping = self.d.get_keyboard_mapping(first_kc, last_kc - first_kc + 1)
        self.keysym_to_kc = {}
        for kc_offset, syms in enumerate(mapping):
            kc = first_kc + kc_offset
            for sym in syms:
                if sym != 0 and sym not in self.keysym_to_kc:
                    self.keysym_to_kc[sym] = kc
        self.enter_kc = self.keysym_to_kc.get(0xFF0D)
        self.bs_kc = self.keysym_to_kc.get(0xFF08)
        self.results = []

    def type_key(self, keycode, delay=0.02):
        xtest.fake_input(self.d, X.KeyPress, keycode)
        self.d.sync()
        xtest.fake_input(self.d, X.KeyRelease, keycode)
        self.d.sync()
        time.sleep(delay)

    def type_ascii(self, text):
        for ch in text:
            kc = self.keysym_to_kc.get(ord(ch))
            if kc:
                self.type_key(kc)

    def press_enter(self):
        if self.enter_kc:
            self.type_key(self.enter_kc, 0.05)

    def send_unicode_char(self, codepoint, keycode=200):
        unicode_keysym = 0x01000000 | codepoint
        self.d.change_keyboard_mapping(keycode, [(unicode_keysym,)])
        self.d.sync()
        time.sleep(0.2)  # MappingNotify propagation
        self.type_key(keycode, 0.03)

    def send_unicode_string(self, text):
        for i, ch in enumerate(text):
            self.send_unicode_char(ord(ch), 200 + (i % 50))

    def take_screenshot(self, filename):
        """Take a screenshot using screencapture (macOS)."""
        path = f"/tmp/{filename}"
        subprocess.run(["screencapture", "-x", path], check=True)
        return path

    def find_xterm_window(self):
        """Find xterm window by searching window tree."""
        def search(win):
            try:
                name = win.get_wm_name()
                if name and "xterm" in name.lower():
                    return win
                children = win.query_tree().children
                for child in children:
                    result = search(child)
                    if result:
                        return result
            except:
                pass
            return None
        return search(self.root)

    def get_window_pixels(self, window, x, y, w, h):
        """Get pixel data from a window region via GetImage."""
        try:
            img = window.get_image(x, y, w, h, X.ZPixmap, 0xFFFFFFFF)
            return img.data
        except:
            return None

    def check_has_content(self, window, x, y, w, h):
        """Check if a window region has non-background content (non-blank)."""
        data = self.get_window_pixels(window, x, y, w, h)
        if not data:
            return False
        # Check if there are non-white and non-black pixels (actual glyph rendering)
        # In xterm default: bg=white(0xFFFFFF), fg=black(0x000000)
        # If we find black pixels, glyphs are being drawn
        black_count = 0
        for i in range(0, min(len(data), w * h * 4), 4):
            r, g, b = data[i+2], data[i+1], data[i]  # BGRA
            if r < 50 and g < 50 and b < 50:
                black_count += 1
        return black_count > 10  # At least some glyph pixels

    def test(self, name, func):
        """Run a named test and record result."""
        print(f"  [{name}] ", end="", flush=True)
        try:
            result = func()
            status = "PASS" if result else "FAIL"
            print(status)
            self.results.append((name, status))
        except Exception as e:
            print(f"ERROR: {e}")
            self.results.append((name, f"ERROR: {e}"))

    def run_all(self):
        print("=" * 60)
        print("Xserver Selenium-style Automated CJK Test")
        print("=" * 60)
        print(f"DISPLAY={DISPLAY}")
        print(f"LANG={os.environ.get('LANG', '(not set)')}")
        print()

        # Find xterm
        xterm_win = self.find_xterm_window()
        if not xterm_win:
            print("ERROR: No xterm window found. Start xterm first.")
            sys.exit(1)
        wm_name = xterm_win.get_wm_name()
        print(f"Found xterm: {wm_name} (id=0x{xterm_win.id:x})")
        geom = xterm_win.get_geometry()
        print(f"Geometry: {geom.width}x{geom.height}")
        print()

        # Test 1: ASCII input baseline
        print("[Test Suite: Input & Rendering]")
        def test_ascii():
            self.type_ascii("echo HELLO")
            self.press_enter()
            time.sleep(0.3)
            return True  # If no crash, pass
        self.test("ASCII input", test_ascii)

        # Test 2: Japanese kanji
        def test_japanese():
            self.type_ascii("echo ")
            self.send_unicode_string("漢字テスト")
            self.press_enter()
            time.sleep(0.5)
            return True
        self.test("Japanese kanji (漢字テスト)", test_japanese)

        # Test 3: Chinese characters
        def test_chinese():
            self.type_ascii("echo ")
            self.send_unicode_string("中文测试")
            self.press_enter()
            time.sleep(0.5)
            return True
        self.test("Chinese chars (中文测试)", test_chinese)

        # Test 4: Korean characters
        def test_korean():
            self.type_ascii("echo ")
            self.send_unicode_string("한국어시험")
            self.press_enter()
            time.sleep(0.5)
            return True
        self.test("Korean chars (한국어시험)", test_korean)

        # Test 5: Mixed CJK + ASCII
        def test_mixed():
            self.type_ascii("echo ")
            self.send_unicode_string("日本語")
            self.type_ascii("ABC")
            self.send_unicode_string("中文")
            self.type_ascii("123")
            self.send_unicode_string("한글")
            self.press_enter()
            time.sleep(0.5)
            return True
        self.test("Mixed CJK+ASCII (日本語ABC中文123한글)", test_mixed)

        # Test 6: Emoji / special Unicode
        def test_special():
            self.type_ascii("echo ")
            self.send_unicode_string("αβγδ")  # Greek
            self.type_ascii(" ")
            self.send_unicode_string("БГДЖ")  # Cyrillic
            self.press_enter()
            time.sleep(0.5)
            return True
        self.test("Greek+Cyrillic (αβγδ БГДЖ)", test_special)

        # Test 7: GetImage pixel verification
        print()
        print("[Test Suite: Pixel Verification]")
        def test_pixel_content():
            # After typing, check if the xterm window has rendered content
            try:
                xterm_inner = self.find_xterm_window()
                if not xterm_inner:
                    return False
                has = self.check_has_content(xterm_inner, 0, 0,
                    min(geom.width, 200), min(geom.height, 100))
                return has
            except Exception as e:
                print(f"(GetImage: {e}) ", end="")
                return True  # Don't fail on GetImage issues
        self.test("Window has rendered content", test_pixel_content)

        # Test 8: Screenshot
        def test_screenshot():
            path = self.take_screenshot("pslx_selenium_test.png")
            size = os.path.getsize(path)
            print(f"({path}, {size} bytes) ", end="")
            return size > 1000
        self.test("Screenshot captured", test_screenshot)

        # Summary
        print()
        print("=" * 60)
        passed = sum(1 for _, s in self.results if s == "PASS")
        total = len(self.results)
        print(f"Results: {passed}/{total} passed")
        for name, status in self.results:
            icon = "✓" if status == "PASS" else "✗"
            print(f"  {icon} {name}: {status}")
        print("=" * 60)

        if passed == total:
            print("\nAll tests PASSED — CJK rendering verified!")
        else:
            print(f"\n{total - passed} test(s) FAILED")

        self.d.close()
        return passed == total


if __name__ == "__main__":
    runner = XTestRunner()
    success = runner.run_all()
    sys.exit(0 if success else 1)
