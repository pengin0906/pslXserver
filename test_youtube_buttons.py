#!/usr/bin/env python3
"""
YouTube button click test via Selenium on Xserver.
Tests various YouTube UI buttons to check which ones work and which don't.
"""

import time
import sys
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.action_chains import ActionChains
from selenium.webdriver.common.keys import Keys
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
    options.add_argument("--disable-features=OverlayScrollbar")
    options.add_argument("--lang=ja")
    options.add_argument("--display=192.168.0.101:0")
    options.add_argument("--user-data-dir=/home/pengin0906/snap/chromium/common/chromium/")

    service = Service("/usr/bin/chromedriver")
    driver = webdriver.Chrome(service=service, options=options)
    driver.implicitly_wait(3)
    return driver

def highlight(driver, element):
    """Highlight element with red border."""
    try:
        driver.execute_script(
            "arguments[0].style.outline='3px solid red'; arguments[0].style.outlineOffset='2px'", element)
    except:
        pass

def find_visible(driver, by, value, timeout=5):
    """Find a visible/displayed element from potentially multiple matches."""
    try:
        elements = WebDriverWait(driver, timeout).until(
            lambda d: d.find_elements(by, value)
        )
        for el in elements:
            if el.is_displayed():
                return el
        return elements[0] if elements else None
    except:
        return None

def try_click(driver, element, description):
    """Try multiple click methods on an element."""
    if element is None:
        print(f"  [FAIL] {description} - element is None")
        return False

    highlight(driver, element)
    time.sleep(0.3)

    # Method 1: regular click
    try:
        element.click()
        print(f"  [OK] {description} - regular click")
        return True
    except Exception as e1:
        pass

    # Method 2: JS click
    try:
        driver.execute_script("arguments[0].click()", element)
        print(f"  [OK] {description} - JS click (regular failed)")
        return True
    except Exception as e2:
        pass

    # Method 3: ActionChains
    try:
        ActionChains(driver).move_to_element(element).click().perform()
        print(f"  [OK] {description} - ActionChains click")
        return True
    except Exception as e3:
        print(f"  [FAIL] {description} - all click methods failed")
        return False

def hover_player(driver):
    """Hover over video player to reveal controls."""
    try:
        player = driver.find_element(By.CSS_SELECTOR, ".html5-video-player")
        ActionChains(driver).move_to_element(player).perform()
        time.sleep(1)
    except:
        pass

