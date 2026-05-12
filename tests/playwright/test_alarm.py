"""Flow-alarm tests against the host build.

Covers the new /api/config/flow_alarm section, the /api/alarm/clear
endpoint, and the SPA card on the Sprinklers tab. The actual alarm
state-machine logic is exercised in core unit tests; here we just
make sure the HTTP + SPA wiring round-trips correctly.
"""

from __future__ import annotations

from playwright.sync_api import APIRequestContext, expect


def test_flow_alarm_config_round_trip(host_url: str, api_request_context: APIRequestContext):
    # Read defaults.
    r = api_request_context.get(f"{host_url}/api/config/flow_alarm")
    assert r.ok, r.text()
    initial = r.json()
    assert "enabled" in initial and "threshold_lph" in initial and "duration_secs" in initial

    # Write new values.
    new = {"enabled": True, "threshold_lph": 250.0, "duration_secs": 30}
    put = api_request_context.put(f"{host_url}/api/config/flow_alarm", data=new)
    assert put.ok or put.status == 204, put.text()

    # Read back — must reflect the write.
    again = api_request_context.get(f"{host_url}/api/config/flow_alarm").json()
    assert again["enabled"] is True
    assert again["threshold_lph"] == 250.0
    assert again["duration_secs"] == 30


def test_alarm_clear_endpoint_returns_204(host_url: str, api_request_context: APIRequestContext):
    r = api_request_context.post(f"{host_url}/api/alarm/clear")
    assert r.status == 204, r.text()


def test_alarm_card_round_trips_through_ui(
    page, host_url: str, api_request_context: APIRequestContext
):
    """Drive the alarm card end-to-end. Bypass the checkbox-click race
    (SPA's renderFlowAlarm runs asynchronously after /api/config returns
    and can clobber a Playwright click) by mutating the form values via
    JS, then clicking Save."""
    # Force a known starting state so this test is independent of order.
    api_request_context.put(
        f"{host_url}/api/config/flow_alarm",
        data={"enabled": False, "threshold_lph": 50.0, "duration_secs": 10},
    )

    page.goto(host_url)
    page.locator("#loading-modal").wait_for(state="hidden", timeout=10_000)
    page.click('button[data-tab="sprinklers"]')
    # Wait until the renderer has populated the form with the seed state
    # — otherwise our fill() can lose to a late renderFlowAlarm().
    expect(page.locator("#alarm-threshold")).to_have_value("50")

    # Set form values via JS so the renderer can't race the click.
    page.evaluate(
        """() => {
            document.getElementById('alarm-enabled').checked = true;
            document.getElementById('alarm-threshold').value = '150';
            document.getElementById('alarm-duration').value = '45';
        }"""
    )
    page.click("#alarm-save")
    expect(page.locator("#flow_alarm-msg")).to_contain_text("saved")

    # Reload — values must persist (round-trip through /api/config/flow_alarm).
    page.reload()
    page.locator("#loading-modal").wait_for(state="hidden", timeout=10_000)
    page.click('button[data-tab="sprinklers"]')
    expect(page.locator("#alarm-enabled")).to_be_checked()
    expect(page.locator("#alarm-threshold")).to_have_value("150")
    expect(page.locator("#alarm-duration")).to_have_value("45")

    # Clear button posts and flashes "alarm cleared".
    with page.expect_response(lambda r: r.url.endswith("/api/alarm/clear")) as info:
        page.click("#alarm-clear")
    assert info.value.status == 204
    expect(page.locator("#flow_alarm-msg")).to_contain_text("alarm cleared")
