#!/usr/bin/env bash
# OTA-flash a running watercontroller device.
#
# Usage:
#   scripts/ota-flash.sh <ip> <app-bin> [admin-token]
#
# Driven by `make ota IP=… [TOKEN=…]`. Captures the pre-upload uptime,
# POSTs the binary to /api/ota, then polls /api/status until uptime resets
# (proves the device actually rebooted into the new slot).
set -eu -o pipefail

ip=${1:?missing device IP}
bin=${2:?missing path to app.bin}
token=${3:-}

if [[ ! -f "$bin" ]]; then
    echo "missing app image: $bin" >&2
    exit 1
fi

before=$(curl -s --max-time 3 "http://$ip/api/status" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("uptime_ms",0))' \
    2>/dev/null || echo 0)

printf "Uploading %s (%s) to http://%s/api/ota …\n" \
    "$bin" "$(du -h "$bin" | cut -f1)" "$ip"

curl_args=(
    --silent --show-error --fail --max-time 120
    -X POST
    -H "Content-Type: application/octet-stream"
    --data-binary "@$bin"
    -w "HTTP %{http_code}  upload=%{time_total}s  size=%{size_upload}B  speed=%{speed_upload}B/s\n"
)
if [[ -n "$token" ]]; then
    curl_args+=(-H "Authorization: Bearer $token")
fi

curl "${curl_args[@]}" "http://$ip/api/ota"
echo

printf "Waiting for the new slot to come back online …\n"
for _ in $(seq 1 30); do
    sleep 1
    after=$(curl -s --max-time 2 "http://$ip/api/status" 2>/dev/null \
        | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("uptime_ms",-1))' \
        2>/dev/null || echo -1)
    if [[ "$after" != "-1" && "$after" -lt "$before" ]]; then
        printf '\033[32m✓\033[0m device rebooted (uptime now %sms, was %sms)\n' "$after" "$before"
        exit 0
    fi
done
printf '\033[33m!\033[0m device did not surface a fresh /api/status within 30s — check serial.\n'
exit 1
