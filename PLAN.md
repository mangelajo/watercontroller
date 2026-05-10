# Watercontroller — Native Rust Port Implementation Plan

## Audience

This document is the implementation brief for an agent (or human) picking up
this project cold. It assumes no prior conversation context. Read it end-to-end
before starting work. Anything not specified here should be raised as a
question, not silently invented.

## Goal

Rewrite the ESPHome-based `doremorwater` water/irrigation controller as a
native Rust ESP32 project, for long-term maintainability by the owner.
Behavioral parity with the existing ESPHome device is the floor; the ceiling
is "nicer to use and easier to maintain."

The original ESPHome YAML at [`ref/watercontroller_esphome.yaml`](ref/watercontroller_esphome.yaml)
is the **behavioral specification**. Where this plan and the YAML disagree,
the YAML wins unless this plan explicitly overrides it.

## Hardware

ESP32 dev board (`esp32dev`), CPU forced to 80 MHz (power-saving — keep this).

| GPIO | Direction | Purpose                | Notes                                        |
|------|-----------|------------------------|----------------------------------------------|
| 36   | ADC in    | Battery voltage        | Linear cal: `1130 → 5.00V`, `2931 → 12.2V`. 15-sample sliding average, publish every 2nd sample. Read every 10 min. |
| 32   | ADC in    | Pressure sensor        | Two linear stages: `0.37→0.54, 2.62→3.98` then `0.54→0.0, 4.50→10.34214`. Output in `bar`. Read every 1 min. |
| 33   | PCNT in   | Flow meter pulses      | Rate: `pulses * 0.00225012 * 60.0` → `L/hr`. Total: `pulses / 516.5` → `L` (state_class total_increasing, 3 dp). Update every 1 min. |
| 12   | GPIO out  | Sprinkler "Riego exterior" | Auto-off after **7 min** (YAML name says "20min" but action is 7min — preserve the 7min behavior; rename label) |
| 4    | GPIO out  | Sprinkler "Riego mobil"    | Auto-off after **5 min** (YAML name says "10min" but action is 5min — same situation) |
| 26   | GPIO out  | Water valve OPEN coil  | 14-second pulse, then off (motorized valve)  |
| 27   | GPIO out  | Water valve CLOSE coil | 14-second pulse, then off                    |
| 25   | GPIO out  | Drain valve            | Auto-off after 5 min                         |
| 13   | GPIO out  | Status LED 1           |                                              |
| 14   | GPIO out  | Status LED 2           |                                              |

### Composite "Water control" virtual switch (preserve exactly)

This is a single user-facing on/off control that orchestrates the motorized
valve + drain valve. Implement as a state machine in `core`.

- **Turn ON sequence:** drain OFF → close OFF → wait 1s → open ON → wait 15s →
  publish state ON. (Open coil auto-offs after 14s as above.)
- **Turn OFF sequence:** open OFF → wait 1s → close ON → wait 15s → drain ON →
  publish state OFF. (Close coil auto-offs after 14s; drain auto-offs after 5min.)
- Persist the desired state across reboots (NVS), default OFF on first boot.
- Reject overlapping transitions (do not start a new sequence while one is in
  progress).

## Architectural decisions (locked, do not relitigate)

| Area               | Decision                                                                                          |
|--------------------|----------------------------------------------------------------------------------------------------|
| Rust stack         | `esp-idf-svc` + `esp-idf-hal` (std on ESP-IDF). **Not** `esp-hal`/embassy.                        |
| HA integration     | MQTT + Home Assistant Discovery (auto-creates entities). **Not** ESPHome native API. **Not** Matter. |
| MQTT client        | `esp-idf-svc`'s MQTT client (wraps ESP-IDF's C lib, reuses already-linked mbedTLS). **Not** `rumqttc`. |
| Web UI             | Embedded SPA bundled into firmware via `include_bytes!`, served from on-device HTTPD. SPA framework: TBD (Preact/Svelte/vanilla — pick during milestone 3). |
| Network logging    | WebSocket log tab in the web UI **and** raw TCP/telnet on port 23. Both tee from one ring buffer. |
| WiFi               | Multi-SSID with failover; AP+captive-portal fallback when no known SSID is reachable.             |
| OTA                | HTTP-based (push from web UI), A/B partitions with auto-rollback on boot failure.                 |
| VPN                | Wireguard via the `esp_wireguard` ESP-IDF component (FFI shim).                                   |
| Config storage     | NVS-backed runtime config (calibrations, schedules, broker URL, WiFi creds, etc.) editable from the web UI without reflashing. |
| Scheduling         | Cron-like sprinkler schedules editable from the web UI (replaces the commented-out `on_time` block in the YAML). |
| Time               | SNTP, default `Europe/Madrid`, default servers `0..2.es.pool.ntp.org` (override-able in NVS).     |
| Workspace          | Cargo workspace, three crates: `core` (portable), `firmware` (ESP32), `host` (native).            |

