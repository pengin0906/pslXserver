#!/usr/bin/env python3
"""Debug remaining 4 failed tests."""
import time
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.action_chains import ActionChains
from selenium.webdriver.chrome.options import Options
from selenium.webdriver.chrome.service import Service

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
time.sleep(15)

# Hover to reveal controls
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC
player = WebDriverWait(driver, 15).until(
    EC.presence_of_element_located((By.CSS_SELECTOR, ".html5-video-player"))
)
ActionChains(driver).move_to_element(player).perform()
time.sleep(2)

# Debug subtitles button
print("=== SUBTITLES BUTTON ===")
for sel in [".ytp-subtitles-button", "button.ytp-subtitles-button",
            "[aria-label*='字幕']"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: tag={e.tag_name}, displayed={e.is_displayed()}, "
              f"aria='{e.get_attribute('aria-label')}', enabled={e.is_enabled()}, "
              f"size={e.size}")

# Debug theater mode
print("\n=== THEATER MODE BUTTON ===")
for sel in [".ytp-size-button", "button.ytp-size-button",
            "[aria-label*='シアター']", "[aria-label*='theater']"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: tag={e.tag_name}, displayed={e.is_displayed()}, "
              f"aria='{e.get_attribute('aria-label')}', size={e.size}")

# Debug large play button
print("\n=== LARGE PLAY BUTTON ===")
# First pause the video
try:
    pp = driver.find_element(By.CSS_SELECTOR, "button.ytp-play-button")
    pp.click()
    time.sleep(1)
except:
    pass
for sel in [".ytp-large-play-button", "button.ytp-large-play-button",
            ".ytp-cued-thumbnail-overlay"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    for e in elems:
        print(f"  {sel}: tag={e.tag_name}, displayed={e.is_displayed()}, "
              f"aria='{e.get_attribute('aria-label')}', size={e.size}")

# Debug recommended videos
print("\n=== RECOMMENDED VIDEOS ===")
for sel in [
    "#secondary a[href*='watch']",
    "ytd-watch-next-secondary-results-renderer a",
    "ytd-compact-video-renderer",
    "ytd-reel-shelf-renderer",
    "#related a",
    "#items ytd-compact-video-renderer",
    "ytd-item-section-renderer a#thumbnail",
]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = sum(1 for e in elems if e.is_displayed())
    print(f"  {sel}: found {len(elems)}, visible={visible}")
    if elems and visible > 0:
        e = next(e for e in elems if e.is_displayed())
        print(f"    first visible: tag={e.tag_name}, href='{(e.get_attribute('href') or '')[:60]}'")

# Check what's actually in the secondary panel
print("\n=== SECONDARY PANEL CONTENT ===")
sec = driver.find_elements(By.CSS_SELECTOR, "#secondary")
if sec:
    inner = sec[0].get_attribute("innerHTML")[:2000]
    # Just print tag names and IDs
    import re
    tags = re.findall(r'<([\w-]+)[^>]*(?:id="([^"]*)")?', inner[:2000])
    unique = set()
    for tag, tid in tags:
        key = f"{tag}#{tid}" if tid else tag
        if key not in unique:
            unique.add(key)
            print(f"  {key}")

driver.quit()
