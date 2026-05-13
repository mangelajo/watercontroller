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