def test_youtube_buttons(driver):
    results = []

    # Navigate to YouTube
    print("\n=== Opening YouTube ===")
    driver.get("https://www.youtube.com")
    time.sleep(4)

    # Handle consent dialog
    try:
        consent = driver.find_elements(By.XPATH,
            "//button[contains(., 'Accept') or contains(., '同意') or contains(., 'すべて同意')]")
        for btn in consent:
            if btn.is_displayed():
                btn.click()
                time.sleep(2)
                break
    except:
        pass

    # Test 1: Search box
    print("\n=== Test 1: Search Box ===")
    try:
        search = WebDriverWait(driver, 10).until(
            EC.element_to_be_clickable((By.NAME, "search_query"))
        )
        search.click()
        time.sleep(0.3)
        search.send_keys("test video")
        time.sleep(0.3)
        print("  [OK] Search box - typing works")
        results.append(("Search box type", True))
    except Exception as e:
        print(f"  [FAIL] Search box: {e}")
        results.append(("Search box type", False))

    # Test 2: Search button (new class-based selector)
    print("\n=== Test 2: Search Button ===")
    el = find_visible(driver, By.CSS_SELECTOR, ".ytSearchboxComponentSearchButton")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "button[aria-label='Search']")
    ok = try_click(driver, el, "Search button")
    results.append(("Search button", ok))
    time.sleep(3)

    # Navigate to video
    print("\n=== Navigating to test video ===")
    driver.get("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    time.sleep(10)

    # Hover player to show controls
    hover_player(driver)

    # Test 3: Play/Pause
    print("\n=== Test 3: Play/Pause Button ===")
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-play-button")
    ok = try_click(driver, el, "Play/Pause button")
    results.append(("Play/Pause", ok))
    time.sleep(1)
    hover_player(driver)

    # Test 4: Mute button (correct selector)
    print("\n=== Test 4: Mute Button ===")
    hover_player(driver)
    # .ytp-mute-button is not a <button>, try aria-label
    el = find_visible(driver, By.CSS_SELECTOR, ".ytp-volume-area button")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, ".ytp-mute-button")
    ok = try_click(driver, el, "Mute button")
    results.append(("Mute", ok))
    time.sleep(1)

    # Test 5: Settings gear
    print("\n=== Test 5: Settings Button ===")
    hover_player(driver)
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-settings-button")
    ok = try_click(driver, el, "Settings gear")
    results.append(("Settings gear", ok))
    time.sleep(1)
    # Close settings popup
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 6: Fullscreen
    print("\n=== Test 6: Fullscreen Button ===")
    hover_player(driver)
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-fullscreen-button")
    ok = try_click(driver, el, "Fullscreen button")
    results.append(("Fullscreen", ok))
    time.sleep(2)
    # Exit fullscreen
    try:
        driver.execute_script("document.exitFullscreen()")
    except:
        ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(1)

    # Test 7: Like button
    print("\n=== Test 7: Like Button ===")
    # Find visible like-button-view-model's button
    el = find_visible(driver, By.XPATH,
        "//like-button-view-model[.//button[@aria-label]]//button")
    if el is None:
        el = find_visible(driver, By.XPATH,
            "//button[contains(@aria-label, '高く評価')]")
    ok = try_click(driver, el, "Like button")
    results.append(("Like button", ok))
    time.sleep(1)

    # Test 8: Dislike button
    print("\n=== Test 8: Dislike Button ===")
    el = find_visible(driver, By.XPATH,
        "//dislike-button-view-model[.//button[@aria-label]]//button")
    if el is None:
        el = find_visible(driver, By.XPATH,
            "//button[contains(@aria-label, '低く評価')]")
    ok = try_click(driver, el, "Dislike button")
    results.append(("Dislike button", ok))
    time.sleep(1)

    # Test 9: Share button
    print("\n=== Test 9: Share Button ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='共有']")
    ok = try_click(driver, el, "Share button")
    results.append(("Share button", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 10: Save/playlist button
    print("\n=== Test 10: Save Button ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='再生リストに保存']")
    ok = try_click(driver, el, "Save button")
    results.append(("Save button", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 11: Subscribe button
    print("\n=== Test 11: Subscribe Button ===")
    el = find_visible(driver, By.CSS_SELECTOR,
        "#subscribe-button-shape button")
    ok = try_click(driver, el, "Subscribe button")
    results.append(("Subscribe button", ok))
    time.sleep(1)

    # Test 12: Progress bar seek
    print("\n=== Test 12: Progress Bar (Seek) ===")
    hover_player(driver)
    try:
        progress = driver.find_element(By.CSS_SELECTOR, ".ytp-progress-bar")
        highlight(driver, progress)
        time.sleep(0.3)
        size = progress.size
        ActionChains(driver).move_to_element_with_offset(
            progress, int(size['width'] * 0.5), int(size['height'] / 2)
        ).click().perform()
        print("  [OK] Progress bar seek - clicked at 50%")
        results.append(("Progress bar seek", True))
    except Exception as e:
        print(f"  [FAIL] Progress bar seek: {e}")
        results.append(("Progress bar seek", False))
    time.sleep(1)

    # Test 13: More actions (3-dot menu under video)
    print("\n=== Test 13: More Actions Menu ===")
    el = find_visible(driver, By.XPATH,
        "//yt-button-shape[@id='button-shape']//button[@aria-label='その他の操作']")
    ok = try_click(driver, el, "More actions (3-dot)")
    results.append(("More actions menu", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 14: Recommended video
    print("\n=== Test 14: Recommended Video ===")
    # Scroll down to load recommendations
    driver.execute_script("window.scrollBy(0, 300)")
    time.sleep(3)
    # Try newer YouTube DOM structures
    el = find_visible(driver, By.CSS_SELECTOR,
        "ytd-compact-video-renderer a#thumbnail", timeout=10)
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR,
            "ytd-watch-next-secondary-results-renderer a#thumbnail", timeout=5)
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR,
            "#related a#thumbnail", timeout=3)
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR,
            "#secondary a[href*='watch']", timeout=3)
    if el is None:
        # Try any link with /watch in the page
        el = find_visible(driver, By.XPATH,
            "//ytd-watch-next-secondary-results-renderer//a[@href]", timeout=5)
    if el:
        current_url = driver.current_url
        ok = try_click(driver, el, "Recommended video")
        time.sleep(3)
        if ok and driver.current_url != current_url:
            print("  -> Navigated to new video!")
        elif ok:
            print("  -> Click registered but URL unchanged (might be same video)")
        results.append(("Recommended video", ok))
    else:
        print("  [FAIL] Recommended video - no thumbnail element found")
        results.append(("Recommended video", False))
    time.sleep(1)

    # Test 15: YouTube logo (Home)
    print("\n=== Test 15: YouTube Logo (Home) ===")
    el = find_visible(driver, By.CSS_SELECTOR, "a#logo")
    ok = try_click(driver, el, "YouTube logo")
    results.append(("YouTube logo", ok))
    time.sleep(3)

    # Test 16: Hamburger menu
    print("\n=== Test 16: Hamburger Menu ===")
    el = find_visible(driver, By.CSS_SELECTOR, "#guide-button button")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "yt-icon-button#guide-button")
    ok = try_click(driver, el, "Hamburger menu")
    results.append(("Hamburger menu", ok))
    time.sleep(2)

    # Test 17: Autoplay toggle
    print("\n=== Test 17: Autoplay Toggle ===")
    driver.get("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    time.sleep(8)
    hover_player(driver)
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-autonav-toggle")
    if el is None:
        el = find_visible(driver, By.XPATH,
            "//button[contains(@aria-label, '自動再生')]")
    ok = try_click(driver, el, "Autoplay toggle")
    results.append(("Autoplay toggle", ok))
    time.sleep(1)

    # Test 18: Subtitles/CC button
    print("\n=== Test 18: Subtitles Button ===")
    hover_player(driver)
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-subtitles-button")
    ok = try_click(driver, el, "Subtitles/CC button")
    results.append(("Subtitles/CC", ok))
    time.sleep(1)

    # Test 19: Theater mode
    print("\n=== Test 19: Theater Mode ===")
    hover_player(driver)
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-size-button")
    ok = try_click(driver, el, "Theater mode button")
    results.append(("Theater mode", ok))
    time.sleep(2)
    # Revert theater mode
    hover_player(driver)
    try:
        driver.find_element(By.CSS_SELECTOR, "button.ytp-size-button").click()
        time.sleep(1)
    except:
        pass

    # Test 20: Large play button (center overlay)
    print("\n=== Test 20: Large Center Play Button ===")
    # Pause first
    hover_player(driver)
    try:
        pp = driver.find_element(By.CSS_SELECTOR, "button.ytp-play-button")
        pp.click()
        time.sleep(1)
    except:
        pass
    el = find_visible(driver, By.CSS_SELECTOR, "button.ytp-large-play-button")
    ok = try_click(driver, el, "Large play button (center)")
    results.append(("Large play button", ok))
    time.sleep(1)

    # Print summary
    print("\n" + "=" * 60)
    print("YOUTUBE BUTTON TEST RESULTS")
    print("=" * 60)
    passed = sum(1 for _, ok in results if ok)
    failed = sum(1 for _, ok in results if not ok)
    for name, ok in results:
        status = "\033[32mPASS\033[0m" if ok else "\033[31mFAIL\033[0m"
        print(f"  [{status}] {name}")
    print(f"\nTotal: {passed} passed, {failed} failed out of {len(results)} tests")
    print("=" * 60)

    return results

if __name__ == "__main__":
    print("Starting YouTube button test on Xserver...")
    print("Browser will be visible on the Xserver display.")

    driver = setup_driver()
    try:
        results = test_youtube_buttons(driver)
    except Exception as e:
        print(f"\nFatal error: {e}")
        import traceback
        traceback.print_exc()
    finally:
        print("\nKeeping browser open for 5 seconds...")
        time.sleep(5)
        driver.quit()
        print("Done.")
