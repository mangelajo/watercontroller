"""Dashboard tab — sensor cards, switches, log panel."""

import re

from playwright.sync_api import Page, expect


def test_dashboard_loads(page: Page, host_url: str):
    page.goto(host_url)
    expect(page).to_have_title(re.compile(r"doremorwater", re.I))
    # Three switch rows.
    expect(page.locator(".switch-row")).to_have_count(3)
    # Connection chip says something.
    expect(page.locator("#conn-text")).not_to_have_text("connecting…")


def test_sprinkler_toggle_round_trip(page: Page, host_url: str):
    page.goto(host_url)
    s1 = page.locator("#t-s1")
    expect(s1).not_to_have_class(re.compile(r"\bon\b"))
    s1.click()
    # The SPA polls /api/status every 2s; allow up to 4s for the toggle to reflect.
    expect(s1).to_have_class(re.compile(r"\bon\b"), timeout=4_000)


def test_water_control_transitions(page: Page, host_url: str):
    page.goto(host_url)
    wc = page.locator("#t-wc")
    wc.click()
    # During the 16 s sequence the toggle goes through "busy" before reaching "on".
    expect(wc).to_have_class(re.compile(r"\bbusy\b"), timeout=4_000)
