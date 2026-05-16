"""Backup / restore via `/api/config?all` + Advanced-tab UI buttons.

Two sets of tests:

* **API**: `?all` returns secrets, default GET redacts. PUT round-trips
  the full payload (incl. secrets) without wiping stored values.
* **UI**: the Advanced tab's "Download config" button triggers a JSON
  file download whose body matches `GET /api/config?all`. The
  "Upload & restore" button accepts a previously-downloaded blob and
  PUTs it to `/api/config`.

The UI test uses Playwright's download interception + JS evaluation
to round-trip without actually hitting the OS file picker (which
Playwright can't drive on a headless host).
"""

from __future__ import annotations

import json

from playwright.sync_api import Page, expect


# ---------- API ---------------------------------------------------------

def test_default_get_redacts_secrets(host_url, api_request_context):
    """`GET /api/config` (no query) must hide credentials so that a SPA
    poll doesn't leak the admin token, WiFi passwords, or TLS keys to
    a casual reader."""
    r = api_request_context.get(f"{host_url}/api/config")
    assert r.ok, r.status
    cfg = r.json()
    # Wifi passwords scrubbed.
    for n in cfg.get("wifi", {}).get("networks", []):
        assert n.get("password", "") == "", n
    # admin_token scrubbed.
    assert cfg.get("admin_token", "") == ""


def test_all_query_returns_full_secrets(host_url, api_request_context):
    """`?all` (or `?all=1`) returns the full config — for the SPA's
    backup download button. On a real device this is auth-gated;
    the host build is open."""
    # Seed a recognisable secret so we can assert it round-trips.
    seed = (api_request_context.get(f"{host_url}/api/config?all=1")).json()
    seed.setdefault("wifi", {}).setdefault("networks", []).append(
        {"ssid": "_backup_test_", "password": "supersecret-XYZ"}
    )
    seed["admin_token"] = "tok-test-backup"
    r = api_request_context.put(
        f"{host_url}/api/config",
        data=json.dumps(seed),
        headers={"Content-Type": "application/json"},
    )
    assert r.ok, r.status

    # ?all returns the secret back.
    full = (api_request_context.get(f"{host_url}/api/config?all=1")).json()
    net = next(n for n in full["wifi"]["networks"] if n["ssid"] == "_backup_test_")
    assert net["password"] == "supersecret-XYZ"
    assert full["admin_token"] == "tok-test-backup"

    # Default GET still redacts the same field.
    red = (api_request_context.get(f"{host_url}/api/config")).json()
    rnet = next(n for n in red["wifi"]["networks"] if n["ssid"] == "_backup_test_")
    assert rnet["password"] == ""
    assert red["admin_token"] == ""


def test_put_full_config_roundtrips(host_url, api_request_context):
    """A backup file downloaded via `?all` should restore cleanly via
    PUT /api/config — values unchanged on the other side."""
    full_before = (api_request_context.get(f"{host_url}/api/config?all=1")).json()
    # Twiddle a non-secret field so the restore is observably different.
    full_before["timezone"] = "Europe/London"
    r = api_request_context.put(
        f"{host_url}/api/config",
        data=json.dumps(full_before),
        headers={"Content-Type": "application/json"},
    )
    assert r.ok, r.status
    full_after = (api_request_context.get(f"{host_url}/api/config?all=1")).json()
    assert full_after["timezone"] == "Europe/London"


# ---------- UI ----------------------------------------------------------

def test_advanced_download_button_triggers_json_download(
    host_url, page: Page
):
    """Clicking "Download config (with secrets)" should produce a
    download whose body parses as JSON and contains the unredacted
    admin_token. We intercept Playwright's `download` event so the
    file never actually lands on disk."""
    # Seed a recognisable token.
    page.request.put(
        f"{host_url}/api/config",
        data=json.dumps({"admin_token": "ui-download-test"}),
        headers={"Content-Type": "application/json"},
    )
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="advanced"]').click()
    expect(page.locator("#cfg-download")).to_be_visible()

    with page.expect_download() as dl_info:
        page.locator("#cfg-download").click()
    dl = dl_info.value
    body = dl.path().read_bytes().decode("utf-8")
    cfg = json.loads(body)
    assert cfg["admin_token"] == "ui-download-test"
    # Filename should be timestamped so multiple backups don't collide.
    assert dl.suggested_filename.startswith("doremorwater-config-")
    assert dl.suggested_filename.endswith(".json")


def test_advanced_upload_button_restores_config(host_url, page: Page):
    """The upload button (hidden file input + accept dialog) restores
    a previously-saved JSON blob via PUT /api/config. We bypass the
    OS file picker by directly setting `.files` on the input element."""
    # Set baseline.
    page.request.put(
        f"{host_url}/api/config",
        data=json.dumps({"timezone": "Europe/Madrid"}),
        headers={"Content-Type": "application/json"},
    )
    page.goto(host_url)
    page.locator('nav.tabs button[data-tab="advanced"]').click()

    # Prepare a synthetic backup that changes timezone.
    full = page.request.get(f"{host_url}/api/config?all=1").json()
    full["timezone"] = "America/New_York"
    blob = json.dumps(full)

    # The button click triggers a `confirm()` — auto-accept.
    page.on("dialog", lambda d: d.accept())
    # Inject the file directly into the hidden <input type="file">.
    page.locator("#cfg-upload-input").set_input_files({
        "name": "backup.json",
        "mimeType": "application/json",
        "buffer": blob.encode(),
    })

    expect(page.locator("#cfg-backup-msg")).to_contain_text("restored", timeout=5000)
    # Server should reflect the new timezone.
    cfg = page.request.get(f"{host_url}/api/config").json()
    assert cfg["timezone"] == "America/New_York"


# ---------- real-device note --------------------------------------------
# These tests mutate the persisted config. They are safe to run against
# real hardware because conftest's `_clean_device_config` session
# teardown erases the NVS config afterwards (serial `config reset`, or
# POST /api/factory_reset) — the board is left on compile-time defaults.
