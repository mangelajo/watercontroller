"""Serial-CLI smoke test against a real ESP32 over Jumpstarter.

Only runs when `JUMPSTARTER_HOST` is set. Each test owns the serial
port exclusively for its duration: the lab-side gRPC server allows
only one `j serial pipe` consumer at a time (the FTDI/CH340 needs
`flock(LOCK_EX)`), so we kill stragglers up-front and use a single
bidirectional pipe per test that both writes commands and captures
output.

Strategy per test:
  1. `_kill_existing_pipes` — release the port from any prior consumer.
  2. Popen `j serial pipe -o LOG` with stdin=PIPE so we can write
     commands to UART; output is mirrored into LOG.
  3. Send commands then poll LOG until the expected `>>` line lands.
  4. SIGTERM the pipe.
"""

from __future__ import annotations

import os
import signal
import subprocess
import time
from pathlib import Path

import pytest


JUMPSTARTER_HOST = os.environ.get("JUMPSTARTER_HOST")
J_BIN = os.environ.get("JUMPSTARTER_BIN", str(Path.home() / ".local/jumpstarter/bin/j"))

pytestmark = pytest.mark.skipif(
    not JUMPSTARTER_HOST or not Path(J_BIN).exists(),
    reason="JUMPSTARTER_HOST not set or `j` binary not found — device test skipped",
)


def _kill_existing_pipes() -> None:
    subprocess.run(["pkill", "-f", "j serial pipe"], check=False)
    # The OS holds the flock for ~250 ms after the holder dies.
    time.sleep(1.5)


def _await_in(log_path: Path, pattern: str, timeout_s: float, sniff: subprocess.Popen | None = None) -> str:
    """Poll `log_path` until `pattern` appears or we time out. If `sniff` is
    given, raise early if that subprocess has already exited (saves the full
    timeout when the pipe failed to start)."""
    deadline = time.monotonic() + timeout_s
    text = ""
    while time.monotonic() < deadline:
        if sniff is not None and sniff.poll() is not None:
            err = sniff.stderr.read().decode(errors="replace") if sniff.stderr else ""
            raise AssertionError(f"serial pipe exited early ({sniff.returncode}): {err.strip()}")
        if log_path.exists():
            text = log_path.read_text(errors="replace")
            if pattern in text:
                return text
        time.sleep(0.3)
    raise AssertionError(
        f"timeout waiting {timeout_s}s for {pattern!r} in {log_path}\nTail:\n{text[-2000:]}"
    )


@pytest.fixture
def serial_pipe(tmp_path: Path):
    """Yield (popen, log_path). Owns the serial port exclusively for the test.
    Commands are written via popen.stdin.write(...) + flush; output lands in
    log_path."""
    _kill_existing_pipes()
    log = tmp_path / "serial.log"
    env = {**os.environ, "JMP_DRIVERS_ALLOW": "UNSAFE"}
    # `-i` is "force enable stdin to serial"; without it, auto-detection
    # may decide our stdin pipe is non-interactive and skip writes.
    proc = subprocess.Popen(
        [J_BIN, "serial", "pipe", "-i", "-o", str(log)],
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    # Wait briefly for the pipe to attach (or fail).
    time.sleep(2.0)
    if proc.poll() is not None:
        err = proc.stderr.read() if proc.stderr else ""
        pytest.skip(f"serial pipe failed to start: {err.strip()}")
    try:
        yield proc, log
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def _send(proc: subprocess.Popen, commands: str) -> None:
    """Write commands to the pipe's UART input. Each command must end in \\n."""
    assert proc.stdin is not None
    proc.stdin.write(commands)
    proc.stdin.flush()


def test_serial_cli_help_and_state(serial_pipe):
    proc, log = serial_pipe
    _send(proc, "help\nstate\n")
    text = _await_in(log, ">> wifi state:", timeout_s=15, sniff=proc)
    # `help` lists at least these three commands.
    assert "wifi scan" in text
    assert "factory_reset" in text
    assert "wifi list" in text


def test_serial_cli_wifi_list(serial_pipe):
    proc, log = serial_pipe
    _send(proc, "wifi list\n")
    text = _await_in(log, ">>", timeout_s=10, sniff=proc)
    # Either we have networks (ssid=…) or the "no networks configured" hint.
    assert ("ssid=" in text) or ("no networks configured" in text)


def test_serial_cli_unknown_command(serial_pipe):
    proc, log = serial_pipe
    _send(proc, "bogus_command_xyz\n")
    _await_in(log, ">> unknown command: bogus_command_xyz", timeout_s=10, sniff=proc)