## Workspace layout

```
watercontroller/
├── Cargo.toml                  # workspace root (members = ["crates/*"])
├── rust-toolchain.toml         # pinned Xtensa toolchain via espup
├── PLAN.md                     # this file
├── ref/
│   └── watercontroller_esphome.yaml   # original spec — read-only reference
├── crates/
│   ├── core/                   # portable, no esp-idf-* dependencies
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs       # Wifi, Adc, GpioOut, PulseCounter, NvsStore, Clock, Mqtt
│   │       ├── water_valve.rs  # composite open/close/drain state machine
│   │       ├── switch.rs       # GPIO output with optional auto-off timer
│   │       ├── calibration.rs  # piecewise-linear interpolation + multi-stage chains
│   │       ├── schedule.rs     # cron-like evaluator
│   │       ├── config.rs       # serde-able runtime config schema + NVS load/save
│   │       ├── ha_discovery.rs # builds HA Discovery JSON payloads
│   │       ├── api.rs          # HTTP request/response types + handlers as pure fns
│   │       ├── log_buffer.rs   # ring buffer + log::Log impl + sink trait
│   │       └── state.rs        # shared device-state type (sensors + switches)
│   ├── firmware/               # cargo build --target xtensa-esp32-espidf
│   │   ├── Cargo.toml
│   │   ├── build.rs
│   │   ├── sdkconfig.defaults
│   │   ├── partitions.csv
│   │   └── src/
│   │       ├── main.rs         # init, take peripherals, spawn tasks
│   │       ├── hw_adc.rs       # impl Adc using esp-idf-hal
│   │       ├── hw_pcnt.rs      # impl PulseCounter using esp-idf-hal
│   │       ├── hw_gpio.rs      # impl GpioOut using esp-idf-hal
│   │       ├── hw_clock.rs     # impl Clock using std::time + SNTP
│   │       ├── hw_nvs.rs       # impl NvsStore using esp-idf-svc
│   │       ├── net_wifi.rs     # multi-SSID supervisor + AP fallback
│   │       ├── net_mdns.rs
│   │       ├── net_ota.rs      # HTTP OTA + A/B + rollback
│   │       ├── net_wg.rs       # esp_wireguard FFI shim
│   │       ├── mqtt_client.rs  # impl Mqtt using esp-idf-svc
│   │       ├── http_server.rs  # esp-idf HTTPD + WebSocket
│   │       ├── log_telnet.rs   # raw TCP log server
│   │       └── assets.rs       # include_bytes! for built SPA
│   └── host/                   # cargo build --bin host (native)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── fakes.rs        # FakeAdc, FakeGpio, FakePulseCounter, FakeNvs, FakeClock, FakeMqtt
│           └── http_server.rs  # axum or similar; same routes as firmware
└── ui/                         # SPA source — built artifact bundled into firmware
    ├── package.json
    ├── vite.config.ts
    └── src/
```

### What lives where (the rule that must not be broken)

- `core/` MUST compile on `x86_64-unknown-linux-gnu` without ESP-IDF. **No
  `esp-idf-*` imports** in `core`. If you find yourself wanting one, add a
  trait in `core/traits.rs` and implement it in `firmware` and `host` instead.
- `firmware/` is the only crate that depends on `esp-idf-svc` / `esp-idf-hal` /
  ESP-IDF C components.
- `host/` depends only on `core/` plus host-friendly libs (axum/tokio/clap).
- The HTTP API surface is defined as pure functions in `core::api` taking a
  state ref — both `firmware` and `host` HTTP servers wire requests into the
  same handlers, ensuring identical behavior.

## Core trait surface (sketch — refine in milestone 2)

