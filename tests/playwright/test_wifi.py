"""WiFi-tab integration tests: scan endpoint, reconnect endpoint, and the
SPA scan card. Runs against the host binary (FakeWifi)."""

from __future__ import annotations

import re

from playwright.sync_api import APIRequestContext, expect


# --- API-level ---------------------------------------------------------------

def test_wifi_scan_endpoint_returns_fake_list(host_url: str, api_request_context: APIRequestContext):
    r = api_request_context.get(f"{host_url}/api/wifi/scan")
    assert r.ok, r.text()
    body = r.json()
    assert "networks" in body
    ssids = {n["ssid"] for n in body["networks"]}
    assert "FakeNet-2.4G" in ssids
    assert "FakeNet-Guest" in ssids
    # Schema sanity — each entry must have the four fields the SPA depends on.
    for net in body["networks"]:
        assert isinstance(net["rssi_dbm"], int)
        assert isinstance(net["channel"], int)
        assert net["auth"] in ("open", "wep", "wpa", "wpa2", "wpa2-ent", "wpa3", "unknown")


def test_wifi_reconnect_endpoint_returns_204(host_url: str, api_request_context: APIRequestContext):
    r = api_request_context.post(f"{host_url}/api/wifi/reconnect")
    assert r.status == 204, r.text()


# --- UI-level ----------------------------------------------------------------

def test_wifi_scan_button_lists_results_and_can_pick(page, host_url: str):
    """Click Scan → result rows appear → clicking one appends a new
    Known-networks row pre-filled with that SSID."""
    page.goto(host_url)
    page.click('button[data-tab="wifi"]')
    page.click("#wifi-scan")

    # Result list populates with our two FakeWifi entries (ordered by RSSI).
    expect(page.locator("#wifi-scan-msg")).to_contain_text(re.compile(r"\d+ network"))
    rows = page.locator("#wifi-scan-results .list-row")
    expect(rows).to_have_count(2)
    expect(rows.first).to_contain_text("FakeNet-2.4G")

    # Click the strongest entry — a new Known-networks row should appear
    # with SSID = FakeNet-2.4G and an empty password.
    before = page.locator("#wifi-networks .list-row").count()
    rows.first.click()
    page.wait_for_function(
        f'document.querySelectorAll("#wifi-networks .list-row").length === {before + 1}'
    )
    added = page.locator("#wifi-networks .list-row").last
    assert added.locator('[data-bind="ssid"]').input_value() == "FakeNet-2.4G"
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
