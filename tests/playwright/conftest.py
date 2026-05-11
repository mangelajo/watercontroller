"""
End-to-end UI tests for the watercontroller SPA.

Three modes, picked automatically by `real_target_url`:

* `WC_TEST_TARGET_URL=http://x.y.z.w` — run all tests against an already
  running device at that URL.
* `JUMPSTARTER_HOST` set (i.e. we're inside an active `jmp shell -l
  target=esp32`) — flash `target/firmware/app.bin`, reset the board,
  and parse the DHCP-assigned IP out of the boot serial output once
  per session. All driver interactions go through `jumpstarter.utils.
  env()` and `PexpectAdapter`; nothing here shells out to `j` or kills
  external `j serial pipe` consumers — if the serial port is already
  held, the flash / pexpect calls will surface a clear error.
* Neither set — spawn the local host binary (the original default,
  used by `make ui-tests`).
"""

from __future__ import annotations

import os
import re
import socket
import subprocess
import sys
import time
from contextlib import closing
from pathlib import Path

import pytest
from playwright.sync_api import APIRequestContext, Playwright


REPO_ROOT = Path(__file__).resolve().parents[2]
APP_BIN = REPO_ROOT / "target/firmware/app.bin"
DEVICE_BOOT_TIMEOUT_S = float(os.environ.get("WC_DEVICE_BOOT_TIMEOUT_S", "45"))
# `partitions.csv`: otadata sits at 0xf000, size 0x2000. Writing it to
# all-1s wipes the OTA selector so the bootloader falls back to ota_0
# (which is where we serial-flash). Without this, a device that
# previously OTA'd into ota_1 keeps booting the stale ota_1 image even
# after we drop a fresh build into ota_0.
_OTADATA_OFFSET = "0xf000"
_OTADATA_SIZE = 0x2000
# Pattern esp_netif prints to UART once DHCP lands. The supervisor's
# follow-up `wifi: connected to <ssid> (<ip>)` line is also a fine
# anchor; we pick the lower-level one because it appears before the
# Rust logger initialises and so works even if log-level filtering
# changes later.
_STA_IP_PATTERN = re.compile(rb"sta ip: (\d+\.\d+\.\d+\.\d+)")


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


# ----------------------------------------------------------------------------
# Jumpstarter client (session-scoped)
# ----------------------------------------------------------------------------

@pytest.fixture(scope="session")
def jumpstarter_client(term):
    """Yield a connected jumpstarter `client` when `JUMPSTARTER_HOST` is set,
    else `None`. The fixture is intentionally permissive: most host-only
    tests don't need a device, so we let them run by yielding None.

    Inside `jmp shell -l target=esp32`, `JUMPSTARTER_HOST` points at the
    lease's gRPC socket and `env()` connects without re-leasing.
    """
    if not os.environ.get("JUMPSTARTER_HOST"):
        yield None
        return
    try:
        from jumpstarter.common.utils import env
    except ImportError as e:
        pytest.skip(f"jumpstarter not importable in this venv: {e}")
        return
    term("jumpstarter: connecting via JUMPSTARTER_HOST…")
    with env() as client:
        term(f"jumpstarter: connected — drivers: {sorted(getattr(client, 'children', {}).keys())}")
        yield client
        term("jumpstarter: releasing client")


