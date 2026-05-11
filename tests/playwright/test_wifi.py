"""WiFi-tab integration tests: scan endpoint, reconnect endpoint, and the
SPA scan card. Runs against the host binary (FakeWifi)."""

from __future__ import annotations

import re

import pytest
from playwright.sync_api import APIRequestContext, expect


# --- API-level ---------------------------------------------------------------

def test_wifi_scan_endpoint_schema(host_url: str, api_request_context: APIRequestContext):
    """Endpoint always answers with a `networks: []` envelope and each
    entry has the four fields the SPA depends on. Doesn't assert on SSID
    names — those are FakeWifi stubs on the host build but real
    neighbourhood APs on a flashed device, so they're not portable."""
    r = api_request_context.get(f"{host_url}/api/wifi/scan")
    assert r.ok, r.text()
    body = r.json()
    assert "networks" in body
    for net in body["networks"]:
        assert isinstance(net["ssid"], str)
        assert isinstance(net["rssi_dbm"], int)
        assert isinstance(net["channel"], int)
        assert net["auth"] in ("open", "wep", "wpa", "wpa2", "wpa2-ent", "wpa3", "unknown")


def test_wifi_scan_endpoint_returns_fake_list(
    host_url: str, api_request_context: APIRequestContext, on_real_device: bool
):
    """Host-only: verify the FakeWifi stub data round-trips through the
    endpoint. Skipped on real hardware where the SSID list is real."""
    if on_real_device:
        pytest.skip("FakeWifi-specific SSIDs only present in the host build")
    r = api_request_context.get(f"{host_url}/api/wifi/scan")
    assert r.ok, r.text()
    body = r.json()
    ssids = {n["ssid"] for n in body["networks"]}
    assert "FakeNet-2.4G" in ssids
    assert "FakeNet-Guest" in ssids


def test_wifi_reconnect_endpoint_returns_204(host_url: str, api_request_context: APIRequestContext):
    r = api_request_context.post(f"{host_url}/api/wifi/reconnect")
    assert r.status == 204, r.text()


# --- UI-level ----------------------------------------------------------------

def test_wifi_scan_button_lists_results_and_can_pick(page, host_url: str):
    """Click Scan → at least one result row appears → clicking one
    appends a new Known-networks row pre-filled with that SSID.

    Works against both the host build (FakeWifi, 2 entries) and a real
    device (whatever the neighbourhood looks like, ≥1 entry expected
    in any lab environment)."""
    page.goto(host_url)
    page.click('button[data-tab="wifi"]')
    page.click("#wifi-scan")

    # The real device's scan takes several seconds (esp_wifi_scan_start
    # does a full active scan across all channels + the supervisor's
    # poll loop). Default 5 s is plenty for FakeWifi but not for real
    # hardware; 20 s is comfortable on both.
    expect(page.locator("#wifi-scan-msg")).to_contain_text(
        re.compile(r"\d+ network"), timeout=20_000
    )
    rows = page.locator("#wifi-scan-results .list-row")
    # ≥1 row on any reasonable network. Use to_have_count(N, timeout=…)
    # implicitly via a wait_for. The .first locator below also waits.
    first_row = rows.first
    expect(first_row).to_be_visible()

    # Click the first (strongest, since we sort by RSSI in the SPA) entry —
    # a new Known-networks row should appear with whatever SSID was on it.
    first_ssid = first_row.locator(".field").first.locator("div").last.text_content()
    before = page.locator("#wifi-networks .list-row").count()
    first_row.click()
    page.wait_for_function(
        f'document.querySelectorAll("#wifi-networks .list-row").length === {before + 1}'
    )
    added = page.locator("#wifi-networks .list-row").last
    assert added.locator('[data-bind="ssid"]').input_value() == first_ssid
    assert added.locator('[data-bind="password"]').input_value() == ""


def test_wifi_reconnect_button_calls_endpoint(page, host_url: str):
    page.goto(host_url)
    page.click('button[data-tab="wifi"]')

    # Watch the network for the reconnect POST.
    with page.expect_response(lambda r: r.url.endswith("/api/wifi/reconnect")) as info:
        page.click("#wifi-reconnect")
    resp = info.value
    assert resp.request.method == "POST"
    assert resp.status == 204
    expect(page.locator("#wifi-msg")).to_contain_text("reconnect signaled")
