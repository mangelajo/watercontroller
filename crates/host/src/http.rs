//! Native HTTP server (axum) that exposes the same routes as the firmware
//! HTTPD. Serves the embedded SPA and the JSON API.

use watercontroller_core::app::App;
use axum::{
    extract::{ws::WebSocketUpgrade, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use tokio::sync::broadcast;
use watercontroller_core::api::{routes, ConfigUpdate, SwitchCommand, WifiScanResponse};
use watercontroller_core::traits::Wifi;
use watercontroller_core::config::{
    HttpsConfig, MqttConfig, SensorsConfig, SwitchesConfig, WifiConfig, WireguardConfig,
};
use watercontroller_core::log_buffer::LogRecord;
use watercontroller_core::schedule::Schedule;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct TimeSection {
    timezone: String,
    sntp_servers: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct AuthSection {
    admin_token: String,
}

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub log_tx: broadcast::Sender<LogRecord>,
    pub wifi: Arc<dyn Wifi>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route(routes::STATUS, get(get_status))
        .route(routes::CONFIG, get(get_config).put(put_config))
        // Per-section endpoints — see firmware/src/http_server.rs for the
        // production implementation. The host mirrors them so Playwright
        // tests exercise the same surface as a real browser would on the
        // device.
        .route("/api/config/wifi",      get(get_wifi).put(put_wifi))
        .route("/api/config/mqtt",      get(get_mqtt).put(put_mqtt))
        .route("/api/config/switches",  get(get_switches).put(put_switches))
        .route("/api/config/sensors",   get(get_sensors).put(put_sensors))
        .route("/api/config/schedule",  get(get_schedule).put(put_schedule))
        .route("/api/config/https",     get(get_https).put(put_https))
        .route("/api/config/wireguard", get(get_wg).put(put_wg))
        .route("/api/config/time",      get(get_time).put(put_time))
        .route("/api/config/auth",      get(get_auth).put(put_auth))
        .route(routes::SWITCH, post(post_switch))
        .route(routes::LOGS_WS, get(ws_logs))
        .route(routes::FACTORY_RESET, post(post_factory_reset))
        .route(routes::WIFI_SCAN, get(get_wifi_scan))
        .route("/api/wifi/reconnect", post(post_wifi_reconnect))
        .with_state(Arc::new(state))
}

async fn get_wifi_scan(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    match s.wifi.scan() {
        Ok(networks) => (StatusCode::OK, Json(serde_json::to_value(&WifiScanResponse { networks }).unwrap())),
        Err(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "message": msg })),
        ),
    }
}

async fn post_wifi_reconnect(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    s.wifi.reconnect();
    StatusCode::NO_CONTENT
}

async fn post_factory_reset() -> impl IntoResponse {
    // The host build has no persistent storage to wipe. Return 501 so the
    // SPA can show "factory reset is firmware-only" cleanly.
    (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({
        "message": "factory reset is only implemented on real devices"
    })))
}

async fn index() -> impl IntoResponse {
    let html = include_str!("../../firmware/assets/index.html");
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}

async fn get_status(State(s): State<Arc<AppState>>) -> Json<watercontroller_core::state::DeviceSnapshot> {
    Json(s.app.snapshot())
}

async fn get_config(State(s): State<Arc<AppState>>) -> Json<watercontroller_core::config::Config> {
    Json(s.app.config().redact_secrets_for_api())
}

