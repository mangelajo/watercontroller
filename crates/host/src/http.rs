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
use watercontroller_core::api::{routes, ConfigUpdate, SwitchCommand};
use watercontroller_core::log_buffer::LogRecord;

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub log_tx: broadcast::Sender<LogRecord>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route(routes::STATUS, get(get_status))
        .route(routes::CONFIG, get(get_config).put(put_config))
        .route(routes::SWITCH, post(post_switch))
        .route(routes::LOGS_WS, get(ws_logs))
        .route(routes::FACTORY_RESET, post(post_factory_reset))
        .with_state(Arc::new(state))
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
    Json(s.app.config())
}

async fn put_config(
    State(s): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> impl IntoResponse {
    s.app.replace_config(update.0);
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
