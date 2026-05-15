# firmware-nostd

Pure-Rust no_std port of the doremorwater firmware. Replaces the
`esp-idf-svc`-based `crates/firmware` with `esp-hal` + `esp-wifi` +
`embassy` + `embedded-tls` to escape ESP-IDF's mbedtls/httpd/pthread
quality issues.

Status: **WIP, do not use yet.** Tracked in tasks N1–N12 (see TaskList).

## Why

The IDF-based firmware works but ships a class of recurring failures
that come from the C stack (mbedtls cipher-storm DRAM fragmentation,
httpd CLOSE_WAIT wedges, pthread cond_var races in BlockingWifi).
None of those are Rust problems and none are fixable from the wrapper
level — they live in IDF C code.

The bet: a pure-Rust async stack (embassy + smoltcp + embedded-tls
+ picoserve) trades IDF's mature WPA2-Enterprise / OTA-rollback / mDNS
implementations for a stack we can actually own end-to-end.

## What's reusable from `crates/firmware`

- `crates/core` — all platform-independent logic (schedule, switches,
  valve sequencer, webhooks, HA discovery, NVS schema, HTTP API types,
  cron). Needs a no_std audit (replace `std::sync::Mutex` → embassy
  Mutex; `std::collections::VecDeque` → already in `alloc`).
- `crates/firmware/assets/index.html` — the SPA. Just include_bytes!
  it from the same path.
- `crates/firmware/partitions.csv` — same OTA A/B layout.

## What's new here

- async task graph instead of FreeRTOS pthreads
- embedded-tls instead of mbedtls
- smoltcp via embassy-net instead of LWIP
- Manual OTA driver (esp-storage + bootloader rollback flag) instead
  of esp_https_ota

## Build

TBD — pending toolchain selection from N1 research.
