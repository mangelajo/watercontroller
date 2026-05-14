"""Webhook dispatch end-to-end.

Spins up an in-process HTTP mock server that captures every POST/PUT it
receives, then:

1. Configures a webhook on the device (host or real) via
   `PUT /api/config/webhooks` pointing at the mock URL.
2. Triggers each event we want to verify via the serial console
   (`webhook fire <event>`) or, on the host build, via the equivalent
   `POST /api/webhooks/test` endpoint (the host has no serial).
3. Asserts the mock saw the call with the right method, body
   (template-substituted), and custom headers.

The mock binds to 127.0.0.1:<free port>. On the host build the
dispatcher is in-process — it reaches the mock trivially. The real
device path is skipped here (the device is on a different subnet and
can't reach this container) and is left to manual / lab-time tests.
"""

from __future__ import annotations

import json
import socket
import threading
import time
import urllib.request
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest


def _free_port() -> int:
    with closing(socket.socket(socket.AF_INET, socket.SOCK_STREAM)) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class CapturingHandler(BaseHTTPRequestHandler):
    # Filled in by the server wrapper before serve_forever; one shared
    # list across requests because ThreadingHTTPServer spawns a thread
    # per connection.
    captured: list = []  # type: ignore[type-arg]

    def _handle(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length).decode("utf-8") if length else ""
        self.__class__.captured.append({
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body,
            "received_at": time.time(),
        })
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self) -> None:
        self._handle()

    def do_PUT(self) -> None:
        self._handle()

    def log_message(self, *_a, **_kw) -> None:  # silence stderr noise
        pass


@pytest.fixture
def mock_server():
    """Start an HTTP server on 127.0.0.1:<free_port> that captures
    every POST/PUT into `server.captured`. Stopped at teardown."""
    port = _free_port()
    # Fresh handler subclass per test so captures don't leak.
    handler = type("H", (CapturingHandler,), {"captured": []})
    server = ThreadingHTTPServer(("127.0.0.1", port), handler)
    server.captured = handler.captured  # type: ignore[attr-defined]
    server.base_url = f"http://127.0.0.1:{port}"  # type: ignore[attr-defined]
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()