```rust
// crates/core/src/traits.rs

pub trait Clock {
    fn now(&self) -> chrono::DateTime<chrono::Utc>;
    fn monotonic_ms(&self) -> u64;
}

pub trait GpioOut {
    fn set(&mut self, high: bool);
}

pub trait Adc {
    /// Returns raw counts (0..4095 for 12-bit) — calibration is applied in core.
    fn read_raw(&mut self) -> u16;
}

pub trait PulseCounter {
    /// Returns the cumulative count since boot, monotonically increasing.
    fn count(&self) -> u64;
}

pub trait NvsStore {
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    fn set(&mut self, key: &str, value: &[u8]) -> Result<(), NvsError>;
    fn remove(&mut self, key: &str) -> Result<(), NvsError>;
}

pub trait Mqtt {
    fn publish(&self, topic: &str, payload: &[u8], retained: bool, qos: u8);
    fn subscribe(&self, topic: &str);
    /// Returns a stream/channel of incoming messages.
    fn incoming(&self) -> Box<dyn Iterator<Item = MqttMessage> + Send>;
}

pub trait Wifi {
    fn state(&self) -> WifiState; // Connected{ssid, ip} | ApMode | Connecting | Disconnected
    fn connect(&mut self, networks: &[WifiCreds]);
}
```

These are the only abstractions `core` needs. Resist adding more — every trait
is a maintenance cost. If a piece of logic doesn't need to talk to hardware,
it doesn't need a trait at all; just write a pure function.

## Status (snapshot as of 2026-05-10)

| Milestone | Status | Notes |
|-----------|--------|-------|
| M1 Skeleton & build pipeline | ✅ Done | Workspace + container build via `scripts/firmware.sh`. ELF built (~26 MB debug, ~2 MB release). |
| M2 Trait surface + fakes | ✅ Done | All traits in `core::traits`. Host fakes in `host/src/fakes.rs`. Firmware skeletons in `firmware/src/hw_*.rs` — placeholders for ADC/PCNT/GPIO. |
| M3 WiFi multi-SSID + AP fallback | ✅ Compiles, **untested on hardware** | `firmware::net_wifi::WifiSupervisor` runs its own thread, walks the configured SSID list with retry, falls back to AP+captive-portal mode when none reach. |
| M4 HTTP server + SPA + logs | ✅ Done | Embedded vanilla-JS SPA (Dashboard + Settings tabs), HTTPD on firmware, axum on host. Live logs over **WebSocket** + telnet (TCP/23). WS verified through QEMU. |
| M5 Sensors | ✅ Compiles, hardware path untested | `firmware::hw_adc::EspAdcChan` (oneshot ADC1) + `firmware::hw_pcnt::EspPulseCounter` (legacy PCNT, 16→64 bit accumulator). The `qemu` feature swaps both for placeholders because qemu's ADC/PCNT models hang or null-deref on init. |
| M6 Switches + valve sequencer | ✅ Compiles, GPIO writes verified through tick task | `firmware::hw_gpio::EspGpioOut` wraps each output pin; tick task drives valve coils + sprinkler GPIOs every 10 ms from the core state machine. QEMU run shows water_control transitions through `Off→Transitioning→On` while real GPIO writes happen each tick. |
| M7 MQTT + HA Discovery | ✅ Compiles, **untested on hardware** | `firmware::mqtt_client::EspMqtt` wraps `esp-idf-svc`'s `EspMqttClient`. Supervisor task connects after STA up, publishes Discovery + retained state on (re)connect, routes commands. |
| M8 NVS-backed runtime config | ✅ Done | Schema in `core::config`. Firmware loads on boot, persists on edit (60s coalescing thread). |
| M9 Schedule engine | ✅ Done | `core::schedule` with cron-like rules. Firmware spawns the executor at 30 s tick. Local-time conversion uses a fixed +01:00 offset until chrono-tz is added. |
| M10 OTA | ✅ Compiles, host-untested rollback | `POST /api/ota` streams body into inactive slot via `EspOta::initiate_update`/`write`/`complete`, then reboots. Boot path calls `mark_running_slot_valid()` after self-test passes. Rollback only verifiable on hardware. |
| M11 Wireguard | ⏳ Stub | Skeleton at `firmware::net_wg`. Highest risk; should be last per plan. |

**What's tested:** `cargo test -p watercontroller-core` exercises calibration, valve sequencing (with fake clock), schedule evaluation (incl. missed-minute recovery and DST-jump cap), HA Discovery payload shapes, MQTT command routing, NVS round-trip, log ring buffer eviction. **Hardware-attached behaviors** (real WiFi, ADC/PCNT, GPIO levels, OTA rollback) are not covered. The QEMU smoke (see "QEMU emulation" section) covers boot, HTTPD, WS logs, OTA upload accept, factory reset, auth round-trips.

