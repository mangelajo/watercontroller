"""Long-running soak test against a real ESP32.

Polls /api/diag every minute over a configurable duration (default 30
minutes) and asserts that the device stays healthy:

* No panic reboots — `reset_reason` mustn't transition to a panic-
  flavored reason during the run, and uptime must monotonically grow
  (a drop means the device rebooted).
* Heap floor — `min_ever_free_bytes` mustn't drop below 80% of the
  starting value. A gradual leak shows up as min-ever drifting down;
  a one-shot fragmentation spike does too.
* WiFi-sup HWM stable — the supervisor task's smallest stack free
  must not shrink below its initial value (within a small slack).
  CLAUDE.md tracks this as the canonical "did we break stack
  discipline" signal.

Opt-in: this is too slow for the default device suite. Pass `--soak`
to run it. Without the flag the test silently skips.

Duration knob: `WC_SOAK_MINUTES=30`. Sample interval: `WC_SOAK_INTERVAL_S=60`.
"""

from __future__ import annotations

import json
import os
import time
import urllib.request

import pytest


def pytest_addoption_compat() -> None:
    """Stub so this file documents the flag even when conftest doesn't
    forward it. Real registration lives in conftest.py."""


pytestmark = [
    pytest.mark.skipif(
        not os.environ.get("JUMPSTARTER_HOST") and not os.environ.get("WC_TEST_TARGET_URL"),
        reason="soak test needs a real device (JUMPSTARTER_HOST or WC_TEST_TARGET_URL)",
    ),
]


PANIC_REASONS = {
    "panic / exception",
    "interrupt watchdog",
    "task watchdog",
    "other watchdog",
    "brownout",
}


def _diag(url: str) -> dict:
    with urllib.request.urlopen(f"{url}/api/diag", timeout=5) as r:
        return json.loads(r.read())


def _hwm(d: dict, name: str) -> int | None:
    for t in d.get("tasks", []):
        if t.get("name") == name:
            return t.get("stack_min_free_bytes")
    return None


def test_soak(real_target_url, term, request):
    if not request.config.getoption("--soak", default=False):
        pytest.skip("pass --soak to opt in to the long-running soak test")
    assert real_target_url, "soak needs a reachable device URL"

    minutes = int(os.environ.get("WC_SOAK_MINUTES", "30"))
    interval_s = float(os.environ.get("WC_SOAK_INTERVAL_S", "60"))
    samples = max(2, int((minutes * 60) // interval_s))
    term(f"soak: {minutes} min, {samples} samples @ {interval_s:.0f}s")

    baseline = _diag(real_target_url)
    heap = baseline.get("heap", {})
    start_min_free = int(heap.get("min_ever_free_bytes") or 0)
    start_wifi_hwm = _hwm(baseline, "wifi-sup") or 0
    start_uptime = int(baseline.get("uptime_ms") or 0)
    start_reset = baseline.get("reset_reason") or ""
    floor = int(start_min_free * 0.80)
    term(
        f"soak: baseline min-free={start_min_free:,}B floor={floor:,}B "
        f"wifi-sup hwm={start_wifi_hwm}B reset={start_reset!r}"
    )

    last_uptime = start_uptime
    for i in range(1, samples):
        time.sleep(interval_s)
        d = _diag(real_target_url)
        h = d.get("heap", {})
        min_free = int(h.get("min_ever_free_bytes") or 0)
        wifi_hwm = _hwm(d, "wifi-sup") or 0
        uptime = int(d.get("uptime_ms") or 0)
        reset = d.get("reset_reason") or ""
        term(
            f"soak[{i}/{samples-1}]: up={uptime/1000:.0f}s min-free={min_free:,} "
            f"wifi-sup hwm={wifi_hwm} reset={reset!r}"
        )

        assert reset not in PANIC_REASONS or reset == start_reset, (
            f"device reset_reason became {reset!r} (was {start_reset!r}) — reboot during soak"
        )
        assert uptime >= last_uptime, (
            f"uptime regressed {last_uptime} -> {uptime} — device rebooted"
        )
        assert min_free >= floor, (
            f"heap min-ever {min_free:,}B fell below 80% floor {floor:,}B "
            f"(start {start_min_free:,}B)"
        )
        # wifi-sup HWM is monotonically non-increasing by definition (it's
        # a high-water mark of *free* stack — only shrinks as a task pushes
        # closer to its ceiling). Tighten the gate: must not shrink
        # below start by more than 100 B.
        if start_wifi_hwm and wifi_hwm:
            assert wifi_hwm >= start_wifi_hwm - 100, (
                f"wifi-sup HWM drifted: start {start_wifi_hwm}B now {wifi_hwm}B"
            )
        last_uptime = uptime
