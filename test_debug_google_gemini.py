#!/usr/bin/env python3
"""Debug failed selectors for Google.com and Gemini."""
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
    options.add_argument("--user-data-dir=/home/pengin0906/snap/chromium/common/chromium/")
    service = Service("/usr/bin/chromedriver")
    driver = webdriver.Chrome(service=service, options=options)
    driver.implicitly_wait(3)
    return driver

driver = setup_driver()

# ======= GOOGLE SEARCH RESULTS DEBUG =======
print("=" * 60)
print("GOOGLE SEARCH RESULTS PAGE DEBUG")
print("=" * 60)
driver.get("https://www.google.com/search?q=Selenium+test")
time.sleep(5)

# Search result links
print("\n--- Search result links ---")
for sel in ["#search a h3", "#rso a h3", "h3.LC20lb", ".g a h3", "a[jsname] h3"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")
    if visible:
        print(f"    first: text='{visible[0].text[:50]}'")

# Videos tab
print("\n--- Videos tab ---")
for sel in ["//a[contains(., '動画')]", "//a[contains(., 'Videos')]",
            "//div[@role='navigation']//a"]:
    elems = driver.find_elements(By.XPATH, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")
    for e in visible[:3]:
        print(f"    text='{e.text}', href='{(e.get_attribute('href') or '')[:60]}'")

# Next page
print("\n--- Next page ---")
for sel in ["#pnnext", "a[aria-label*='次']", "a[aria-label*='Next']",
            "//a[contains(., '次へ')]", "//a[@id='pnnext']",
            "//span[contains(., '次へ')]/.."]:
    if sel.startswith("//"):
        elems = driver.find_elements(By.XPATH, sel)
    else:
        elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")

# Google logo on results page
print("\n--- Google logo on results ---")
for sel in ["a.logo", "a img[alt='Google']", "//a[.//img[@alt='Google']]",
            "a[href*='google.com/webhp']", ".logo a", "#logo a",
            "//a[@href='/']"]:
    if sel.startswith("//"):
        elems = driver.find_elements(By.XPATH, sel)
    else:
        elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")
    for e in visible[:2]:
        href = e.get_attribute("href") or ""
        print(f"    tag={e.tag_name}, href='{href[:60]}'")

# Tools
print("\n--- Tools button ---")
for sel in ["//div[text()='ツール']", "//div[text()='Tools']",
            "//button[contains(., 'ツール')]", "#hdtb-tls",
            "//a[contains(., 'ツール')]"]:
    if sel.startswith("//"):
        elems = driver.find_elements(By.XPATH, sel)
    else:
        elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")

# ======= GEMINI DEBUG =======
print("\n" + "=" * 60)
print("GEMINI DEBUG")
print("=" * 60)
driver.get("https://gemini.google.com")
time.sleep(10)

# All buttons
print("\n--- All visible buttons ---")
buttons = driver.find_elements(By.TAG_NAME, "button")
for i, btn in enumerate(buttons):
    try:
        if btn.is_displayed():
            aria = btn.get_attribute("aria-label") or ""
            text = btn.text[:40] if btn.text else ""
            mattooltip = btn.get_attribute("mattooltip") or ""
            cls = (btn.get_attribute("class") or "")[:50]
            if aria or text or mattooltip:
                print(f"  [{i}] aria='{aria}' text='{text}' tooltip='{mattooltip}' class='{cls}'")
    except:
        pass

# All visible links
print("\n--- Key links ---")
links = driver.find_elements(By.TAG_NAME, "a")
for a in links:
    try:
        if a.is_displayed():
            text = a.text[:40] if a.text else ""
            href = (a.get_attribute("href") or "")[:60]
            aria = a.get_attribute("aria-label") or ""
            if text or aria:
                print(f"  text='{text}' aria='{aria}' href='{href}'")
    except:
        pass

# contenteditable / textbox
print("\n--- Input areas ---")
for sel in ["[contenteditable='true']", "textarea", "[role='textbox']",
            "rich-textarea", ".ql-editor", "div.input-area"]:
    elems = driver.find_elements(By.CSS_SELECTOR, sel)
    visible = [e for e in elems if e.is_displayed()]
    print(f"  {sel}: found {len(elems)}, visible {len(visible)}")

driver.quit()
print("\nDone.")