def _flash_and_detect_ip(client, log) -> str:
    """Flash `target/firmware/app.bin`, reset the board, and return the
    DHCP-assigned IP read from the boot serial output. All driver calls
    go through the jumpstarter client — no subprocesses, no port killing.

    `log` is a callable taking a single string; progress messages flow
    through it so the user sees what's happening during the 30+ s of
    setup (otherwise pytest's first test appears to hang).
    """
    from jumpstarter_driver_network.adapters import PexpectAdapter

    if not APP_BIN.exists():
        raise RuntimeError(
            f"{APP_BIN} not found — run `make app-image` "
            "(or `make device-test` which does it for you)."
        )

    # Reset the OTA selector before flashing the app slot. Otherwise a
    # device that previously OTA'd into ota_1 keeps booting the stale
    # ota_1 image, even when we just dropped a fresh build into ota_0.
    blank = REPO_ROOT / "target/firmware/otadata_blank.bin"
    blank.parent.mkdir(parents=True, exist_ok=True)
    blank.write_bytes(b"\xff" * _OTADATA_SIZE)
    log(f"device-setup: wiping otadata @ {_OTADATA_OFFSET}")
    client.esp32.flash(str(blank), target=_OTADATA_OFFSET)

    log(f"device-setup: flashing {APP_BIN.name} @ 0x20000 ({APP_BIN.stat().st_size:,} B)")
    client.esp32.flash(str(APP_BIN), target="0x20000")

    log("device-setup: attaching console, resetting, waiting for sta ip…")
    with PexpectAdapter(client=client.serial) as console:
        client.esp32.hard_reset()
        console.expect(_STA_IP_PATTERN, timeout=DEVICE_BOOT_TIMEOUT_S)
        ip = console.match.group(1)
        if isinstance(ip, bytes):
            ip = ip.decode()
        log(f"device-setup: device IP {ip}")
        return ip


# ----------------------------------------------------------------------------
# Target URL resolution
# ----------------------------------------------------------------------------

def _terminal_writer(_config=None):
    """Return a callable that prints a progress line to the actual
    terminal regardless of pytest's output capture state.

    Why bypass `terminalreporter`: it routes through the captured
    stdout (and depending on pytest's version the line ends up
    buffered until the surrounding test finishes), so a 30 s flash +
    boot phase looks like a frozen prompt. Writing directly to
    `sys.__stderr__` is unbuffered and unaffected by capture, so the
    user sees `device-setup: …` and `cli: …` lines live."""
    def write(msg: str) -> None:
        print(msg, file=sys.__stderr__, flush=True)
    return write


@pytest.fixture(scope="session")
def term(pytestconfig):
    """Expose the terminal writer to tests/fixtures that want to surface
    progress (e.g. each `>>` command/response in the serial CLI suite)."""
    return _terminal_writer(pytestconfig)


@pytest.fixture(scope="session")
def on_real_device(real_target_url) -> bool:
    """`True` when tests are running against a flashed ESP32 (either via
    `JUMPSTARTER_HOST` or an explicit `WC_TEST_TARGET_URL`). Used by
    tests that assert on host-only behaviour like FakeWifi's stub
    network list, so they can adapt or skip on real hardware."""
    return real_target_url is not None


@pytest.fixture(scope="session")
def real_target_url(jumpstarter_client, term) -> str | None:
    """Resolve where the test session should run:

    1. `WC_TEST_TARGET_URL` — point at an already-running device.
    2. `JUMPSTARTER_HOST` — flash + boot + detect IP (done once).
    3. neither — return None and let `host_url` spawn the host binary.
    """
    if env_url := os.environ.get("WC_TEST_TARGET_URL"):
        term(f"target: using WC_TEST_TARGET_URL={env_url}")
        return env_url.rstrip("/")
    if jumpstarter_client is not None:
        ip = _flash_and_detect_ip(jumpstarter_client, term)
        return f"http://{ip}"
    term("target: no device — falling back to local host binary")
    return None


@pytest.fixture(scope="session", autouse=True)
def _device_session_setup(real_target_url):
    """Force the flash + IP-detect to run before any test, not lazily when
    the first test happens to request `host_url`. With this autouse hook
    the user sees `device-setup: …` progress lines immediately after
    collection, instead of staring at a frozen `test_dashboard_loads` for
    half a minute."""
    return real_target_url


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
    """Yield a base URL for the SPA. Either the local host binary or the
    real device URL resolved above."""
    if real_target_url:
        yield real_target_url
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