**What's known to not yet work on real hardware:**
- ADC reads return a placeholder constant.
- PCNT (water flow) returns 0.
- GPIO outputs from the valve sequencer are not connected to physical pins.
- MQTT client doesn't connect anywhere (compiles + dispatches; no real broker reached yet).
- Wireguard tunnel cannot be brought up.

## Build milestones

Each milestone has explicit success criteria. Do not advance until they pass.

### Milestone 1 — Skeleton & build pipeline

- Cargo workspace exists with `core`, `firmware`, `host` members.
- `rust-toolchain.toml` pins the Xtensa toolchain (via `espup`).
- `crates/firmware` boots a "hello, world" log line on real ESP32 hardware
  (or in QEMU if hardware unavailable — flag this).
- `crates/host` runs natively, prints "hello, world".
- `cargo build` from the workspace root succeeds for both targets (firmware
  build invoked with the appropriate target).
- `cargo test -p core` runs (no tests yet, just the harness).
- `partitions.csv` defined with `ota_0`, `ota_1`, `nvs`, `phy_init`, and a
  small partition for SPA assets (or use `EMBED_FILES` if going that route).

**Done when:** firmware flashes & boots; `cargo test -p core` is green;
`cargo run -p host` prints something.

### Milestone 2 — Core trait surface + fakes

- `core/traits.rs` finalized (start from the sketch above; adjust as needed).
- `host/fakes.rs` implements every trait with simple in-memory fakes.
- Stub `firmware/hw_*.rs` files implementing each trait against `esp-idf-hal`,
  even if not yet wired into anything.

**Done when:** both `firmware` and `host` link against `core` with all traits
satisfied; nothing functional yet but the seams are in place.

### Milestone 3 — WiFi + multi-SSID + AP fallback

- `firmware/net_wifi.rs`: scan known SSIDs, connect with retry/backoff, on
  total failure switch to AP mode with captive portal.
- AP SSID default: `Doremorwater Fallback Hotspot`, no password (matches YAML;
  override-able from NVS later).
- mDNS advertises `doremorwater.local`.
- `host` stubs WiFi as "always connected" — do not invest in faking radio.

**Done when:** device connects to known WiFi; powering off the AP makes it
fall back to its own AP within ~30s; phone can join AP and reach
`http://192.168.4.1` (placeholder page).

### Milestone 4 — HTTP server + SPA shell + logging

- HTTPD on port 80 (firmware) / configurable port (host).
- Serves a placeholder SPA from embedded bytes.
- WebSocket endpoint `/ws/logs` streams from `core::log_buffer`.
- Telnet log server on TCP/23 (firmware only — skip on host or use stdout).
- A `log::Log` impl writes every record to the ring buffer; the existing
  ESP-IDF logger continues to write to UART.
- Pick the SPA framework now (default recommendation: **Preact** for size, or
  vanilla JS if no toolchain is preferred). Document the choice in this file.

**Done when:** browsing to the device's IP shows the SPA shell; opening the
"Logs" tab shows live log lines from the device; `nc <ip> 23` shows the same
stream; `cargo run -p host` exposes the same endpoints on localhost.

### Milestone 5 — Sensors

- ADC oneshot reads with ESP-IDF calibration eFuse curve fitting.
- Pulse counter via PCNT peripheral.
- `core::calibration` implements piecewise-linear interpolation and
  composition (the pressure sensor's two-stage calibration must compose
  cleanly).
- Sliding-window moving average for battery (window 15, publish every 2nd).
- Sensor task publishes readings into shared state at the cadences from the
  YAML (battery 10min, pressure 1min, flow 1min).
- SPA renders current values.

**Done when:** all three sensors read sensible values on real hardware;
calibration math has unit tests in `core` covering the YAML's exact data
points; values appear in the SPA.

### Milestone 6 — Switches & water-valve sequencer

- `core::switch::TimedSwitch` — GPIO output with optional auto-off duration.
- `core::water_valve::WaterValve` — state machine implementing the composite
  ON/OFF sequences exactly as specified in the Hardware section.
- SPA exposes individual switches + the composite "Water control".
- The composite refuses to start a transition while one is in progress.

**Done when:** Unit tests in `core` (using a fake clock) verify both
sequences fire the right GPIO calls in the right order at the right times;
hardware test confirms the valve actually opens and closes.

### Milestone 7 — MQTT + HA Discovery

- `firmware/mqtt_client.rs` implements `core::traits::Mqtt` over
  `esp-idf-svc`'s MQTT client.
