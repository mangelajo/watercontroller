"""Settings — per-section tabs + raw JSON editor on the Advanced tab.

The SPA now exposes one top-level tab per setting section (WiFi, Sprinklers,
Schedule, MQTT, Time, Sensors, HTTPS, VPN, Auth, OTA, Advanced) instead of a
single Settings tab. These tests cover (a) structured forms render with the
loaded config across a few representative tabs, and (b) the raw JSON editor
on the Advanced tab still works.
"""

import json
import os
import re

import pytest
from playwright.sync_api import Page, expect


def _open_tab(page: Page, host_url: str, tab: str) -> None:
    """Navigate to host_url and click the named tab.

    Settings tabs lazy-load /api/config the first time you enter them. Most
    callers want to wait for that to land — they assert against a populated
    field after this returns.
    """
    page.goto(host_url)
    page.locator(f'nav.tabs button[data-tab="{tab}"]').click()


def test_wifi_tab_renders_structured_forms(page: Page, host_url: str):
    _open_tab(page, host_url, "wifi")
    expect(page.locator("#wifi-hostname")).to_have_value("doremorwater", timeout=2_000)
    expect(page.locator("#wifi-ap-ssid")).to_have_value(
        re.compile(r"Doremorwater Fallback Hotspot")
    )


def test_schedule_tab_pre_populated_from_yaml_defaults(page: Page, host_url: str):
    _open_tab(page, host_url, "schedule")
    expect(page.locator("#schedule-rules .list-row")).to_have_count(2, timeout=2_000)


def test_sensors_tab_calibration_lists_render(page: Page, host_url: str):
    _open_tab(page, host_url, "sensors")
    # Cal points are inside collapsed <details> but the rows are in the DOM.
    expect(page.locator("#cal-battery .list-row")).to_have_count(2, timeout=2_000)


def test_sprinklers_tab_loads_default_auto_off(page: Page, host_url: str):
    """The form is in minutes; defaults are 7 min (420 s) and 5 min (300 s)."""
    _open_tab(page, host_url, "sprinklers")
    expect(page.locator("#s1-mins")).to_have_value("7", timeout=2_000)
    expect(page.locator("#s2-mins")).to_have_value("5")


def test_sprinklers_form_minutes_round_trip_to_seconds(
    page: Page, host_url: str, api_request_context
):
    """User types 3 min, save, and the persisted field is 180 s.

    Restores the original value at the end so other tests aren't affected.
    """
    cfg_before = api_request_context.get(f"{host_url}/api/config").json()
    s1_before = cfg_before["switches"]["sprinkler_1_auto_off_secs"]
    s2_before = cfg_before["switches"]["sprinkler_2_auto_off_secs"]

    try:
        _open_tab(page, host_url, "sprinklers")
        expect(page.locator("#s1-mins")).not_to_have_value("", timeout=2_000)
        page.locator("#s1-mins").fill("3")
        page.locator("#s2-mins").fill("4")
        page.locator("#sprinklers-save").click()
        expect(page.locator("#sprinklers-msg")).to_have_text(
            re.compile(r"saved"), timeout=2_000
        )
        cfg = api_request_context.get(f"{host_url}/api/config").json()
        assert cfg["switches"]["sprinkler_1_auto_off_secs"] == 180
        assert cfg["switches"]["sprinkler_2_auto_off_secs"] == 240
    finally:
        api_request_context.put(
            f"{host_url}/api/config",
            data={
                **cfg_before,
                "switches": {
                    "sprinkler_1_auto_off_secs": s1_before,
                    "sprinkler_2_auto_off_secs": s2_before,
                },
            },
        )


