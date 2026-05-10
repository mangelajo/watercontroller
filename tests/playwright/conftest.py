"""
End-to-end UI tests for the watercontroller SPA.

These tests drive the SPA against the **host build** (`crates/host`), which
serves the same HTML and HTTP API as the firmware. Hardware behavior
(real WiFi, MQTT, sensor reads) is out of scope here — those tests live on
the device. What's covered here:

    * the SPA loads and the dashboard renders
    * switch toggles round-trip through the JSON API
    * the Settings tab loads, saves, and surfaces validation errors
    * factory reset and OTA buttons exist and are wired up

Conftest fixtures:

* `host_binary` — path to the `host` cargo bin; built on first use of
  the session (`cargo build --bin host`).
* `host_url` — session-scoped fixture that spawns the host binary on a
  free port and tears it down at the end of the test session.
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
from contextlib import closing
from pathlib import Path

import pytest
from playwright.sync_api import APIRequestContext, Playwright


REPO_ROOT = Path(__file__).resolve().parents[2]


def _free_port() -> int:
    with closing(socket.socket(socket.AF_INET, socket.SOCK_STREAM)) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_until_listening(host: str, port: int, timeout_s: float = 30.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        with closing(socket.socket(socket.AF_INET, socket.SOCK_STREAM)) as s:
            s.settimeout(0.5)
            try:
                s.connect((host, port))
                return
            except OSError:
                time.sleep(0.2)
    raise RuntimeError(f"host binary did not start listening on {host}:{port} within {timeout_s}s")


@pytest.fixture(scope="session")
def real_target_url() -> str | None:
    """If `WC_TEST_TARGET_URL` is set, run tests against that URL (a real
    device, typically) instead of spawning the local host binary."""
    return os.environ.get("WC_TEST_TARGET_URL")


@pytest.fixture(scope="session")
def host_binary(real_target_url) -> Path | None:
    """Build and locate the host binary. Skipped when targeting a real device."""
    if real_target_url:
        return None
    subprocess.check_call(
        ["cargo", "build", "--bin", "host"],
        cwd=REPO_ROOT,
        env={**os.environ, "CARGO_TARGET_DIR": str(REPO_ROOT / "target")},
    )
    binary = REPO_ROOT / "target" / "debug" / "host"
    assert binary.exists(), f"missing {binary}"
    return binary


@pytest.fixture(scope="session")
def host_url(host_binary: Path | None, real_target_url: str | None):
    """Yield a base URL for the SPA. Either:
      * the local host binary (default), or
      * `WC_TEST_TARGET_URL` when set (e.g. http://192.168.1.151)."""
    if real_target_url:
        yield real_target_url.rstrip("/")
        return
    port = _free_port()
    bind = f"127.0.0.1:{port}"
    env = {**os.environ, "WC_HOST_BIND": bind, "RUST_LOG": "warn"}
    proc = subprocess.Popen(
        [str(host_binary)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    try:
        _wait_until_listening("127.0.0.1", port)
        yield f"http://{bind}"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


@pytest.fixture
def api_request_context(playwright: Playwright) -> APIRequestContext:
    """A Playwright APIRequestContext for talking to /api/* directly without
    a browser page — used for setup/teardown of config and switch state."""
    ctx = playwright.request.new_context()
    yield ctx
    ctx.dispose()
