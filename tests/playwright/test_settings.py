"""Settings tab — structured forms + JSON-editor fallback.

The Settings tab now ships structured cards (WiFi, MQTT, Schedule, …) plus
the raw JSON editor under an "Advanced" collapsed section. These tests
cover (a) the structured forms render with the loaded config, and (b) the
JSON editor still works when expanded.
"""

import json
import re

from playwright.sync_api import Page, expect


def _open_settings(page: Page, host_url: str) -> None:
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="settings"]').click()
    # Wait for any Settings card to render — the WiFi card's hostname input
    # only gets a value after /api/config has resolved.
    expect(page.locator("#wifi-hostname")).not_to_have_value("", timeout=2_000)


def test_settings_renders_structured_forms(page: Page, host_url: str):
    _open_settings(page, host_url)
    # Default config has hostname "doremorwater" and AP SSID matching YAML.
    expect(page.locator("#wifi-hostname")).to_have_value("doremorwater")
    expect(page.locator("#wifi-ap-ssid")).to_have_value(
        re.compile(r"Doremorwater Fallback Hotspot")
    )
    # Schedule card pre-populated with two rules from the YAML default.
    expect(page.locator("#schedule-rules .list-row")).to_have_count(2)
    # Sensor calibration cards are inside <details>; the lists should still
    # exist in the DOM.
    expect(page.locator("#cal-battery .list-row")).to_have_count(2)


def test_settings_wifi_save_round_trip(page: Page, host_url: str):
    _open_settings(page, host_url)
    # Add a network via the form.
    page.locator("#wifi-add").click()
    rows = page.locator("#wifi-networks .list-row")
    expect(rows).to_have_count(1)
    rows.first.locator('[data-bind="ssid"]').fill("home_5g")
    rows.first.locator('[data-bind="password"]').fill("hunter2")
    page.locator("#wifi-save").click()
    expect(page.locator("#wifi-msg")).to_have_text(re.compile(r"saved"), timeout=2_000)

    # Reload the tab and confirm the network persisted.
    page.reload()
    page.locator('nav.tabs button[data-tab="settings"]').click()
    expect(page.locator("#wifi-hostname")).not_to_have_value("", timeout=2_000)
    expect(page.locator("#wifi-networks .list-row")).to_have_count(1)
    expect(
        page.locator('#wifi-networks .list-row [data-bind="ssid"]').first
    ).to_have_value("home_5g")


def test_advanced_json_editor_round_trip(page: Page, host_url: str):
    _open_settings(page, host_url)
    # Expand the Advanced details so the textarea becomes visible.
    page.locator("section.card:has-text('Advanced (raw config)') details > summary").click()
    expect(page.locator("#cfg-json")).to_be_visible()
    raw = page.locator("#cfg-json").input_value()
    cfg = json.loads(raw)
    cfg["timezone"] = "Europe/Madrid"
    page.locator("#cfg-json").fill(json.dumps(cfg, indent=2))
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"saved"), timeout=2_000)


def test_advanced_json_editor_invalid_json_inline_error(page: Page, host_url: str):
    _open_settings(page, host_url)
    page.locator("section.card:has-text('Advanced (raw config)') details > summary").click()
    page.locator("#cfg-json").fill("{ this is not json }")
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"invalid JSON"), timeout=1_000)
