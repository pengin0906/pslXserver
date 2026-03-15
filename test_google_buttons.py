#!/usr/bin/env python3
"""
Google.com button click test via Selenium on Xserver.
"""

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
    options.add_argument("--disable-features=OverlayScrollbar")
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
    if element is None:
        print(f"  [FAIL] {description} - element not found")
        return False
    highlight(driver, element)
    time.sleep(0.3)
    # regular click
    try:
        element.click()
        print(f"  [OK] {description} - regular click")
        return True
    except:
        pass
    # JS click
    try:
        driver.execute_script("arguments[0].click()", element)
        print(f"  [OK] {description} - JS click")
        return True
    except:
        pass
    # ActionChains
    try:
        ActionChains(driver).move_to_element(element).click().perform()
        print(f"  [OK] {description} - ActionChains click")
        return True
    except:
        print(f"  [FAIL] {description} - all click methods failed")
        return False

def test_google(driver):
    results = []

    # === Google Top Page ===
    print("\n=== Opening Google.com ===")
    driver.get("https://www.google.com")
    time.sleep(5)

    # Test 1: Search box
    print("\n=== Test 1: Search Box ===")
    try:
        search = WebDriverWait(driver, 10).until(
            EC.element_to_be_clickable((By.CSS_SELECTOR, "textarea[name='q'], input[name='q']"))
        )
        search.click()
        time.sleep(0.3)
        search.send_keys("Selenium test")
        time.sleep(0.5)
        print("  [OK] Search box - typing works")
        results.append(("Search box type", True))
    except Exception as e:
        print(f"  [FAIL] Search box: {e}")
        results.append(("Search box type", False))

    # Test 2: Google Search button
    print("\n=== Test 2: Google Search Button ===")
    # Dismiss autocomplete first
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)
    el = find_visible(driver, By.CSS_SELECTOR, "input[name='btnK']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//input[@value='Google 検索' or @value='Google Search']")
    ok = try_click(driver, el, "Google Search button")
    results.append(("Google Search button", ok))
    time.sleep(3)

    # Now we should be on search results page
    print("\n=== On Search Results Page ===")

    # Test 3: Click a search result link
    print("\n=== Test 3: Search Result Link ===")
    el = find_visible(driver, By.CSS_SELECTOR, "#rso a h3", timeout=10)
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "#search a h3", timeout=5)
    if el:
        # Click the h3 directly (it's inside the <a>)
        ok = try_click(driver, el, "Search result link")
        time.sleep(3)
        results.append(("Search result link", ok))
        driver.back()
        time.sleep(3)
    else:
        print("  [FAIL] No search result found")
        results.append(("Search result link", False))

    # Test 4: Images tab
    print("\n=== Test 4: Images Tab ===")
    el = find_visible(driver, By.XPATH, "//a[contains(., '画像') or contains(., 'Images')]")
    ok = try_click(driver, el, "Images tab")
    results.append(("Images tab", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 5: News tab (via direct URL since Google AI mode hides nav tabs)
    print("\n=== Test 5: News Tab ===")
    driver.get("https://www.google.com/search?q=Selenium+test&tbm=nws")
    time.sleep(4)
    # Verify we're on news results - find a news result link
    el = find_visible(driver, By.CSS_SELECTOR, "#search a, #rso a", timeout=5)
    if el:
        ok = try_click(driver, el, "News result link")
        results.append(("News result link", ok))
        time.sleep(2)
        driver.back()
        time.sleep(2)
    else:
        print("  [FAIL] News results not found")
        results.append(("News result link", False))

    # Test 6: Videos tab (via direct URL)
    print("\n=== Test 6: Videos Tab ===")
    driver.get("https://www.google.com/search?q=Selenium+test&tbm=vid")
    time.sleep(4)
    el = find_visible(driver, By.CSS_SELECTOR, "#search a, #rso a", timeout=5)
    if el:
        ok = try_click(driver, el, "Video result link")
        results.append(("Video result link", ok))
        time.sleep(2)
        driver.back()
        time.sleep(2)
    else:
        print("  [FAIL] Video results not found")
        results.append(("Video result link", False))

    # Test 7: "もっと見る" / More filters
    print("\n=== Test 7: More Filters ===")
    driver.get("https://www.google.com/search?q=Selenium+test")
    time.sleep(4)
    el = find_visible(driver, By.XPATH,
        "//div[@role='navigation']//a[contains(., 'もっと見る') or contains(., 'More')]")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(., 'すべてのフィルタ')]")
    if el is None:
        el = find_visible(driver, By.XPATH, "//div[@role='navigation']//a[contains(., '書籍') or contains(., 'フライト') or contains(., 'ショッピング')]")
    ok = try_click(driver, el, "More filters")
    results.append(("More filters", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 8: Tools button
    print("\n=== Test 8: Tools Button ===")
    driver.get("https://www.google.com/search?q=Selenium+test")
    time.sleep(4)
    el = find_visible(driver, By.CSS_SELECTOR, "#hdtb-tls")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(., 'ツール')]")
    ok = try_click(driver, el, "Tools button")
    results.append(("Tools button", ok))
    time.sleep(2)

    # Test 9: Next page
    print("\n=== Test 9: Next Page ===")
    # Scroll to bottom to find next page link
    driver.execute_script("window.scrollTo(0, document.body.scrollHeight)")
    time.sleep(2)
    el = find_visible(driver, By.CSS_SELECTOR, "#pnnext")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(., '次へ')]")
    ok = try_click(driver, el, "Next page")
    results.append(("Next page", ok))
    time.sleep(3)

    # Test 10: Google logo (back to home)
    print("\n=== Test 10: Google Logo ===")
    el = find_visible(driver, By.CSS_SELECTOR, ".logo a")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "a[href*='google.com/webhp']")
    ok = try_click(driver, el, "Google logo")
    results.append(("Google logo", ok))
    time.sleep(3)

    # Back to home page
    driver.get("https://www.google.com")
    time.sleep(4)

    # Test 11: I'm Feeling Lucky button
    print("\n=== Test 11: I'm Feeling Lucky ===")
    el = find_visible(driver, By.CSS_SELECTOR, "input[name='btnI']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//input[@value='I\\'m Feeling Lucky' or @value='I\\'m Feeling Lucky']")
    ok = try_click(driver, el, "I'm Feeling Lucky")
    results.append(("I'm Feeling Lucky", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 12: Voice search (microphone)
    print("\n=== Test 12: Voice Search (Mic) ===")
    driver.get("https://www.google.com")
    time.sleep(4)
    el = find_visible(driver, By.CSS_SELECTOR, "div[aria-label*='音声'] , div[aria-label*='Voice']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//*[@aria-label='音声で検索' or @aria-label='Search by voice']")
    ok = try_click(driver, el, "Voice search mic")
    results.append(("Voice search mic", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 13: Camera/Lens search
    print("\n=== Test 13: Google Lens (Camera) ===")
    el = find_visible(driver, By.CSS_SELECTOR, "div[aria-label*='レンズ'], div[aria-label*='Lens']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//*[@aria-label='画像で検索' or @aria-label='Search by image']")
    ok = try_click(driver, el, "Google Lens/Camera")
    results.append(("Google Lens", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 14: Profile/Account icon
    print("\n=== Test 14: Account Icon ===")
    el = find_visible(driver, By.CSS_SELECTOR, "a[aria-label*='Google アカウント'], a[aria-label*='Google Account']")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "#gb a.gb_A, #gb a.gb_b, #gb img.gb_o")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(@href, 'accounts.google.com')]")
    ok = try_click(driver, el, "Account icon")
    results.append(("Account icon", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 15: Google Apps grid (9-dot menu)
    print("\n=== Test 15: Google Apps Grid ===")
    el = find_visible(driver, By.CSS_SELECTOR, "a[aria-label='Google アプリ'], a[aria-label='Google apps']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//*[@aria-label='Google アプリ' or @aria-label='Google apps']")
    ok = try_click(driver, el, "Google Apps grid")
    results.append(("Google Apps grid", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 16: Gmail link
    print("\n=== Test 16: Gmail Link ===")
    driver.get("https://www.google.com")
    time.sleep(4)
    el = find_visible(driver, By.XPATH, "//a[contains(., 'Gmail')]")
    ok = try_click(driver, el, "Gmail link")
    results.append(("Gmail link", ok))
    time.sleep(3)
    driver.back()
    time.sleep(2)

    # Test 17: Search by pressing Enter
    print("\n=== Test 17: Search by Enter Key ===")
    driver.get("https://www.google.com")
    time.sleep(4)
    try:
        search = driver.find_element(By.CSS_SELECTOR, "textarea[name='q'], input[name='q']")
        search.click()
        time.sleep(0.3)
        search.send_keys("hello world")
        time.sleep(0.5)
        search.send_keys(Keys.RETURN)
        time.sleep(3)
        if "search" in driver.current_url or "q=" in driver.current_url:
            print("  [OK] Search by Enter key - navigated to results")
            results.append(("Search by Enter", True))
        else:
            print("  [FAIL] Search by Enter - didn't navigate")
            results.append(("Search by Enter", False))
    except Exception as e:
        print(f"  [FAIL] Search by Enter: {e}")
        results.append(("Search by Enter", False))

    # Test 18: Autocomplete suggestion click
    print("\n=== Test 18: Autocomplete Suggestion ===")
    driver.get("https://www.google.com")
    time.sleep(4)
    try:
        search = driver.find_element(By.CSS_SELECTOR, "textarea[name='q'], input[name='q']")
        search.click()
        time.sleep(0.3)
        search.send_keys("pytho")
        time.sleep(2)
        # Find autocomplete suggestions
        el = find_visible(driver, By.CSS_SELECTOR, "ul[role='listbox'] li, .sbct, .G43f7e")
        if el is None:
            el = find_visible(driver, By.XPATH, "//div[@role='listbox']//li | //div[@role='option']")
        ok = try_click(driver, el, "Autocomplete suggestion")
        time.sleep(3)
        results.append(("Autocomplete suggestion", ok))
    except Exception as e:
        print(f"  [FAIL] Autocomplete: {e}")
        results.append(("Autocomplete suggestion", False))

    # Print summary
    print("\n" + "=" * 60)
    print("GOOGLE.COM BUTTON TEST RESULTS")
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
    print("Starting Google.com button test on Xserver...")
    driver = setup_driver()
    try:
        test_google(driver)
    except Exception as e:
        print(f"\nFatal error: {e}")
        import traceback
        traceback.print_exc()
    finally:
        print("\nKeeping browser open for 5 seconds...")
        time.sleep(5)
        driver.quit()
        print("Done.")
