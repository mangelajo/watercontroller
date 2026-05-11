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
def console(jumpstarter_client, real_target_url, term):
    """Attach a pexpect console to the ESP32's UART. Depends on
    `real_target_url` so the session-level flash+boot has already
    completed before we try to grab the port — otherwise our adapter
    would race against `esp32.flash` for the FTDI lock."""
    from jumpstarter_driver_network.adapters import PexpectAdapter

    assert jumpstarter_client is not None, "JUMPSTARTER_HOST present but client is None"
    _ = real_target_url  # only bound to force the device-setup dependency
    term("cli: attaching pexpect adapter over client.serial")
    with PexpectAdapter(client=jumpstarter_client.serial) as c:
        yield Narrator(c, term)
    term("cli: console detached")


class Narrator:
    """Thin wrapper that logs every `sendline` / `expect` to the terminal
    before delegating to the underlying pexpect spawn. Keeps the test
    bodies short while still surfacing each UART exchange so a hung
    `expect` is immediately obvious."""

    def __init__(self, spawn, term):
        self._s = spawn
        self._term = term

    def sendline(self, line: str) -> None:
        # Drain any pending bytes from the previous test / unrelated
        # heartbeat output so they don't get consumed by the next
        # `expect()` call. read_nonblocking with timeout=0 returns
        # whatever's currently buffered without waiting.
        try:
            while True:
                self._s.read_nonblocking(size=4096, timeout=0)
        except Exception:
            pass
        self._term(f"cli: >>> {line}")
        self._s.sendline(line)

    def expect(self, pattern, timeout: float = 10) -> None:
        # `pattern` is bytes — show a printable form.
        shown = pattern.decode(errors="replace") if isinstance(pattern, (bytes, bytearray)) else str(pattern)
        self._term(f"cli: waiting for /{shown}/ (timeout {timeout}s)")
        self._s.expect(pattern, timeout=timeout)
        match = getattr(self._s, "match", None)
        if match is not None:
            text = match.group(0)
            if isinstance(text, bytes):
                text = text.decode(errors="replace")
            self._term(f"cli: <<< matched {text!r}")


def test_serial_cli_help(console):
    console.sendline("help")
    console.expect(rb">> commands:", timeout=10)
    console.expect(rb"wifi scan", timeout=5)
    console.expect(rb"factory_reset", timeout=5)


def test_serial_cli_state_reports_wifi(console):
    console.sendline("state")
    # Either Connected{…} (we got DHCP) or ApMode{…} (no known network).
    # Both are valid CLI states.
    console.expect(rb">> wifi state: (Connected|ApMode|Connecting|Disconnected)", timeout=10)


def test_serial_cli_wifi_list(console):
    console.sendline("wifi list")
    # Either "ssid=…" rows or the "no networks configured" hint.
    console.expect(rb">> (\s*\[\d+\] ssid=|no networks configured)", timeout=10)


def test_serial_cli_unknown_command(console):
    console.sendline("bogus_command_xyz")
    console.expect(rb">> unknown command: bogus_command_xyz", timeout=10)
