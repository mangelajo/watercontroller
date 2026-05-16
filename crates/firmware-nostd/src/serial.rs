//! UART serial command console.
//!
//! Reads newline-terminated commands from UART0 RX (GPIO3 — the same
//! USB-serial line `esp-println` logs to) and runs them. Replies go
//! through `log::info!`, so they land on the UART, the `/ws/logs`
//! WebSocket, and the telnet log port alike.
//!
//! This is the only interface that still works when the device is off
//! the network (AP-mode fallback, or no WiFi at all), so the command
//! set covers the recovery essentials: status, reset, alarm-clear.

use alloc::sync::Arc;

use esp_hal::{
    peripherals::{GPIO3, UART0},
    uart::{Config as UartConfig, UartRx},
    Async,
};
use watercontroller_core::{
    app::App,
    config::Config,
    traits::{NvsStore, WifiState},
};

/// Run one command line. Replies via `log::info!`.
fn handle(app: &App, nvs: &dyn NvsStore, line: &str) {
    let cmd = line.trim();
    if cmd.is_empty() {
        return;
    }
    match cmd {
        "help" => {
            log::info!(
                "serial: commands — help | status | diag | reset | config reset | ap | alarm clear"
            );
        }
        "status" => {
            let snap = app.snapshot();
            log::info!(
                "serial: uptime={}s fw={} mqtt={}",
                crate::uptime_secs(),
                snap.firmware_version,
                snap.network.mqtt_connected,
            );
            match &snap.network.wifi {
                Some(WifiState::Connected { ssid, ip }) => {
                    log::info!("serial: wifi connected {} ip={}", ssid, ip)
                }
                Some(WifiState::ApMode { ssid, ip }) => {
                    log::info!("serial: wifi AP-mode {} ip={}", ssid, ip)
                }
                Some(WifiState::Connecting { ssid }) => {
                    log::info!("serial: wifi connecting to {}", ssid)
                }
                _ => log::info!("serial: wifi disconnected"),
            }
            log::info!(
                "serial: switches s1={} s2={} water={:?}",
                snap.switches.sprinkler_1,
                snap.switches.sprinkler_2,
                snap.switches.water_control,
            );
        }
        "diag" => {
            log::info!(
                "serial: heap_free={} heap_used={}",
                esp_alloc::HEAP.free(),
                esp_alloc::HEAP.used(),
            );
        }
        "reset" => {
            log::info!("serial: resetting…");
            crate::ota::request_reboot();
        }
        "config reset" => {
            // Erase the persisted config; the next boot falls back to
            // the compile-time defaults. Pair with `reset` to apply —
            // lets a test harness start from a known clean state.
            match Config::factory_reset(nvs) {
                Ok(()) => log::info!("serial: config erased — run 'reset' to boot defaults"),
                Err(e) => log::info!("serial: config reset failed: {:?}", e),
            }
        }
        "ap" => {
            // Force the SoftAP setup portal: persist the AP boot hint
            // and reboot. AP mode reboots itself back to STA once a
            // configured network is in range again.
            crate::write_boot_mode(nvs, crate::BootMode::Ap);
            log::info!("serial: AP setup mode set — rebooting");
            crate::ota::request_reboot();
        }
        "alarm clear" => {
            app.clear_flow_alarm();
            log::info!("serial: flow alarm cleared");
        }
        other => log::info!("serial: unknown command '{}' (try 'help')", other),
    }
}

#[embassy_executor::task]
pub async fn serial_task(
    app: App,
    nvs: Arc<dyn NvsStore>,
    uart0: UART0<'static>,
    rx_gpio: GPIO3<'static>,
) {
    let mut rx: UartRx<'static, Async> = match UartRx::new(uart0, UartConfig::default()) {
        Ok(r) => r.with_rx(rx_gpio).into_async(),
        Err(e) => {
            log::info!("serial: UART RX init failed: {:?}", e);
            return;
        }
    };
    log::info!("serial: CLI ready — type 'help'");

    // One command line at a time; longer input is discarded.
    let mut line: heapless::String<80> = heapless::String::new();
    let mut buf = [0u8; 32];
    loop {
        let n = match rx.read_async(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        for &b in &buf[..n] {
            match b {
                b'\r' | b'\n' => {
                    handle(&app, &*nvs, &line);
                    line.clear();
                }
                0x08 | 0x7f => {
                    line.pop();
                }
                _ => {
                    if line.push(b as char).is_err() {
                        // Overflow — drop the oversized line.
                        line.clear();
                    }
                }
            }
        }
    }
}
