#!/usr/bin/env python3
"""Debug: dump YouTube DOM to find correct selectors for failed buttons."""

import time
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.chrome.options import Options
from selenium.webdriver.chrome.service import Service
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC

def setup_driver():
    options = Options()
    options.binary_location = "/usr/bin/chromium-browser"
    options.add_argument("--no-sandbox")
    options.add_argument("--disable-dev-shm-usage")
    options.add_argument("--no-first-run")
    options.add_argument("--disable-gpu")
    options.add_argument("--ozone-platform=x11")
    options.add_argument("--start-maximized")
    options.add_argument("--lang=ja")
    options.add_argument("--display=192.168.0.101:0")
    service = Service("/usr/bin/chromedriver")
    driver = webdriver.Chrome(service=service, options=options)
    driver.implicitly_wait(3)
    return driver

driver = setup_driver()
driver.get("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
time.sleep(8)

# Dump all buttons on the page
print("=== ALL BUTTONS ===")
buttons = driver.find_elements(By.TAG_NAME, "button")
for i, btn in enumerate(buttons):
    try:
        aria = btn.get_attribute("aria-label") or ""
        cls = btn.get_attribute("class") or ""
        text = btn.text[:50] if btn.text else ""
        displayed = btn.is_displayed()
        tag = btn.tag_name
        parent_tag = btn.find_element(By.XPATH, "..").tag_name if btn else ""
        parent_id = btn.find_element(By.XPATH, "..").get_attribute("id") or ""
        if displayed and (aria or text):
            print(f"  [{i}] aria='{aria}' class='{cls[:60]}' text='{text}' parent={parent_tag}#{parent_id}")
    except:
        pass

# Check search button specifically
print("\n=== SEARCH BUTTON CANDIDATES ===")
for sel in ["#search-icon-legacy", "#search-icon", "button[aria-label='Search']",
            "button[aria-label='検索']", "#search-btn", ".ytSearchboxComponentSearchButton"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: displayed={e.is_displayed()}, tag={e.tag_name}, text='{e.text[:30]}'")

# Check video player controls
print("\n=== PLAYER CONTROLS ===")
for sel in [".ytp-chrome-bottom", ".ytp-chrome-controls"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        inner = e.get_attribute("innerHTML")[:500] if e else ""
        print(f"  {sel}: displayed={e.is_displayed()}, children count={len(e.find_elements(By.TAG_NAME, 'button'))}")

# Mute button
print("\n=== MUTE BUTTON ===")
for sel in [".ytp-mute-button", "button.ytp-mute-button",
            ".ytp-volume-area button", "[data-tooltip-target-id='ytp-autonav-toggle-button']"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: displayed={e.is_displayed()}, aria='{e.get_attribute('aria-label')}'")

# Like/dislike area
print("\n=== LIKE/DISLIKE AREA ===")
# Get the actions area
for sel in ["#actions", "#top-level-buttons-computed", "ytd-menu-renderer",
            "segmented-like-dislike-button-view-model", "#actions-inner",
            "like-button-view-model", "dislike-button-view-model"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    print(f"  {sel}: found {len(elems)}")
    for e in elems:
        print(f"    displayed={e.is_displayed()}, tag={e.tag_name}, text='{e.text[:80]}'")

# Settings button
print("\n=== SETTINGS BUTTON ===")
for sel in [".ytp-settings-button", "button.ytp-settings-button",
            ".ytp-right-controls button"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: displayed={e.is_displayed()}, aria='{e.get_attribute('aria-label')}'")

# Recommended videos
print("\n=== RECOMMENDED VIDEOS ===")
for sel in ["ytd-compact-video-renderer", "ytd-rich-item-renderer",
            "#related ytd-compact-video-renderer a#thumbnail",
            "#secondary ytd-compact-video-renderer",
            "ytd-watch-next-secondary-results-renderer"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    print(f"  {sel}: found {len(elems)}")

# Share button
print("\n=== SHARE BUTTON ===")
share_elems = driver.find_elements(By.XPATH,
    "//*[contains(@aria-label, 'Share') or contains(@aria-label, '共有') or contains(text(), 'Share') or contains(text(), '共有')]")
for e in share_elems:
    print(f"  tag={e.tag_name}, aria='{e.get_attribute('aria-label')}', text='{e.text[:50]}', displayed={e.is_displayed()}")

driver.quit()
print("\nDone.")
