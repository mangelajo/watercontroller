"""Settings tab — JSON config editor, factory reset (host returns 501)."""

import json
import re

from playwright.sync_api import Page, expect


def test_settings_tab_shows_config_json(page: Page, host_url: str):
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="settings"]').click()
    expect(page.locator("#cfg-json")).to_be_visible()
    # The textarea should populate within 2s after switching to the tab.
    expect(page.locator("#cfg-json")).not_to_be_empty(timeout=2_000)
    raw = page.locator("#cfg-json").input_value()
    cfg = json.loads(raw)
    assert "wifi" in cfg
    assert "schedule" in cfg
    assert cfg["wifi"]["hostname"] == "doremorwater"


def test_settings_save_round_trip(page: Page, host_url: str):
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="settings"]').click()
    expect(page.locator("#cfg-json")).not_to_be_empty(timeout=2_000)

    # Edit timezone and save.
    raw = page.locator("#cfg-json").input_value()
    cfg = json.loads(raw)
    cfg["timezone"] = "Europe/London"
    page.locator("#cfg-json").fill(json.dumps(cfg, indent=2))
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"saved"), timeout=2_000)

    # Reload, confirm round-trip.
    page.locator("#cfg-reload").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"loaded"), timeout=2_000)
    raw2 = page.locator("#cfg-json").input_value()
    cfg2 = json.loads(raw2)
    assert cfg2["timezone"] == "Europe/London"


def test_settings_invalid_json_surfaces_error(page: Page, host_url: str):
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="settings"]').click()
    expect(page.locator("#cfg-json")).not_to_be_empty(timeout=2_000)
    page.locator("#cfg-json").fill("{ this is not json }")
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"invalid JSON"), timeout=1_000)
