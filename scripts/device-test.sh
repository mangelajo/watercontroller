#!/usr/bin/env bash
# Flash the firmware via Jumpstarter, wait for the device to come up,
# discover its DHCP-assigned IP from the serial boot log, and run the
# Playwright suite against the real device.
#
# Designed to be invoked from inside a `jmp shell` session (so
# JUMPSTARTER_HOST is already set), but will fall back to looking at
# `$JUMPSTARTER_HOST` if you pre-set it elsewhere.
#
# Usage (from `make device-test`):
#   jmp shell -l target=esp32 -- scripts/device-test.sh
#
# Environment:
#   APP_BIN   — defaults to target/firmware/app.bin (built by `make app-image`)
#   BOOT_TIMEOUT_S — defaults to 45; how long we wait for `sta ip: …` in serial
#   IP        — skip flash + detect, run tests directly against this IP
#
# Exit codes:
#   0 — tests passed
#   1 — flash failed
#   2 — IP not detected within BOOT_TIMEOUT_S
#   3 — tests failed

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_BIN="${APP_BIN:-target/firmware/app.bin}"
BOOT_TIMEOUT_S="${BOOT_TIMEOUT_S:-45}"
SERIAL_LOG="${SERIAL_LOG:-/tmp/wc-device-test.log}"

cd "$REPO_ROOT"

if [[ -z "${JUMPSTARTER_HOST:-}" ]]; then
    echo "error: JUMPSTARTER_HOST is not set — run this under \`jmp shell\`." >&2
    exit 1
fi

# --- 0. release the port from any prior consumer ----------------------------
# The lab-side gRPC server allows only one `j serial pipe` consumer at a
# time. If we don't kill stragglers up-front the flash call will fail with
# EAGAIN on the FTDI/CH340 flock.
echo "==> releasing serial port (killing stale j serial pipe)"
pkill -f "j serial pipe" 2>/dev/null || true
sleep 1.5

# --- 1. flash + reset (unless caller pointed us at an existing device) ------
if [[ -z "${IP:-}" ]]; then
    if [[ ! -f "$APP_BIN" ]]; then
        echo "error: $APP_BIN not found — run \`make app-image\` first." >&2
        exit 1
    fi
    echo "==> flashing $APP_BIN to 0x20000"
    j esp32 flash --address 0x20000 "$APP_BIN" >/dev/null

    # Start a background pipe to capture boot output. ESP32 prints the
    # DHCP-assigned IP as `sta ip: X.X.X.X` once the netif handler fires.
    : > "$SERIAL_LOG"
    j serial pipe -o "$SERIAL_LOG" &
    PIPE_PID=$!
    trap 'kill $PIPE_PID 2>/dev/null || true' EXIT

    echo "==> resetting device"
    j esp32 reset >/dev/null

    echo "==> waiting up to ${BOOT_TIMEOUT_S}s for sta ip: line"
    deadline=$(( $(date +%s) + BOOT_TIMEOUT_S ))
    IP=""
    while [[ $(date +%s) -lt $deadline ]]; do
        if grep -qoE 'sta ip: [0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' "$SERIAL_LOG" 2>/dev/null; then
            IP=$(grep -oE 'sta ip: [0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' "$SERIAL_LOG" | tail -1 | awk '{print $3}')
            break
        fi
        sleep 1
    done
    if [[ -z "$IP" ]]; then
        echo "==> last 40 lines of serial log:"
        tail -n 40 "$SERIAL_LOG" || true
        echo "error: device did not announce sta ip within ${BOOT_TIMEOUT_S}s" >&2
        exit 2
    fi
    echo "==> device IP: $IP"

    # Release the boot-capture pipe so test_serial_cli.py can claim it.
    kill "$PIPE_PID" 2>/dev/null || true
    wait "$PIPE_PID" 2>/dev/null || true
    sleep 1
else
    echo "==> IP override: $IP (skipping flash)"
fi

# --- 2. wait for HTTP to actually answer ------------------------------------
echo "==> waiting for $IP/api/status to answer"
deadline=$(( $(date +%s) + 30 ))
while [[ $(date +%s) -lt $deadline ]]; do
    if curl -sk --max-time 3 "http://$IP/api/status" -o /dev/null; then
        break
    fi
    sleep 1
done

# --- 3. run pytest ----------------------------------------------------------
VENV="$REPO_ROOT/tests/playwright/.venv"
if [[ ! -x "$VENV/bin/pytest" ]]; then
    echo "error: playwright venv missing — run \`make playwright\` first." >&2
    exit 1
fi

echo "==> running playwright suite against http://$IP and JUMPSTARTER_HOST=$JUMPSTARTER_HOST"
WC_TEST_TARGET_URL="http://$IP" \
    "$VENV/bin/pytest" tests/playwright -v
