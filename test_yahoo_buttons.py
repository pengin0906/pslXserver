#!/usr/bin/env python3
"""Yahoo! JAPAN button click test via Selenium on Xserver."""

import time
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
    options.add_argument("--lang=ja")
    options.add_argument("--display=192.168.0.101:0")
    options.add_argument("--user-data-dir=/home/pengin0906/snap/chromium/common/chromium/")
    service = Service("/usr/bin/chromedriver")
    driver = webdriver.Chrome(service=service, options=options)
    driver.implicitly_wait(3)
    return driver

def highlight(driver, element):
    try:
        driver.execute_script(
            "arguments[0].style.outline='3px solid red'; arguments[0].style.outlineOffset='2px'", element)
    except:
        pass

def find_visible(driver, by, value, timeout=5):
    try:
        elements = WebDriverWait(driver, timeout).until(lambda d: d.find_elements(by, value))
        for el in elements:
            if el.is_displayed():
                return el
        return elements[0] if elements else None
    except:
        return None

def try_click(driver, element, description):
    if element is None:
        print(f"  [FAIL] {description} - element not found")
        return False
    highlight(driver, element)
    time.sleep(0.3)
    for method_name, fn in [
        ("regular", lambda: element.click()),
        ("JS", lambda: driver.execute_script("arguments[0].click()", element)),
        ("ActionChains", lambda: ActionChains(driver).move_to_element(element).click().perform()),
    ]:
        try:
            fn()
            print(f"  [OK] {description} - {method_name} click")
            return True
        except:
            continue
    print(f"  [FAIL] {description} - all click methods failed")
    return False

def test_yahoo(driver):
    results = []

    print("\n=== Opening Yahoo! JAPAN ===")
    driver.get("https://www.yahoo.co.jp")
    time.sleep(5)

    # Test 1: Search box
    print("\n=== Test 1: Search Box ===")
    try:
        search = WebDriverWait(driver, 10).until(
            EC.element_to_be_clickable((By.CSS_SELECTOR, "input[name='p'], input.SearchBox__searchInput, input#srchtxt"))
        )
        search.click()
        time.sleep(0.3)
        search.send_keys("テスト検索")
        time.sleep(0.5)
        print("  [OK] Search box - typing works")
        results.append(("Search box", True))
    except Exception as e:
        print(f"  [FAIL] Search box: {e}")
        results.append(("Search box", False))

    # Test 2: Search button
    print("\n=== Test 2: Search Button ===")
    el = find_visible(driver, By.CSS_SELECTOR, "button[type='submit'], input[type='submit']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//button[contains(., '検索')]")
    ok = try_click(driver, el, "Search button")
    results.append(("Search button", ok))
    time.sleep(3)

    # Test 3: Search result link
    print("\n=== Test 3: Search Result Link ===")
    el = find_visible(driver, By.CSS_SELECTOR, "#contents a h3, .sw-CardBase a, #WS2m a", timeout=10)
    if el is None:
        el = find_visible(driver, By.XPATH, "//div[@id='contents']//a//h3/..")
    if el:
        current_url = driver.current_url
        ok = try_click(driver, el, "Search result")
        time.sleep(3)
        results.append(("Search result", ok))
        driver.back()
        time.sleep(2)
    else:
        print("  [FAIL] Search result - not found")
        results.append(("Search result", False))

    # Back to Yahoo top
    driver.get("https://www.yahoo.co.jp")
    time.sleep(5)

    # Test 4: News headline click
    print("\n=== Test 4: News Headline ===")
    el = find_visible(driver, By.CSS_SELECTOR,
        ".topicsListItem a, .sc-fLlhyt a, a[data-ual-gotocontent]")
    if el is None:
        el = find_visible(driver, By.XPATH,
            "//section//a[contains(@href, 'news.yahoo.co.jp')]")
    if el is None:
        # Generic: first visible link in the main topics area
        el = find_visible(driver, By.CSS_SELECTOR, "#tabpanelTopics1 a, .Topics a")
    ok = try_click(driver, el, "News headline")
    results.append(("News headline", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 5: Mail link
    print("\n=== Test 5: Mail Link ===")
    el = find_visible(driver, By.XPATH, "//a[contains(., 'メール') and contains(@href, 'mail')]")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "a[href*='mail.yahoo.co.jp']")
    ok = try_click(driver, el, "Mail link")
    results.append(("Mail link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 6: Navigation tabs (ニュース、スポーツ等)
    print("\n=== Test 6: News Nav Tab ===")
    driver.get("https://www.yahoo.co.jp")
    time.sleep(4)
    el = find_visible(driver, By.XPATH, "//a[contains(., 'ニュース') and contains(@href, 'news')]")
    ok = try_click(driver, el, "News nav tab")
    results.append(("News nav tab", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 7: Shopping link
    print("\n=== Test 7: Shopping Link ===")
    el = find_visible(driver, By.XPATH, "//a[contains(., 'ショッピング') or contains(@href, 'shopping')]")
    ok = try_click(driver, el, "Shopping link")
    results.append(("Shopping link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 8: Yahoo! JAPAN logo
    print("\n=== Test 8: Yahoo Logo ===")
    driver.get("https://search.yahoo.co.jp/search?p=test")
    time.sleep(4)
    el = find_visible(driver, By.CSS_SELECTOR, "a[href*='yahoo.co.jp'] img, .Header__logo a, a#logo")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(@href, 'www.yahoo.co.jp')]//img | //a[@id='logo']")
    ok = try_click(driver, el, "Yahoo logo")
    results.append(("Yahoo logo", ok))
    time.sleep(3)

    # Test 9: Weather widget
    print("\n=== Test 9: Weather Link ===")
    driver.get("https://www.yahoo.co.jp")
    time.sleep(4)
    el = find_visible(driver, By.XPATH, "//a[contains(@href, 'weather.yahoo.co.jp')]")
    ok = try_click(driver, el, "Weather link")
    results.append(("Weather link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 10: Sports link
    print("\n=== Test 10: Sports Link ===")
    el = find_visible(driver, By.XPATH, "//a[contains(., 'スポーツ') and contains(@href, 'sports')]")
    ok = try_click(driver, el, "Sports link")
    results.append(("Sports link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 11: Tab switching on top page (主要/経済/エンタメ etc)
    print("\n=== Test 11: Topic Tabs ===")
    driver.get("https://www.yahoo.co.jp")
    time.sleep(4)
    el = find_visible(driver, By.XPATH,
        "//button[contains(., '経済') or contains(., 'エンタメ') or contains(., 'スポーツ')]")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "[role='tab']:nth-child(2), .Topics__tab:nth-child(2)")
    ok = try_click(driver, el, "Topic tab")
    results.append(("Topic tab", ok))
    time.sleep(2)

    # Test 12: More news link (もっと見る)
    print("\n=== Test 12: More News ===")
    el = find_visible(driver, By.XPATH, "//a[contains(., 'もっと見る')]")
    ok = try_click(driver, el, "More news link")
    results.append(("More news link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Summary
    print("\n" + "=" * 60)
    print("YAHOO! JAPAN BUTTON TEST RESULTS")
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
    print("Starting Yahoo! JAPAN button test on Xserver...")
    driver = setup_driver()
    try:
        test_yahoo(driver)
    except Exception as e:
        print(f"\nFatal error: {e}")
        import traceback
        traceback.print_exc()
    finally:
        print("\nKeeping browser open for 5 seconds...")
        time.sleep(5)
        driver.quit()
        print("Done.")
