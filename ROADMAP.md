# Roadmap

Improvement ideas distilled from operating this codebase. Ordered
roughly by impact-per-effort within each section.

## Reliability

### OTA rollback safety
`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` is set, but the firmware
never calls `esp_ota_mark_app_valid_cancel_rollback_image()`. A new
OTA image stays in `pending-verify` and the bootloader reverts on
the next reboot. Fix: mark valid after N seconds of healthy runtime
(WiFi up + HTTPD listening + heap stable). One-line FFI call, big
safety net.

### WiFi reliability hardening
The -85 to -94 dBm flake hurts the device and the test suite alike.
Three layers:

- **Watchdog reboot** when `probe_link` fails 3× in a row AND the
  subsequent reconnect attempt fails N times. Today the supervisor
  loops forever.
- **Beacon-loss event log via a decoupled task**: revive the
  WIFI_EVENT subscription that previously blew wifi-sup's stack,
  but route through an `AtomicU16` + a separate `wifi-event-log`
  pthread that handles the formatting. Surfaces *why* the AP
  dropped us.
- **Reset-reason persistence**: log every panic / brown-out / wdt /
  poweron reset to a small NVS-backed ring buffer surfaced on
  `/api/diag`. Today you see "panic / exception" once at boot and
  lose history.

### Wireguard FFI (M11)
Still pending in the task list. Without it remote access goes
through the LAN only. Easiest path: `esp_wireguard` IDF component
wired via `bindgen`. Heavier-but-pure-Rust alternative:
`wireguard-rs`.

### Boring-device safeguards
- Periodic NVS-saved counter of panic reboots (alarms when it
  crosses N/day).
- Optional auto-reboot every 24 h (planned, predictable, prevents
  latent leaks from accumulating).
- Watchdog on stuck valve states: `WaterValve` mid-sequence > 30 s
  → force-close + log.

## Observability

### Soak test
`pytest -m soak` that runs the device for 30 min and asserts:
- no panic reboots (reset_reason stays "software restart" or
  "power-on", never "panic").
- heap `min_ever_free_bytes` doesn't drift below 80 % of starting
  value.
- `wifi-sup` HWM doesn't grow.
- WiFi reconnect events stay under a threshold.
Run overnight before each release.

### Prometheus / OpenMetrics endpoint
`/metrics` exposing heap, per-task HWM, RSSI, uptime, total water,
flow, alarm state, schedule fires. HA + Grafana can scrape directly
without going through MQTT.

### Long-term log persistence
A small flash partition (or rotating file on PSRAM) holding the
last ~100 KB of logs so a post-mortem after a reboot has more than
just the panic backtrace from boot+1.

## Features

### Water budget alarm + alarm history
Two small extensions of the existing flow alarm:
- **Daily/weekly cumulative-L alarm**: catches a slow leak the
  spike-based flow alarm misses (same `enabled / threshold /
  window` shape, integrates over time).
- **Alarm history**: persist the last N (say 16) alarm events with
  timestamp + flow + duration to NVS. SPA shows them on the
  Sprinklers tab; HA exposes them via an `event` entity. Tells the
  user "what happened last Tuesday" without scrolling HA history.

### Mobile-friendly SPA
Cards overflow horizontally on phones. Wrap `.grid2` in
`@media (max-width: 600px) { display: block }`, force `width:100%`
on inputs, bump toggle tap area to ≥44 px. Cheap, visible win for
the way this device actually gets used (from the garden, not a
laptop).

### Per-zone moisture sensors
Add ADC inputs + matching `Sensor` discovery so HA can drive
schedule overrides ("skip irrigation if zone-2 moisture > X").

### HA Energy dashboard hook
We already publish `water_total` as `state_class: total_increasing`.
Confirm it shows up under HA's water dashboard and document the
setup.

## Developer experience

### CI on every PR
GitHub Actions:
- `cargo test -p watercontroller-core`
- `cargo build -p watercontroller-host`
- `cargo clippy -- -D warnings` on core + host
- Playwright host suite (`pytest tests/playwright`)
- firmware *build-only* check via the `idf-rust` container
- (Skip Jumpstarter device tests; those stay opt-in.)

Biggest force-multiplier on this list — every later change becomes
safer to ship.

### Pre-commit hooks
`pre-commit` config running `cargo fmt`, basic `cargo check`, and a
fast subset of clippy. Cheap.

### Wiring + setup docs
A README aimed at non-developers (Bill of Materials, wiring
diagram for the valve + sensors, first-boot AP-mode walkthrough,
HA integration guide).

## Security

### Brute-force protection on admin_token
Currently any number of bad `Authorization: Bearer` attempts cost
nothing. Add a per-IP lockout (5 fails → 30 s) and surface in
`/api/diag`.

### Production hardening (when packaging for real)
- Secure boot (cuts unsigned-firmware attack)
- Flash encryption (cuts physical NVS-dump attack)
- Disable serial CLI in release builds, or gate it behind a
  hardware jumper.
