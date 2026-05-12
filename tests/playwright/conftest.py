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
DEVICE_BOOT_TIMEOUT_S = float(os.environ.get("WC_DEVICE_BOOT_TIMEOUT_S", "45"))
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


def _reset_and_detect_ip(client, console, log) -> str:
    """Reset the board and return the DHCP-assigned IP read from the
    boot serial output. Assumes the firmware is already flashed — the
    `device-test` make target handles the (one-shot) flash up front.

    `console` is the long-lived session pexpect adapter; reusing it
    keeps the serial-output mirroring (logfile_read) continuous from
    session start. `log` writes progress lines to the live terminal.
    """
    log("device-setup: resetting, waiting for sta ip…")
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

    Pytest's default `--capture=fd` redirects fds 1 and 2 at OS level,
    which catches `sys.stderr`, `sys.__stderr__`, AND `print()` alike.
    The only reliable bypass on POSIX is opening `/dev/tty` directly —
    that's a fresh fd to the controlling terminal, untouched by
    pytest's redirect. We fall back to `sys.__stderr__` for non-tty
    environments (CI logs, redirected output) so the messages are at
    least preserved in the captured stream."""
    tty = None
    try:
        tty = open("/dev/tty", "w", buffering=1)  # line-buffered
    except OSError:
        tty = None

    def write(msg: str) -> None:
        # Always dump to __stderr__ too — captured by pytest but kept in
        # the "Captured stderr" section of test reports and visible via
        # `pytest -s`. /dev/tty handles the live-terminal case.
        if tty is not None:
            try:
                tty.write(msg + "\n")
                tty.flush()
            except OSError:
                pass
        print(msg, file=sys.__stderr__, flush=True)

    return write


@pytest.fixture(scope="session")
def term(pytestconfig):
    """Expose the terminal writer to tests/fixtures that want to surface
    progress (e.g. each `>>` command/response in the serial CLI suite)."""
    return _terminal_writer(pytestconfig)


def _fetch_diag(url: str, timeout: float = 3.0) -> dict | None:
    """Cheap stdlib-only GET of /api/diag. Returns parsed JSON or None on
    error — never raises, so a missed poll doesn't fail an unrelated test."""
    import json
    import urllib.request

    try:
        with urllib.request.urlopen(f"{url}/api/diag", timeout=timeout) as r:
            return json.loads(r.read())
    except Exception:
        return None


def _format_diag(d: dict, tag: str) -> str:
    heap = d.get("heap", {})
    tasks = {t["name"]: t.get("stack_min_free_bytes") for t in d.get("tasks", [])}
    # Surface the tasks whose stacks we tune most often. `?` keeps the
    # column count stable when a task name moves or isn't present (e.g.
    # we're running an older build).
    def hwm(name: str) -> str:
        v = tasks.get(name)
        return f"{v:,}" if isinstance(v, int) else "?"

    free = heap.get("total_free_bytes", 0)
    minfree = heap.get("min_ever_free_bytes", 0)
    return (
        f"diag[{tag}]: heap free={free:,} min={minfree:,} | "
        f"hwm wifi-sup={hwm('wifi-sup')} "
        f"serial-cli={hwm('serial-cli')} "
        f"httpd={hwm('httpd')} "
        f"sys_evt={hwm('sys_evt')}"
    )


@pytest.fixture(autouse=True)
def _post_test_diag(request, real_target_url, term):
    """After each test, fetch /api/diag and print a one-line heap +
    stack-HWM summary. Skipped silently when we're not running against
    a real device, or when /api/diag isn't reachable (a crashed device
    shouldn't make every test report an extra failure — the test that
    actually broke things will show the cause)."""
    yield
    if not real_target_url:
        return
    d = _fetch_diag(real_target_url)
    if d is None:
        term(f"diag[{request.node.name}]: device unreachable for diag poll")
        return
    term(_format_diag(d, request.node.name))


@pytest.fixture(scope="session")
def on_real_device(real_target_url) -> bool:
    """`True` when tests are running against a flashed ESP32 (either via
    `JUMPSTARTER_HOST` or an explicit `WC_TEST_TARGET_URL`). Used by
    tests that assert on host-only behaviour like FakeWifi's stub
    network list, so they can adapt or skip on real hardware."""
    return real_target_url is not None


@pytest.fixture(scope="session")
def device_console(jumpstarter_client, term):
    """Long-lived pexpect adapter over the device's UART, live for the
    whole test session when JUMPSTARTER_HOST is set, else `None`.

    Mirrors every byte the device emits to `sys.__stderr__` so the
    serial trail is visible live under `pytest -s` and lands in
    pytest's "Captured stderr" section on a failure — same pattern as
    the jumpstarter-dev soc-pytest example (`console.logfile_read =
    sys.stdout.buffer`). Without this you have to re-attach
    `j serial pipe` by hand to diagnose what the firmware was saying
    when a test failed.

    Reused by `real_target_url` (for the boot `sta ip:` probe) and by
    the serial-CLI tests (for `sendline` / `expect`). Since the lab
    gRPC layer holds the FTDI flock exclusively, sharing a single
    adapter is the only way to keep serial captured across phases.
    """
    if jumpstarter_client is None:
        yield None
        return
    from jumpstarter_driver_network.adapters import PexpectAdapter
    term("serial: attaching session console (mirror → stderr)")
    with PexpectAdapter(client=jumpstarter_client.serial) as console:
        # pexpect spawns expose `logfile_read` — every byte consumed by
        # the matcher is also written here. We use the real stderr fd
        # (sys.__stderr__) so pytest's per-test fd-capture picks it up
        # for failure reports; under `pytest -s` it just flows straight
        # through to the user's terminal.
        try:
            console.logfile_read = sys.__stderr__.buffer
        except Exception:
            # If the underlying stream doesn't have a `.buffer` (rare
            # — happens under some IDE runners) fall back to silent.
            pass
        yield console
    term("serial: session console detached")


@pytest.fixture(scope="session")
def real_target_url(jumpstarter_client, device_console, term) -> str | None:
    """Resolve where the test session should run:

    1. `WC_TEST_TARGET_URL` — point at an already-running device.
    2. `JUMPSTARTER_HOST` — reset the (already-flashed) board and
       detect the IP from boot serial. Flashing is done up front by
       the `device-test` make target — pytest doesn't reflash on
       every session.
    3. neither — return None and let `host_url` spawn the host binary.
    """
    if env_url := os.environ.get("WC_TEST_TARGET_URL"):
        term(f"target: using WC_TEST_TARGET_URL={env_url}")
        return env_url.rstrip("/")
    if jumpstarter_client is not None:
        assert device_console is not None
        ip = _reset_and_detect_ip(jumpstarter_client, device_console, term)
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
