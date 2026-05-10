# watercontroller (`doremorwater`)

Native Rust ESP32 firmware for an irrigation/water controller — successor to
an ESPHome project of the same name. The original ESPHome YAML is preserved
at [`ref/watercontroller_esphome.yaml`](ref/watercontroller_esphome.yaml) as
the behavioral specification.

For the full implementation brief, decisions, milestone status, and
known-deferred work, see [`PLAN.md`](PLAN.md).

## What runs where

The codebase is a Cargo workspace with three crates:

| Crate | Target | Purpose |
|-------|--------|---------|
| `crates/core` | host + Xtensa | All platform-independent logic (valve state machine, schedule engine, calibration math, HA Discovery payloads, MQTT dispatch, NVS schema, HTTP API types, traits for hardware). 37 unit tests. |
| `crates/firmware` | `xtensa-esp32-espidf` | ESP32 binary. Implements the `core` traits via `esp-idf-svc`/`-hal` and wires HTTPD, NVS, schedule, sensor, MQTT supervisor, telnet log server. |
| `crates/host` | x86_64 (native) | Native binary for SPA development. Same HTTP routes as the firmware, served by axum, with fake hardware. |

The SPA at [`crates/firmware/assets/index.html`](crates/firmware/assets/index.html)
is bundled into the firmware via `include_bytes!` and also served by the
host build, so frontend iteration doesn't need a flash cycle.

## Quick start

### Prerequisites (one-time)

- **Host Rust** (any stable): used for `host` + `core` tests.
- **Espressif Rust toolchain**: install via `cargo install espup --locked && espup install --targets esp32`.
- **`ldproxy`**: `cargo install ldproxy --locked`.
- **Podman** (or Docker) for the firmware build container.
- **`qemu-system-xtensa`** for the optional emulator path. Prebuilt:
  `curl -L https://github.com/espressif/qemu/releases/download/esp-develop-9.0.0-20240606/qemu-xtensa-softmmu-esp_develop_9.0.0_20240606-x86_64-linux-gnu.tar.xz | tar xJ -C ~/.local/qemu-xtensa --strip-components=0`

### Day-to-day

```sh
# Native dev: business logic + UI
cargo test -p watercontroller-core           # 37 tests, fast
cargo run  -p watercontroller-host           # http://127.0.0.1:8765

# Firmware
scripts/firmware.sh build                    # debug build inside container
scripts/firmware.sh build --release          # ~1.2 MB stripped, fits OTA partition
scripts/firmware.sh shell                    # drop into the container shell

# QEMU smoke test (boot + tasks; HTTP needs the qemu_eth wiring + open_eth NIC)
scripts/qemu.sh                              # builds with --features qemu, boots

# Flash to a real board (when you have hardware connected)
scripts/firmware.sh build --release \
  && espflash flash --monitor --partition-table crates/firmware/partitions.csv \
       target/firmware/xtensa-esp32-espidf/release/watercontroller-firmware
```

## Hardware

Pin map matches the original ESPHome config (preserved verbatim):

| GPIO | Direction | Purpose |
|------|-----------|---------|
| 36   | ADC in    | Battery voltage |
| 32   | ADC in    | Pressure sensor |
| 33   | PCNT in   | Water flow pulses |
| 12   | GPIO out  | Sprinkler 1 ("Riego exterior", 7 min auto-off) |
| 4    | GPIO out  | Sprinkler 2 ("Riego mobil", 5 min auto-off) |
| 26   | GPIO out  | Water valve OPEN coil (14 s pulse) |
| 27   | GPIO out  | Water valve CLOSE coil (14 s pulse) |
| 25   | GPIO out  | Drain valve (5 min hold) |
| 13/14| GPIO out  | Status LEDs |

CPU forced to 80 MHz to match the original (power-saving).

## Repo layout

```
watercontroller/
├── PLAN.md                     # implementation brief + milestone status
├── README.md                   # this file
├── ref/                        # ESPHome YAML reference (read-only)
├── scripts/
│   ├── firmware.sh             # cargo-in-container wrapper
│   └── qemu.sh                 # builds + boots firmware in QEMU
├── crates/
│   ├── core/                   # platform-independent logic + tests
│   ├── firmware/               # ESP32 binary
│   │   ├── assets/index.html   # embedded SPA
│   │   ├── partitions.csv      # OTA A/B + NVS + spiffs reserve
│   │   └── sdkconfig.defaults
│   └── host/                   # native binary (axum HTTP server)
└── target/firmware/            # firmware build artifacts (gitignored)
```

## State of the world

`PLAN.md` has the per-milestone status table. Short version:

- ✅ Build pipeline, dual-compile, 37 core tests, host SPA + API.
- ✅ Firmware compiles with WiFi supervisor, MQTT client, HTTPD,
  NVS, schedule, telnet logs, tee logger.
- ✅ Boots cleanly under QEMU (`scripts/qemu.sh`) — verified through
  HTTPD route registration.
- 🟡 WiFi/MQTT compile but are unverified — first flash will surface real issues.
- ⏳ ADC/PCNT/GPIO peripheral wiring, OTA, Wireguard are scaffolded but
  not driven against real hardware. See PLAN.md for the punch list.

## License

MIT.