async fn put_config(
    State(s): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> impl IntoResponse {
    let mut current = (*s.app.config()).clone();
    current.merge_preserving_secrets(update.0);
    s.app.replace_config(current);
    StatusCode::NO_CONTENT
}

async fn post_switch(
    State(s): State<Arc<AppState>>,
    Json(cmd): Json<SwitchCommand>,
) -> impl IntoResponse {
    let outcome = s.app.switch_command(cmd);
    let status = match &outcome {
        watercontroller_core::api::CommandOutcome::Ok => StatusCode::OK,
        watercontroller_core::api::CommandOutcome::Busy { .. } => StatusCode::CONFLICT,
    };
    (status, Json(outcome))
}

// Per-section config handlers. Same redact-on-GET / merge-on-empty
// semantics as the firmware side.
async fn get_wifi(State(s): State<Arc<AppState>>) -> Json<WifiConfig> {
    Json(s.app.config().redact_secrets_for_api().wifi)
}
async fn put_wifi(State(s): State<Arc<AppState>>, Json(mut new): Json<WifiConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    for net in new.networks.iter_mut() {
        if net.password.is_empty() {
            if let Some(o) = cfg.wifi.networks.iter().find(|n| n.ssid == net.ssid) {
                net.password = o.password.clone();
            }
        }
    }
    cfg.wifi = new;
    let networks = cfg.wifi.networks.clone();
    s.app.replace_config(cfg);
    // Mirror the firmware path: any wifi-config save kicks the supervisor
    // to (re)evaluate the network list and switch AP↔STA as needed.
    s.wifi.connect(&networks);
    StatusCode::NO_CONTENT
}

async fn get_mqtt(State(s): State<Arc<AppState>>) -> Json<MqttConfig> {
    Json(s.app.config().redact_secrets_for_api().mqtt)
}
async fn put_mqtt(State(s): State<Arc<AppState>>, Json(mut new): Json<MqttConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    if new.password.is_empty() { new.password = cfg.mqtt.password.clone(); }
    if new.client_key_pem.is_empty() { new.client_key_pem = cfg.mqtt.client_key_pem.clone(); }
    cfg.mqtt = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_switches(State(s): State<Arc<AppState>>) -> Json<SwitchesConfig> {
    Json(s.app.config().switches.clone())
}
async fn put_switches(State(s): State<Arc<AppState>>, Json(new): Json<SwitchesConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    cfg.switches = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_sensors(State(s): State<Arc<AppState>>) -> Json<SensorsConfig> {
    Json(s.app.config().sensors.clone())
}
async fn put_sensors(State(s): State<Arc<AppState>>, Json(new): Json<SensorsConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    cfg.sensors = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_schedule(State(s): State<Arc<AppState>>) -> Json<Schedule> {
    Json(s.app.config().schedule.clone())
}
async fn put_schedule(State(s): State<Arc<AppState>>, Json(new): Json<Schedule>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    cfg.schedule = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_https(State(s): State<Arc<AppState>>) -> Json<HttpsConfig> {
    Json(s.app.config().redact_secrets_for_api().https)
}
async fn put_https(State(s): State<Arc<AppState>>, Json(mut new): Json<HttpsConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    if new.key_pem.is_empty() { new.key_pem = cfg.https.key_pem.clone(); }
    cfg.https = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_wg(State(s): State<Arc<AppState>>) -> Json<WireguardConfig> {
    Json(s.app.config().redact_secrets_for_api().wireguard)
}
async fn put_wg(State(s): State<Arc<AppState>>, Json(mut new): Json<WireguardConfig>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    if new.private_key.is_empty() { new.private_key = cfg.wireguard.private_key.clone(); }
    if new.peer_preshared_key.is_empty() {
        new.peer_preshared_key = cfg.wireguard.peer_preshared_key.clone();
    }
    cfg.wireguard = new;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_time(State(s): State<Arc<AppState>>) -> Json<TimeSection> {
    let cfg = s.app.config();
    Json(TimeSection {
        timezone: cfg.timezone.clone(),
        sntp_servers: cfg.sntp_servers.clone(),
    })
}
async fn put_time(State(s): State<Arc<AppState>>, Json(new): Json<TimeSection>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    cfg.timezone = new.timezone;
    cfg.sntp_servers = new.sntp_servers;
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn get_auth(State(_s): State<Arc<AppState>>) -> Json<AuthSection> {
    // Always redacted on GET.
    Json(AuthSection { admin_token: String::new() })
}
async fn put_auth(State(s): State<Arc<AppState>>, Json(new): Json<AuthSection>) -> impl IntoResponse {
    let mut cfg = (*s.app.config()).clone();
    if !new.admin_token.is_empty() {
        cfg.admin_token = new.admin_token;
    }
    s.app.replace_config(cfg);
    StatusCode::NO_CONTENT
}

async fn ws_logs(State(s): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    let mut rx = s.log_tx.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        // Send initial snapshot of recent records.
        if let Some(buf) = watercontroller_core::log_buffer::global() {
            for r in buf.snapshot(200) {
                let line = r.formatted();
                if socket
                    .send(axum::extract::ws::Message::Text(line))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        // Stream new records as they arrive.
        while let Ok(rec) = rx.recv().await {
            let line = rec.formatted();
            if socket
                .send(axum::extract::ws::Message::Text(line))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}