def test_sprinkler_auto_off_applies_live_without_reboot(
    page: Page, host_url: str, api_request_context
):
    """Set a 5-second auto-off via /api/config, then turn on sprinkler_1
    via the dashboard toggle, and observe it auto-off within ~10s. Proves
    `replace_config` plumbs the new duration into the running TimedSwitch
    without requiring a reboot.

    Uses /api/config PUT directly to set 5 s — bypassing the (minutes-only)
    form so the test runs in seconds rather than minutes.
    """
    cfg_before = api_request_context.get(f"{host_url}/api/config").json()
    s1_before = cfg_before["switches"]["sprinkler_1_auto_off_secs"]

    try:
        # Defensive setup: a prior test may have left sprinkler_1 on.
        api_request_context.post(
            f"{host_url}/api/switch", data={"kind": "sprinkler1", "on": False}
        )
        # Tighten auto-off to 5 s without rebooting.
        api_request_context.put(
            f"{host_url}/api/config",
            data={**cfg_before, "switches": {**cfg_before["switches"], "sprinkler_1_auto_off_secs": 5}},
        )
        # Turn on via the dashboard so we exercise the toggle path too.
        page.goto(host_url)
        s1 = page.locator("#t-s1")
        expect(s1).not_to_have_class(re.compile(r"\bon\b"), timeout=4_000)
        s1.click()
        expect(s1).to_have_class(re.compile(r"\bon\b"), timeout=4_000)
        # Wait for the live auto-off — 5 s window + status poll lag (≤2 s).
        # A generous 12 s lets a sluggish device or a CI host catch up.
        expect(s1).not_to_have_class(re.compile(r"\bon\b"), timeout=12_000)
    finally:
        # Make sure the sprinkler is off and restore the original duration.
        api_request_context.post(
            f"{host_url}/api/switch", data={"kind": "sprinkler1", "on": False}
        )
        api_request_context.put(
            f"{host_url}/api/config",
            data={
                **cfg_before,
                "switches": {**cfg_before["switches"], "sprinkler_1_auto_off_secs": s1_before},
            },
        )


@pytest.mark.skipif(
    bool(os.environ.get("WC_TEST_TARGET_URL")),
    reason="WiFi save would persist a bogus network on a real device — host build only",
)
def test_wifi_save_round_trip(page: Page, host_url: str):
    """Add a new network, save, reload, and confirm it persisted.

    The default config may already have one network from `.env` build-time
    seeding, so we don't assume an empty list — we add a row and assert the
    last one round-trips.
    """
    _open_tab(page, host_url, "wifi")
    expect(page.locator("#wifi-hostname")).to_have_value("doremorwater", timeout=2_000)
    rows = page.locator("#wifi-networks .list-row")
    initial_count = rows.count()
    page.locator("#wifi-add").click()
    expect(rows).to_have_count(initial_count + 1)
    rows.last.locator('[data-bind="ssid"]').fill("home_5g")
    rows.last.locator('[data-bind="password"]').fill("hunter2")
    page.locator("#wifi-save").click()
    expect(page.locator("#wifi-msg")).to_have_text(re.compile(r"saved"), timeout=2_000)

    # Reload the tab and confirm the new network persisted as the last entry.
    page.reload()
    page.locator('nav.tabs button[data-tab="wifi"]').click()
    expect(page.locator("#wifi-hostname")).to_have_value("doremorwater", timeout=2_000)
    expect(rows).to_have_count(initial_count + 1)
    expect(
        page.locator('#wifi-networks .list-row [data-bind="ssid"]').last
    ).to_have_value("home_5g")


def test_advanced_json_editor_round_trip(page: Page, host_url: str):
    _open_tab(page, host_url, "advanced")
    expect(page.locator("#cfg-json")).to_be_visible()
    expect(page.locator("#cfg-json")).not_to_have_value("", timeout=2_000)
    raw = page.locator("#cfg-json").input_value()
    cfg = json.loads(raw)
    cfg["timezone"] = "Europe/Madrid"
    page.locator("#cfg-json").fill(json.dumps(cfg, indent=2))
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"saved"), timeout=2_000)


def test_advanced_json_editor_invalid_json_inline_error(page: Page, host_url: str):
    _open_tab(page, host_url, "advanced")
    expect(page.locator("#cfg-json")).not_to_have_value("", timeout=2_000)
    page.locator("#cfg-json").fill("{ this is not json }")
    page.locator("#cfg-save").click()
    expect(page.locator("#cfg-msg")).to_have_text(re.compile(r"invalid JSON"), timeout=1_000)


def test_advanced_diagnostics_auto_refresh_populates(page: Page, host_url: str):
    """The Advanced tab's "Diagnostics (auto-refresh)" panel must fill
    from /api/diag.

    Regression: refreshDiag() formatted every heap field unconditionally;
    a field this firmware doesn't report (largest_free_block, the tasks
    array) made .toLocaleString() throw, aborting the whole refresh and
    leaving the panel on its "—" placeholders.
    """
    _open_tab(page, host_url, "advanced")
    # diag polling starts on tab entry — the heap-free field must leave
    # its "—" placeholder and show a byte count.
    expect(page.locator("#diag-free")).not_to_have_text("—", timeout=8_000)
    expect(page.locator("#diag-free")).to_contain_text("B")
    # min-ever-free is firmware-provided too and must populate.
    expect(page.locator("#diag-min")).to_contain_text("B", timeout=8_000)
