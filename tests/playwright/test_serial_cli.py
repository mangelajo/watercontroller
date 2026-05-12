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
def console(device_console, real_target_url, term):
    """Wrap the session-scoped `device_console` in a Narrator for this
    test. The underlying PexpectAdapter is owned by the session — we
    don't attach/detach per test, so serial-output mirroring to
    stderr (logfile_read in conftest.device_console) stays
    continuous across tests."""
    assert device_console is not None, "JUMPSTARTER_HOST present but device_console missing"
    _ = real_target_url  # force the boot+IP-detect to complete first
    n = Narrator(device_console, term)
    # Quiet info-level chatter (heartbeats, wifi events, https
    # handshakes) for the duration of the test so they don't
    # interleave with the `>>` lines we're matching on.
    try:
        n.sendline("log warn")
        n.expect(rb">> log level set to warn", timeout=5)
    except Exception:
        term("cli: warn-level set attempt failed (older firmware?) — continuing")
    try:
        yield n
    finally:
        # Best-effort restore so other channels (manual serial,
        # different test session) see default verbosity.
        try:
            n.sendline("log info")
        except Exception:
            pass


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
        try:
            self._s.expect(pattern, timeout=timeout)
        except Exception:
            # Surface the bytes pexpect actually received so a TIMEOUT
            # is debuggable instead of an opaque "didn't match".
            before = getattr(self._s, "before", b"") or b""
            if isinstance(before, bytes):
                tail = before[-1500:].decode(errors="replace")
            else:
                tail = str(before)[-1500:]
            self._term(f"cli: TIMEOUT — buffer tail:\n{tail}")
            raise
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
    console.expect(rb">> unknown command: bogus_command_xyz", timeout=20)


def test_serial_cli_tasks(console):
    """`tasks` prints a tabulated task list. We don't assert exact stack
    numbers (they're per-build) but we expect the header row, a
    separator, and at least one named task we know to exist."""
    console.sendline("tasks")
    console.expect(rb">> NAME\s+STATE\s+PRI\s+STACK_FREE\s+RUNTIME", timeout=10)
    console.expect(rb"-{20,}", timeout=5)
    console.expect(rb"wifi-sup\s+\S+\s+\d+\s+\d+\s+\d+", timeout=5)


def test_serial_cli_mem(console):
    """`mem` prints heap stats with thousands separators. The label
    width varies per row (longer labels eat the colon padding), so we
    match colon with `\\s*` rather than `\\s+`."""
    console.sendline("mem")
    console.expect(rb">> heap:", timeout=10)
    console.expect(rb">>\s+total free\s*:\s+[\d,]+ B", timeout=5)
    console.expect(rb">>\s+largest free block\s*:\s+[\d,]+ B", timeout=5)
    console.expect(rb">>\s+min-ever free\s*:\s+[\d,]+ B", timeout=5)


def test_serial_cli_alarm_status_and_clear(console):
    """`alarm status` prints the config + latched state; `alarm clear`
    acks. We don't assert on specific numbers — the config may have
    been mutated by other test runs against the same NVS — just the
    structure of the response."""
    console.sendline("alarm status")
    console.expect(rb">> flow alarm: (ACTIVE|idle) \| enabled=(true|false)", timeout=10)
    console.sendline("alarm clear")
    console.expect(rb">> alarm cleared", timeout=10)
    console.sendline("alarm bogus")
    console.expect(rb">> usage: alarm <status\|clear>", timeout=10)


def test_serial_cli_log_level(console):
    """`log <level>` accepts the canonical levels and rejects garbage.
    We restore `info` at the end so subsequent tests / interactive
    sessions get the default verbosity back."""
    console.sendline("log warn")
    console.expect(rb">> log level set to warn", timeout=10)
    console.sendline("log bogus")
    console.expect(rb">> usage: log <off\|error\|warn\|info\|debug\|trace>", timeout=10)
    console.sendline("log info")
    console.expect(rb">> log level set to info", timeout=10)
