#!/usr/bin/env python3
"""Gemini.com button click test via Selenium on Xserver."""

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

def test_gemini(driver):
    results = []

    print("\n=== Opening gemini.google.com ===")
    driver.get("https://gemini.google.com")
    time.sleep(8)

    # Test 1: Chat input box
    print("\n=== Test 1: Chat Input Box ===")
    el = find_visible(driver, By.CSS_SELECTOR, "rich-textarea, .ql-editor, [contenteditable='true'], textarea", timeout=10)
    if el is None:
        el = find_visible(driver, By.XPATH, "//*[@contenteditable='true'] | //textarea | //*[@role='textbox']", timeout=5)
    if el:
        try:
            el.click()
            time.sleep(0.3)
            el.send_keys("Hello Gemini")
            time.sleep(0.5)
            print("  [OK] Chat input - typing works")
            results.append(("Chat input", True))
        except:
            try:
                driver.execute_script("arguments[0].innerText = 'Hello Gemini'", el)
                print("  [OK] Chat input - JS setText works")
                results.append(("Chat input", True))
            except Exception as e:
                print(f"  [FAIL] Chat input: {e}")
                results.append(("Chat input", False))
    else:
        print("  [FAIL] Chat input - not found")
        results.append(("Chat input", False))

    # Test 2: Send button
    print("\n=== Test 2: Send Button ===")
    el = find_visible(driver, By.CSS_SELECTOR, "button[aria-label*='送信'], button[aria-label*='Send'], .send-button, button[mattooltip*='Send']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//button[contains(@aria-label, 'Send') or contains(@aria-label, '送信') or contains(@mattooltip, 'Send')]")
    ok = try_click(driver, el, "Send button")
    results.append(("Send button", ok))
    time.sleep(5)

    # Test 3: New chat button (it's a link with aria-label)
    print("\n=== Test 3: New Chat Button ===")
    el = find_visible(driver, By.XPATH,
        "//a[@aria-label='チャットを新規作成' or @aria-label='New chat']")
    if el is None:
        el = find_visible(driver, By.XPATH, "//a[contains(., 'チャットを新規作成')]")
    ok = try_click(driver, el, "New chat button")
    results.append(("New chat", ok))
    time.sleep(2)

    # Test 4: Sidebar toggle / hamburger (メインメニュー)
    print("\n=== Test 4: Sidebar Toggle ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='メインメニュー' or @aria-label='Main menu']")
    ok = try_click(driver, el, "Sidebar toggle")
    results.append(("Sidebar toggle", ok))
    time.sleep(2)

    # Test 5: Model selector / mode selector
    print("\n=== Test 5: Model Selector ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='モード選択ツールを開く' or contains(@aria-label, 'mode')]")
    if el is None:
        el = find_visible(driver, By.XPATH, "//button[contains(., '高速モード') or contains(., 'PRO')]")
    ok = try_click(driver, el, "Model selector")
    results.append(("Model selector", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 6: Settings & help
    print("\n=== Test 6: Settings ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='Settings & help' or contains(., '設定とヘルプ')]")
    ok = try_click(driver, el, "Settings & help")
    results.append(("Settings", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(1)

    # Test 7: Upload/attach file (ファイルをアップロード)
    print("\n=== Test 7: Upload/Attach Button ===")
    el = find_visible(driver, By.XPATH,
        "//button[contains(@aria-label, 'ファイルをアップロード') or contains(@aria-label, 'Upload')]")
    ok = try_click(driver, el, "Upload/Attach")
    results.append(("Upload/Attach", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 8: Mic / voice input (マイク)
    print("\n=== Test 8: Voice Input ===")
    el = find_visible(driver, By.XPATH,
        "//button[@aria-label='マイク' or @aria-label='Microphone']")
    ok = try_click(driver, el, "Voice input (mic)")
    results.append(("Voice input", ok))
    time.sleep(2)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 9: Account button
    print("\n=== Test 9: Account Button ===")
    el = find_visible(driver, By.XPATH,
        "//a[contains(@aria-label, 'Google アカウント')]")
    if el is None:
        el = find_visible(driver, By.CSS_SELECTOR, "a[href*='accounts.google.com/SignOut']")
    ok = try_click(driver, el, "Account button")
    results.append(("Account", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 10: Search button (検索)
    print("\n=== Test 10: Search Button ===")
    el = find_visible(driver, By.XPATH, "//button[@aria-label='検索']")
    ok = try_click(driver, el, "Search button")
    results.append(("Search", ok))
    time.sleep(1)
    ActionChains(driver).send_keys(Keys.ESCAPE).perform()
    time.sleep(0.5)

    # Test 11: Suggestion cards
    print("\n=== Test 11: Suggestion Card ===")
    el = find_visible(driver, By.CSS_SELECTOR, "button.card-zero-state")
    ok = try_click(driver, el, "Suggestion card")
    results.append(("Suggestion card", ok))
    time.sleep(2)

    # Test 12: Chat history link
    print("\n=== Test 12: Chat History Link ===")
    el = find_visible(driver, By.XPATH,
        "//a[contains(@href, 'gemini.google.com/app/')]")
    ok = try_click(driver, el, "Chat history link")
    results.append(("Chat history", ok))
    time.sleep(2)
    driver.back()
    time.sleep(2)

    # Summary
    print("\n" + "=" * 60)
    print("GEMINI BUTTON TEST RESULTS")
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
    print("Starting Gemini button test on Xserver...")
    driver = setup_driver()
    try:
        test_gemini(driver)
    except Exception as e:
        print(f"\nFatal error: {e}")
        import traceback
        traceback.print_exc()
    finally:
        print("\nKeeping browser open for 5 seconds...")
        time.sleep(5)
        driver.quit()
        print("Done.")