- LWT topic publishes `offline` retained; on connect publish `online` retained.
- `core::ha_discovery::publish_all(&mqtt, &state)` emits one retained
  `homeassistant/.../config` topic per entity (sensors, switches, the
  composite water control).
- State updates published as JSON to per-entity state topics.
- Command topics subscribed; commands routed to the same handlers used by the
  HTTP API.

**Done when:** the device appears in Home Assistant with all entities
auto-created; toggling switches from HA works and reflects in the SPA;
unplugging the device makes HA show it offline within keepalive; snapshot
tests in `core` verify HA discovery JSON payloads.

### Milestone 8 — NVS-backed runtime config + web editor

- `core::config::Config` — single serde-able struct holding everything
  configurable: WiFi creds (list), MQTT broker URL/creds, calibration tables,
  schedules, timezone, hostname, AP password override.
- Loaded from NVS on boot; sensible defaults if absent.
- `PUT /api/config` validates and persists; `GET /api/config` returns current.
- SPA "Settings" tab edits sections of it. Sensitive fields (passwords) write-only.
- WiFi creds change triggers a reconnect.

**Done when:** changing pressure calibration via SPA persists across reboot
and changes the published values without reflashing; changing WiFi creds and
rebooting joins the new network.

### Milestone 9 — Schedule engine

- `core::schedule::Schedule` — list of cron-like rules `{ hours, minutes,
  days_of_week, action }` where `action` references a switch ID or the
  composite water control + a duration.
- Evaluator runs once per minute, fires due actions.
- Editable from SPA "Schedules" tab; persisted in NVS.
- Restore the YAML's commented `on_time` block as the default schedule.

**Done when:** scheduled triggers fire on hardware at the configured times;
unit tests in `core` cover edge cases (DST transition, missed minutes after
NTP sync).

### Milestone 10 — OTA

- HTTPS-or-HTTP push endpoint accepts a firmware binary and writes to the
  inactive OTA partition.
- After flash, sets pending-verify and reboots.
- On boot, after successful self-test (WiFi up, HTTPD up), marks the new
  partition valid; otherwise rolls back.
- SPA "Update" tab provides upload UI + version display.

**Done when:** uploading a new firmware image via SPA reboots into the new
version; intentionally-broken upload rolls back automatically on the next
boot.

### Milestone 11 — Wireguard

- `firmware/net_wg.rs` shims the `esp_wireguard` C component via
  `idf_component.yml` + `unsafe extern "C"` bindings.
- Tunnel config (private key, peer public key, peer endpoint, peer PSK,
  allowed IPs, keepalive) lives in NVS; editable from SPA "VPN" tab.
- Brought up after WiFi is connected; brought down before sleep/reboot.
- Restore the YAML's commented Wireguard parameters as a starting reference
  but do NOT bake the keys into source — they live in NVS only.

**Done when:** with a Wireguard peer configured, the device is reachable on
the tunnel address (the YAML's `10.6.0.5` is the historical address — keep
this as the default for continuity).

## QEMU emulation

Espressif's QEMU fork (prebuilt `qemu-xtensa-softmmu-esp_develop_*` tarball
extracted to `~/.local/qemu-xtensa`) boots the firmware end-to-end **with
working network**. Use `scripts/qemu.sh`:

```sh
scripts/qemu.sh                                # build + merge + boot
curl http://127.0.0.1:18080/api/status         # API reachable from host
nc 127.0.0.1 18023                             # live device logs (telnet)
```

