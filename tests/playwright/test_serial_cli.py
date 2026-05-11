"""Serial-CLI smoke test against a real ESP32.

Pattern follows jumpstarter-dev's soc-pytest example: a long-lived
`PexpectAdapter` over `client.serial`, driven by `sendline` /
`expect` rather than shelling out to `j serial pipe`. Auto-skipped
when `JUMPSTARTER_HOST` is not set.

The fixtures here intentionally never touch external `j serial pipe`
processes. If the serial port is held by something else, PexpectAdapter
surfaces a clear error and pytest reports the failing fixture — no
silent stomping.
"""

from __future__ import annotations

import os

import pytest


pytestmark = pytest.mark.skipif(
    not os.environ.get("JUMPSTARTER_HOST"),
    reason="JUMPSTARTER_HOST not set — device test skipped",
)


@pytest.fixture
def console(jumpstarter_client, real_target_url):
    """Attach a pexpect console to the ESP32's UART. Depends on
    `real_target_url` so the session-level flash+boot has already
    completed before we try to grab the port — otherwise our adapter
    would race against `esp32.flash` for the FTDI lock."""
    from jumpstarter_driver_network.adapters import PexpectAdapter

    assert jumpstarter_client is not None, "JUMPSTARTER_HOST present but client is None"
    # real_target_url is bound only to materialise the dependency; we
    # discard the value here. The flash+detect already ran.
    _ = real_target_url
    with PexpectAdapter(client=jumpstarter_client.serial) as c:
        yield c


def test_serial_cli_help(console):
    console.sendline("help")
    console.expect(rb">> commands:", timeout=10)
    console.expect(rb"wifi scan", timeout=5)
    console.expect(rb"factory_reset", timeout=5)


def test_serial_cli_state_reports_wifi(console):
    console.sendline("state")
    # Either Connected{...} (we got DHCP) or ApMode{...} (no network was
    # reachable). Both are valid CLI states.
    console.expect(rb">> wifi state: (Connected|ApMode|Connecting|Disconnected)", timeout=10)


def test_serial_cli_wifi_list(console):
    console.sendline("wifi list")
    # Either "ssid=…" rows or the "no networks configured" hint.
    console.expect(rb">> (\s*\[\d+\] ssid=|no networks configured)", timeout=10)


def test_serial_cli_unknown_command(console):
    console.sendline("bogus_command_xyz")
    console.expect(rb">> unknown command: bogus_command_xyz", timeout=10)
