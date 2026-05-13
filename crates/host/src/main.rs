//! Native binary for hot-reload UI development and unit-style integration runs.
//! Drives `core::app::App` with fake peripherals over the same HTTP routes
//! used by the firmware build.

mod fakes;
mod http;
mod webhook_dispatch;

use crate::fakes::WallClock;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use watercontroller_core::app::App;
use watercontroller_core::config::Config;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let log_buf = watercontroller_core::log_buffer::init_global(2048);
    log::info!("{}", watercontroller_core::greeting());

    let clock: Arc<dyn watercontroller_core::traits::Clock> = Arc::new(WallClock::new());
    let app = App::new(clock.clone(), Config::default());
    let webhook_dispatcher = Arc::new(
        crate::webhook_dispatch::HostWebhookDispatcher::spawn(app.clone()),
    );
    app.set_webhook_dispatcher(webhook_dispatcher);

    // Bridge the std::sync::mpsc log subscription onto a tokio broadcast so
    // the axum WS handler can subscribe per-connection.
    let (log_tx, _) = broadcast::channel::<watercontroller_core::log_buffer::LogRecord>(256);
    {
        let log_buf = log_buf.clone();
        let log_tx = log_tx.clone();
        std::thread::spawn(move || {
            let (_id, rx) = log_buf.subscribe(256);
            while let Ok(rec) = rx.recv() {
                let _ = log_tx.send(rec);
            }
        });
    }

    // Tick task — drives switches + valve sequencer at 100 Hz, plus stamps
    // a faux wifi connection so the SPA's connection chip looks alive in dev.
    {
        let app = app.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            loop {
                interval.tick().await;
                let _ = app.tick();
                app.update_state(|s| {
                    s.uptime_ms = started.elapsed().as_millis() as u64;
                    if s.firmware_version.is_empty() {
                        s.firmware_version = watercontroller_core::version().into();
                    }
                    if s.network.wifi.is_none() {
                        s.network.wifi = Some(
                            watercontroller_core::traits::WifiState::Connected {
                                ssid: "host-dev".into(),
                                ip: "127.0.0.1".into(),
                            },
                        );
                    }
                });
            }
        });
    }

    let wifi: std::sync::Arc<dyn watercontroller_core::traits::Wifi> =
        std::sync::Arc::new(crate::fakes::FakeWifi::connected_to("host-dev", "127.0.0.1"));
    let state = http::AppState { app, log_tx, wifi };
    let router = http::router(state);

    let bind = std::env::var("WC_HOST_BIND").unwrap_or_else(|_| "127.0.0.1:8765".into());
    log::info!("host server listening on http://{bind}");
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    axum::serve(listener, router).await.expect("axum serve");
}