What's verified under QEMU:
- ESP-IDF bootloader, partition table (4 MB), OTA-0 selection.
- All ESP-IDF init (heap, spi_flash, NVS, esp_event).
- Rust `app_main` + tee logger + telnet log server (TCP/23).
- NVS config load + persistence.
- `open_eth` → ESP-IDF Ethernet driver → lwIP netif (10.0.2.15 via DHCP from
  qemu's user-mode network).
- All HTTPD routes (`/`, `/api/status`, `/api/config` GET/PUT, `/api/switch`,
  `/api/factory_reset`).
- **Full HTTP round-trip from host through `hostfwd` ports** —
  `POST /api/switch` toggles state, `GET /api/status` reflects it; valve
  state machine reports `Transitioning` mid-sequence.
- Schedule + sensor + tick tasks all spawn.
- Sensor pipeline (placeholder ADC values are calibration-applied and visible
  in `/api/status`).
- Diagnostic sensors: free heap, min heap, reset reason, uptime.

What's **not** verified under QEMU (and why):
- WiFi — qemu's WiFi peripheral shim asserts in lwIP. The `qemu` Cargo
  feature swaps WiFi for `open_eth` instead. Real WiFi behavior needs
  hardware.
- ADC/PCNT real values — `PlaceholderAdc`/`PlaceholderPcnt` return constants;
  swapping in real wrappers is a hardware-only milestone.
- mDNS — the `mdns` ESP-IDF component is in managed_components, not pulled in
  yet; `mdns_init.rs` is a logging stub for now.
- Wireguard — same story, gated until M11.

**Bottom line:** QEMU now reproduces the device closely enough to validate
config flows, schedule firings, MQTT dispatch logic (against a host-side
broker), valve state machine, and SPA/JSON contracts without flashing.

## Build environment

The firmware crate is built inside the `espressif/idf-rust:esp32_latest`
container, not on the host. The host's job is only to invoke the container.

**Why containerized:** the espup-installed Xtensa Rust toolchain links
against `GLIBCXX_3.4.30+`, which RHEL 9's libstdc++ does not provide. The
container ships its own libstdc++ and avoids the mismatch entirely. As a
side benefit, the build is reproducible across machines.

**Wrapper:** `scripts/firmware.sh` runs cargo inside the container with the
workspace mounted at `/project` and persistent caches at:

- `~/.cache/watercontroller-cargo/registry`  — Cargo registry
- `~/.cache/watercontroller-cargo/git`       — Cargo git deps
- `~/.cache/watercontroller-espressif`       — ESP-IDF + tools

Build artifacts go to `target/firmware/` (separate from the host build's
`target/` so target dirs don't collide between host and Xtensa builds).

**Common commands:**

```sh
# Build firmware
scripts/firmware.sh build

# Type-check fast
scripts/firmware.sh check

# Drop into the container shell for debugging
scripts/firmware.sh shell

# Run host + core natively (no container needed)
cargo run -p watercontroller-host
cargo test -p watercontroller-core
```

**Flashing:** runs on the host (not the container, since the container does
not have access to USB serial devices). Install `espflash` on the host once
the toolchain situation is sorted, or use `cargo-espflash` from the
container with `--device` passthrough if needed.

## Risks & gotchas

- **TLS on Xtensa:** `rustls` does not build cleanly. Use ESP-IDF's mbedTLS
  via `esp-idf-svc` (already the chosen MQTT client). If a TLS-using crate
  pulls `rustls` transitively, replace it.
- **Wireguard FFI:** the `esp_wireguard` component evolves; pin a known-good
  version in `idf_component.yml`. The FFI shim is the highest-risk milestone
  — leave it for last.
- **PCNT API differences:** ESP-IDF v5 reworked PCNT. Use the v5 API; reject
  copy-paste from older v4 examples.
- **Captive portal:** requires DNS hijacking (return device IP for any A
  record) — this is a known pattern in ESP-IDF examples; reuse it.
- **Power consumption at 80 MHz:** the YAML pins CPU to 80 MHz for power
  reasons. Keep this in `sdkconfig.defaults`. Light sleep is not in scope
  unless explicitly requested later.
- **Secrets in YAML:** the reference YAML contains placeholder values
  (`"xxxxx"`, `"xxxxxx"`) for API encryption / OTA password. Treat these as
  redacted; do not import them and do not check real secrets into source.
- **Sprinkler labeling drift:** YAML names say "20min" / "10min" but the
  `delay` actions are 7min / 5min. Preserve the **action timings**, fix the
  **labels** during the port.
- **Two GPIOs per motorized valve:** open and close are separate coils that
  each pulse for 14s. Do not energize them simultaneously — the state
  machine in `core::water_valve` is the single owner that prevents this.

## Out of scope (do not add unless the user requests)

- Reimplementing the ESPHome native API protocol.
- Matter / Thread support.
- BLE provisioning (captive portal AP serves this need).
- Battery-powered deep-sleep operation.
- Multi-device coordination / cluster features.
- Anything labeled "future" without a concrete user request behind it.

## How to use this plan

If you are an agent starting work:

1. Read `ref/watercontroller_esphome.yaml` end-to-end before writing code.
2. Verify the milestone you are working on in this document; do not skip ahead.
3. Each milestone's "Done when" is the contract — tests must back it up where
   the milestone touches `core`.
4. When you make a non-trivial decision not covered here (SPA framework,
   library choice, partition sizing), document it inline in this file under
   the relevant section so the next agent inherits it.
5. Do not relitigate decisions in the "Architectural decisions (locked)"
   table without explicit user approval.
