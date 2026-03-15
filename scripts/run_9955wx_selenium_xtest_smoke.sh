#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

REMOTE="${REMOTE:-pengin0906@9955wx}"
MAC_DISPLAY="${MAC_DISPLAY:-192.168.0.101:0}"

ssh "$REMOTE" "DISPLAY=$MAC_DISPLAY python3 -" <<'PY'
import os
import shutil
import sys
import tempfile
import time

from selenium import webdriver
from selenium.webdriver.chrome.options import Options
from selenium.webdriver.chrome.service import Service
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait

from Xlib import X, display as xdisplay
from Xlib.ext import xtest


DISPLAY_NAME = os.environ["DISPLAY"]
TEST_TEXT = "xtest-smoke"


def find_chrome_window(root):
    stack = [root]
    while stack:
        win = stack.pop()
        try:
            wm_class = win.get_wm_class()
            if wm_class and any("chrome" in part.lower() or "chromium" in part.lower() for part in wm_class):
                return win
            stack.extend(win.query_tree().children)
        except Exception:
            continue
    return None


def keysym_to_keycode(dpy, ch):
    return dpy.keysym_to_keycode(ord(ch))


def xtest_type(dpy, text):
    for ch in text:
        kc = keysym_to_keycode(dpy, ch)
        if not kc:
            raise RuntimeError(f"no keycode for {ch!r}")
        xtest.fake_input(dpy, X.KeyPress, kc)
        xtest.fake_input(dpy, X.KeyRelease, kc)
        dpy.sync()
        time.sleep(0.03)


tmpdir = tempfile.mkdtemp(prefix="xserver-smoke-")
driver = None

try:
    dpy = xdisplay.Display(DISPLAY_NAME)
    xtest_info = dpy.query_extension("XTEST")
    print(f"XTEST available: {xtest_info}")
    if not xtest_info.present:
        raise RuntimeError("XTEST extension not present")

    options = Options()
    options.binary_location = "/usr/bin/google-chrome-stable"
    options.add_argument("--no-sandbox")
    options.add_argument("--disable-dev-shm-usage")
    options.add_argument("--disable-gpu")
    options.add_argument("--no-first-run")
    options.add_argument("--ozone-platform=x11")
    options.add_argument(f"--user-data-dir={tmpdir}")
    options.add_argument("--window-size=1200,900")
    service = Service("/usr/bin/chromedriver")

    driver = webdriver.Chrome(service=service, options=options)
    driver.get("data:text/html,<html><head><title>pslx-smoke</title></head><body><input id=q autofocus></body></html>")

    field = WebDriverWait(driver, 10).until(lambda d: d.find_element(By.ID, "q"))
    field.click()
    time.sleep(1.0)

    chrome_win = find_chrome_window(dpy.screen().root)
    if chrome_win is None:
        raise RuntimeError("chrome window not found in X11 tree")

    chrome_win.set_input_focus(X.RevertToParent, X.CurrentTime)
    dpy.sync()
    time.sleep(0.2)

    xtest_type(dpy, TEST_TEXT)
    time.sleep(0.8)

    value = field.get_attribute("value")
    print(f"Input value: {value!r}")
    if value != TEST_TEXT:
        raise RuntimeError(f"unexpected input value: {value!r}")

    print("PASS: Selenium + XTEST smoke test succeeded")
finally:
    try:
        dpy.close()
    except Exception:
        pass
    if driver is not None:
        driver.quit()
    shutil.rmtree(tmpdir, ignore_errors=True)
PY
