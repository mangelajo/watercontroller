# watercontroller (`doremorwater`)

Native Rust ESP32 firmware for an irrigation/water controller — successor to
an ESPHome project of the same name. The original ESPHome YAML at
[`ref/watercontroller_esphome.yaml`](ref/watercontroller_esphome.yaml) is the
behavioral specification.

Companion docs:
- [`PLAN.md`](PLAN.md) — implementation brief, decisions, milestone status
- [`ROADMAP.md`](ROADMAP.md) — improvement ideas, ordered by impact
- [`CLAUDE.md`](CLAUDE.md) — operating notes for working on ESP32 stack
  discipline, decoding panics, the hardware test loop

## What it does

- Two sprinklers + a motorised water valve (open/close pulse coils) + drain
  valve, driven from GPIOs with per-channel auto-off timers.
- Battery + pressure ADCs, water-flow pulse counter (PCNT), per-rule cron
  schedule with optional duration override.
- Web SPA + HTTP/HTTPS API + WS log stream + telnet log server on :23.
- WiFi multi-SSID with AP-mode setup wizard fallback, mDNS responder,
  captive-portal DNS, OTA (A/B partitions + rollback-on-failure).
- MQTT client with Home Assistant Discovery, TLS broker support.
- NVS-backed runtime config (admin token, TLS cert/key, schedule rules,
  WiFi creds, webhook destinations, flow-alarm thresholds).
- Webhook framework with Slack / Discord / HA / generic presets,
  template substitution, configurable headers.
- `/api/diag` exposes per-task stack HWM, heap split (internal vs PSRAM),
  WiFi state, uptime — pulled by `serial-logs/healthcheck.sh` for trend
  monitoring during long hardware runs.

## What runs where

The codebase is a Cargo workspace with three crates:

| Crate              | Target                    | Purpose                                                                                                  |
|--------------------|---------------------------|----------------------------------------------------------------------------------------------------------|
| `crates/core`      | host + Xtensa             | Platform-independent logic: valve state machine, schedule engine, calibration, HA discovery, NVS schema, HTTP API types, webhook dispatch trait, hardware traits. Unit-tested. |
| `crates/firmware`  | `xtensa-esp32-espidf`     | ESP32 binary. Implements `core` traits via `esp-idf-svc`/`-hal`. WiFi supervisor, HTTPD x2 (HTTP + HTTPS), MQTT supervisor, telnet log server, OTA dispatcher, serial CLI. |
| `crates/host`      | x86_64 (native)           | Native binary serving the same SPA + API with fake hardware (for frontend iteration without a flash cycle). |

The SPA at [`crates/firmware/assets/index.html`](crates/firmware/assets/index.html)
is embedded via `include_bytes!` into the firmware and also served by the
host build.

## Quick start

```sh
# One-time: install everything (rustup, esp toolchain, qemu, idf-rust
# container, playwright venv). Idempotent.
make bootstrap

# Native dev: run the SPA + fake hardware on http://127.0.0.1:8765
make host

# Run core tests
make test

# Build release firmware (in the idf-rust container)
make firmware-release

# OTA-flash a running device
make ota IP=192.168.1.16
```

`make` with no args lists every target with a one-line description.
`make doctor` reports which prerequisites are installed.

## Makefile reference

### One-time setup

| Target              | What it does                                                                       |
|---------------------|------------------------------------------------------------------------------------|
| `bootstrap`         | Runs all install targets below. Safe to re-run.                                    |
| `rustup`            | Install rustup + stable toolchain (curl from sh.rustup.rs).                        |
| `esp-toolchain`     | Install `espup`, `ldproxy`, and the Xtensa Rust toolchain via espup.               |
| `qemu-xtensa`       | Download Espressif's prebuilt `qemu-system-xtensa` (x86_64 Linux only).            |
| `container`         | Pull `docker.io/espressif/idf-rust:esp32_latest` (used by firmware builds).        |
| `playwright`        | Create the Playwright Python venv and install headless chromium.                   |

### Day-to-day

| Target              | What it does                                                                       |
|---------------------|------------------------------------------------------------------------------------|
| `test`              | `cargo test --lib -p watercontroller-core`.                                        |
| `host`              | `cargo run -p watercontroller-host` — SPA + API at http://127.0.0.1:8765.          |
| `firmware`          | Debug firmware build inside the idf-rust container.                                |
| `firmware-release`  | Release firmware build (size-optimised, fits the OTA partition).                   |
| `firmware-shell`    | Drop into a shell inside the firmware build container.                             |
| `qemu`              | Build with `--features qemu`, boot in QEMU (HTTP on :18080, telnet on :18023).     |
| `qemu-stop`         | Kill any running `qemu-system-xtensa` process.                                     |
| `ui-tests`          | Playwright tests in headless chromium against the host build.                      |

### Hardware iteration

OTA path — `make ota IP=<addr>` is the fast iteration loop on a connected
device (~15 s wall time vs. ~25 s for a full serial flash). It doesn't wipe
otadata/NVS; use the device-test flow for clean-slate runs.

