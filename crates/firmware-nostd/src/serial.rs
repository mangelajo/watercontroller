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

use esp_hal::{
    peripherals::{GPIO3, UART0},
    uart::{Config, UartRx},
    Async,
};
use watercontroller_core::{app::App, traits::WifiState};

/// Run one command line. Replies via `log::info!`.
fn handle(app: &App, line: &str) {
    let cmd = line.trim();
    if cmd.is_empty() {
        return;
    }
    match cmd {
        "help" => {
            log::info!("serial: commands — help | status | diag | reset | alarm clear");
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
        "alarm clear" => {
            app.clear_flow_alarm();
            log::info!("serial: flow alarm cleared");
        }
        other => log::info!("serial: unknown command '{}' (try 'help')", other),
    }
}

#[embassy_executor::task]
pub async fn serial_task(app: App, uart0: UART0<'static>, rx_gpio: GPIO3<'static>) {
    let mut rx: UartRx<'static, Async> = match UartRx::new(uart0, Config::default()) {
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
                    handle(&app, &line);
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