def _put_webhooks(base: str, body: list) -> None:
    req = urllib.request.Request(
        f"{base}/api/config/webhooks",
        method="PUT",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        assert r.status in (200, 204), f"PUT /api/config/webhooks → {r.status}"


def _fire(base: str, kind: str, vars_: dict | None = None) -> None:
    req = urllib.request.Request(
        f"{base}/api/webhooks/test",
        method="POST",
        data=json.dumps({"kind": kind, "vars": vars_ or {}}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        assert r.status in (200, 202, 204), f"POST /api/webhooks/test → {r.status}"


def _wait_for_captures(server, n: int, timeout_s: float = 5.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if len(server.captured) >= n:
            return
        time.sleep(0.05)
    raise AssertionError(
        f"expected {n} captured webhook call(s), got {len(server.captured)}: {server.captured}"
    )


def test_dispatches_flow_alarm_fire_with_substituted_body(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [{
        "enabled": True,
        "url": f"{mock_server.base_url}/alarm",
        "kind": "generic",
        "events": ["flow_alarm.fire"],
        "method": "POST",
        "headers": [],
        "body_template": '{"event":"{{event}}","flow":{{flow_lph}},"device":"{{device}}"}',
    }]
    _put_webhooks(host_url, cfg)
    _fire(host_url, "flow_alarm.fire", {"flow_lph": "250.0"})
    _wait_for_captures(mock_server, 1)
    fires = [c for c in mock_server.captured if c["path"] == "/alarm"]
    assert len(fires) == 1, fires
    body = json.loads(fires[0]["body"])
    assert body["event"] == "flow_alarm.fire"
    assert body["flow"] == 250.0
    assert isinstance(body["device"], str)
    assert fires[0]["method"] == "POST"
    # Default Content-Type when caller didn't set one.
    assert fires[0]["headers"]["content-type"] == "application/json"


def test_custom_headers_propagate(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [{
        "enabled": True,
        "url": f"{mock_server.base_url}/h",
        "kind": "generic",
        "events": ["boot"],
        "method": "POST",
        "headers": [
            {"name": "Authorization", "value": "Bearer test-token-XYZ"},
            {"name": "X-Source", "value": "watercontroller-test"},
            {"name": "Content-Type", "value": "application/vnd.custom+json"},
        ],
        "body_template": '{"event":"{{event}}"}',
    }]
    _put_webhooks(host_url, cfg)
    _fire(host_url, "boot", {})
    _wait_for_captures(mock_server, 1)
    cap = [c for c in mock_server.captured if c["path"] == "/h"][0]
    assert cap["headers"].get("authorization") == "Bearer test-token-XYZ"
    assert cap["headers"].get("x-source") == "watercontroller-test"
    # Custom Content-Type overrides the JSON default.
    assert cap["headers"].get("content-type") == "application/vnd.custom+json"


def test_subscriber_filter_drops_unsubscribed_events(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [{
        "enabled": True,
        "url": f"{mock_server.base_url}/only_fire",
        "kind": "generic",
        "events": ["flow_alarm.fire"],  # NOT boot
        "method": "POST",
        "headers": [],
        "body_template": "{}",
    }]
    _put_webhooks(host_url, cfg)
    _fire(host_url, "boot", {})       # should NOT deliver
    _fire(host_url, "flow_alarm.fire", {})  # should deliver
    _wait_for_captures(mock_server, 1)
    time.sleep(0.5)  # let any spurious extra delivery land before asserting
    only_fire = [c for c in mock_server.captured if c["path"] == "/only_fire"]
    assert len(only_fire) == 1


def test_disabled_webhook_does_not_fire(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [{
        "enabled": False,  # ← disabled
        "url": f"{mock_server.base_url}/never",
        "kind": "generic",
        "events": ["flow_alarm.fire"],
        "method": "POST",
        "headers": [],
        "body_template": "{}",
    }]
    _put_webhooks(host_url, cfg)
    _fire(host_url, "flow_alarm.fire", {})
    time.sleep(0.7)
    assert all(c["path"] != "/never" for c in mock_server.captured), mock_server.captured


def test_multiple_webhooks_both_receive(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [
        {"enabled": True, "url": f"{mock_server.base_url}/a", "kind": "generic",
         "events": ["flow_alarm.fire"], "method": "POST", "headers": [],
         "body_template": '{"who":"a"}'},
        {"enabled": True, "url": f"{mock_server.base_url}/b", "kind": "generic",
         "events": ["flow_alarm.fire"], "method": "POST", "headers": [],
         "body_template": '{"who":"b"}'},
    ]
    _put_webhooks(host_url, cfg)
    _fire(host_url, "flow_alarm.fire", {})
    _wait_for_captures(mock_server, 2)
    paths = sorted(c["path"] for c in mock_server.captured)
    assert paths == ["/a", "/b"], paths


def test_put_method_supported(host_url, mock_server, on_real_device):
    if on_real_device:
        pytest.skip("real device can't reach the mock server (different subnet)")
    cfg = [{
        "enabled": True,
        "url": f"{mock_server.base_url}/put",
        "kind": "generic",
        "events": ["config.changed"],
        "method": "PUT",
        "headers": [],
        "body_template": '{"section":"{{section}}"}',
    }]
    _put_webhooks(host_url, cfg)
    # PUT above itself triggers config.changed which should land here.
    _wait_for_captures(mock_server, 1)
    cap = [c for c in mock_server.captured if c["path"] == "/put"][0]
    assert cap["method"] == "PUT"
    body = json.loads(cap["body"])
    assert body["section"] == "webhooks"


# ---------- UI tests ----------------------------------------------------
# These drive the Webhooks tab via Playwright. The fixtures here are the
# standard ones from conftest.py (page, host_url).


def test_ui_webhooks_tab_renders(host_url, page, on_real_device):
    """The Webhooks tab exists in the nav and renders the test-fire
    section. Smoke check that the SPA bundle includes the new tab."""
    if on_real_device:
        pytest.skip("real device may have user-configured webhooks; UI test mutates state")
    page.goto(host_url)
    tab_btn = page.locator('nav.tabs button[data-tab="webhooks"]')
    from playwright.sync_api import expect
    expect(tab_btn).to_be_visible()
    tab_btn.click()
    expect(page.locator("#webhooks-test-fire")).to_be_visible()
    expect(page.locator("#webhooks-add")).to_be_visible()
    # Empty-state hint should appear when no webhooks are configured.
    # We clear via the API first to ensure a known state.
    page.request.put(
        f"{host_url}/api/config/webhooks",
        data=json.dumps([]),
        headers={"Content-Type": "application/json"},
    )
    # Re-enter the tab to refetch.
    page.locator('nav.tabs button[data-tab="dashboard"]').click()
    page.locator('nav.tabs button[data-tab="webhooks"]').click()
    expect(page.locator("#webhooks-list")).to_contain_text("No webhooks configured")


def test_ui_add_and_save_webhook_persists(host_url, page, mock_server, on_real_device):
    """Add a webhook through the UI, save it, then verify GET
    /api/config/webhooks reflects what the form sent."""
    if on_real_device:
        pytest.skip("real device can't reach mock_server")
    # Start clean.
    page.request.put(
        f"{host_url}/api/config/webhooks",
        data="[]",
        headers={"Content-Type": "application/json"},
    )
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="webhooks"]').click()
    page.locator("#webhooks-add").click()
    # The card for index 0 should now exist.
    page.locator('input[data-wh-url="0"]').fill(f"{mock_server.base_url}/from_ui")
    # Toggle: subscribe to boot (was preset to flow_alarm.fire only).
    page.locator('input[data-wh-events="0"][value="boot"]').check()
    page.locator('textarea[data-wh-body="0"]').fill('{"src":"ui-test","ev":"{{event}}"}')
    page.locator("#webhooks-save").click()
    from playwright.sync_api import expect
    expect(page.locator("#webhooks-msg")).to_contain_text("saved", timeout=5000)
    saved = page.request.get(f"{host_url}/api/config/webhooks").json()
    assert len(saved) == 1
    assert saved[0]["url"].endswith("/from_ui")
    assert "boot" in saved[0]["events"]
    assert "flow_alarm.fire" in saved[0]["events"]
    assert "ui-test" in saved[0]["body_template"]


def test_ui_preset_dropdown_updates_body_template(host_url, page, on_real_device):
    """Picking a preset from the kind dropdown should replace the
    body template with the preset's defaults. Confirm dialog only
    fires when the current body is already customised — for an
    empty/default body the swap is automatic."""
    if on_real_device:
        pytest.skip("real device may have user-configured webhooks; UI test mutates state")
    # Clear + open the tab.
    page.request.put(
        f"{host_url}/api/config/webhooks",
        data="[]",
        headers={"Content-Type": "application/json"},
    )
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="webhooks"]').click()
    page.locator("#webhooks-add").click()

    body_el = page.locator('textarea[data-wh-body="0"]')
    kind_sel = page.locator('select[data-wh-kind="0"]')

    # NOTE: for <textarea> the live JS-set value lives on the `.value`
    # property, not textContent — so `to_have_value` is what we want,
    # NOT `to_contain_text` (which reads textContent and stays on the
    # initial HTML attribute regardless of JS edits).
    from playwright.sync_api import expect

    # The Add button installs the generic preset; verify before
    # changing kind so a slow render can't race.
    expect(body_el).to_have_value(__import__("re").compile(r"event_label"))

    # Pick Slack — no confirm needed because the original body is
    # a known preset.
    kind_sel.select_option("slack")
    expect(body_el).to_have_value(__import__("re").compile(r'"text"'))
    # Slack body has no uptime_s placeholder.
    assert "uptime_s" not in body_el.input_value()

    # Discord swap
    kind_sel.select_option("discord")
    expect(body_el).to_have_value(__import__("re").compile(r'"content"'))

    # Customise the body, then switching presets should prompt
    # before overwriting. Auto-accept the confirm.
    body_el.fill('{"custom":"do-not-lose-this"}')
    page.on("dialog", lambda d: d.accept())
    kind_sel.select_option("generic")
    expect(body_el).to_have_value(__import__("re").compile(r"event_label"))


def test_ui_test_fire_triggers_dispatch(host_url, page, mock_server, on_real_device):
    """Hitting the Test-fire button with a configured webhook causes
    the mock server to receive the rendered body."""
    if on_real_device:
        pytest.skip("real device can't reach mock_server")
    # Pre-configure a webhook via the API so the UI test focuses on the
    # button rather than the add+save dance.
    page.request.put(
        f"{host_url}/api/config/webhooks",
        data=json.dumps([{
            "enabled": True,
            "url": f"{mock_server.base_url}/from_ui_test",
            "kind": "generic",
            "events": ["flow_alarm.fire"],
            "method": "POST",
            "headers": [{"name": "X-UI-Test", "value": "yes"}],
            "body_template": '{"event":"{{event}}","flow":"{{flow_lph}}"}',
        }]),
        headers={"Content-Type": "application/json"},
    )
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="webhooks"]').click()
    page.locator("#webhooks-test-kind").select_option("flow_alarm.fire")
    page.locator("#webhooks-test-fire").click()
    from playwright.sync_api import expect
    expect(page.locator("#webhooks-test-msg")).to_contain_text("emitted", timeout=5000)
    _wait_for_captures(mock_server, 1)
    cap = next(c for c in mock_server.captured if c["path"] == "/from_ui_test")
    assert cap["headers"].get("x-ui-test") == "yes"
    body = json.loads(cap["body"])
    assert body["event"] == "flow_alarm.fire"
    assert body["flow"] == "999"  # default vars from the SPA's fire path