| Target              | What it does                                                                       |
|---------------------|------------------------------------------------------------------------------------|
| `app-image`         | Build `target/firmware/app.bin` (release, OTA-ready).                              |
| `ota IP=<addr>`     | Upload `app.bin` to `<addr>` and reboot into it. `TOKEN=<bearer>` if admin_token is set. |
| `ota-status IP=<addr>` | Pretty-print `/api/status` from the device.                                     |
| `device-test`       | Build, wipe otadata + NVS, flash via Jumpstarter, run the Playwright suite against the real board. Requires an active `jmp shell -l target=esp32` (sets `JUMPSTARTER_HOST`). Override with `WC_TEST_TARGET_URL=http://<ip>` to skip the flash step. |

### Maintenance

| Target              | What it does                                                                       |
|---------------------|------------------------------------------------------------------------------------|
| `clean`             | `cargo clean` + remove `target/firmware`.                                          |
| `distclean`         | Also nuke `~/.cache/watercontroller-{cargo,espressif}`.                            |
| `doctor`            | Report which prerequisites are installed (✓ / ✗ missing).                          |

## Hardware

Pin map matches the original ESPHome config:

| GPIO  | Direction | Purpose                                              |
|-------|-----------|------------------------------------------------------|
| 36    | ADC in    | Battery voltage                                      |
| 32    | ADC in    | Pressure sensor                                      |
| 33    | PCNT in   | Water-flow pulses                                    |
| 12    | GPIO out  | Sprinkler 1 ("Riego exterior", 7 min auto-off)       |
| 4     | GPIO out  | Sprinkler 2 ("Riego mobil", 5 min auto-off)          |
| 26    | GPIO out  | Water valve OPEN coil (14 s pulse)                   |
| 27    | GPIO out  | Water valve CLOSE coil (14 s pulse)                  |
| 25    | GPIO out  | Drain valve (5 min hold)                             |
| 13/14 | GPIO out  | Status LEDs                                          |

CPU forced to 80 MHz to match the original (power-saving).

Module: ESP32-PICO with PSRAM (8 MB SPIRAM). The firmware also runs on plain
WROOM modules without PSRAM — `CONFIG_SPIRAM_IGNORE_NOTFOUND=y` falls back
to internal-only DRAM at boot.

## Configuration

Runtime configuration lives in NVS and is editable via:
- The **SPA** (Settings tabs) on http://`<device-ip>`/.
- The **HTTP API** under `/api/config/*`. With `admin_token` set, mutating
  routes require `Authorization: Bearer <token>`.
- The **serial CLI** over UART (115200 8N1): `wifi list`, `wifi add`,
  `tasks`, `mem`, `log <level>`, `webhook list`, `alarm status`, etc.
  Type `help` for the full list.

The device gets its **WiFi seed** at flash time from `.env` (see
`.env.example`); after first boot, the SPA / API / serial CLI can add or
replace networks. Build-time defaults are in `crates/core/src/config.rs`.

Backup and restore: **`GET /api/config?all=1`** dumps the full config
including secrets (admin token, MQTT password, TLS key); **`PUT /api/config`**
accepts the same payload to restore. The Advanced tab in the SPA exposes
Download + Upload buttons.

## Diagnostics + monitoring

- `GET /api/diag` is unauthenticated and cheap — heap split (internal vs
  PSRAM), per-task stack high-water marks, WiFi state, uptime. Poll freely.
- **WebSocket log stream** at `/ws/logs` and a parallel **telnet server on
  port 23** mirror the UART output. The SPA's Logs tab is the WS consumer.
- `serial-logs/healthcheck.sh` runs a 15-min loop poking `/api/status` and
  `/api/diag`, plus a TCP probe of port 23, and appends one CSV-ish line per
  cycle to `serial-logs/health-check.log`. Useful for long-running hardware
  babysitting. Override with `WC_IP=<ip>` (default `192.168.1.182`).
- See [`CLAUDE.md`](CLAUDE.md) for decoding ESP32 panics with `addr2line`
  and the rules-of-thumb for staying out of stack trouble.

## Repo layout

```
watercontroller/
├── README.md, PLAN.md, ROADMAP.md, CLAUDE.md
├── Makefile                            # see "Makefile reference" above
├── ref/                                # ESPHome YAML reference (read-only)
├── scripts/
│   ├── firmware.sh                     # cargo-in-container wrapper
│   ├── qemu.sh                         # builds + boots firmware in QEMU
│   └── ota-flash.sh                    # OTA upload helper (used by `make ota`)
├── crates/
│   ├── core/                           # platform-independent logic + tests
│   ├── firmware/                       # ESP32 binary
│   │   ├── assets/index.html           # embedded SPA
│   │   ├── partitions.csv              # OTA A/B + NVS + SPA storage
│   │   └── sdkconfig.defaults          # IDF Kconfig overrides
│   └── host/                           # native binary (axum HTTP server)
├── tests/playwright/                   # Python + Playwright UI suite
└── serial-logs/                        # local-only: capture + healthcheck (gitignored)
```

## License

MIT.
