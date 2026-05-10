//! HTTP server: serves the embedded SPA and the JSON API.
//!
//! Routes mirror `core::api::routes`. WebSocket logs deferred until a
//! follow-up — the SPA polls `/api/status` for now and uses the telnet log
//! port for live logs.

use crate::assets::INDEX_HTML;
use anyhow::Result;
use esp_idf_svc::http::server::ws::EspHttpWsDetachedSender;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use esp_idf_svc::sys::EspError;
use esp_idf_svc::ws::FrameType;
use std::sync::{Arc, Mutex};
use watercontroller_core::api::{routes, ApiError, CommandOutcome, ConfigUpdate, SwitchCommand};
use watercontroller_core::app::App;
use watercontroller_core::config::Config;
use watercontroller_core::log_buffer;
use watercontroller_core::traits::NvsStore;

const READ_BUF_LEN: usize = 1024;
const MAX_BODY: usize = 32 * 1024;

/// If `app.config().admin_token` is non-empty, require an
/// `Authorization: Bearer <token>` header. Returns `Ok(())` to proceed,
/// or writes a 401 response and returns `Err(())` to short-circuit the
/// caller.
fn require_auth(
    req: &esp_idf_svc::http::server::Request<&mut esp_idf_svc::http::server::EspHttpConnection<'_>>,
    app: &App,
) -> Result<(), ()> {
    let cfg_token = app.config().admin_token;
    if cfg_token.is_empty() {
        return Ok(());
    }
    let header = req.header("Authorization").unwrap_or("");
    if header.strip_prefix("Bearer ") == Some(&cfg_token) {
        Ok(())
    } else {
        Err(())
    }
}

fn write_unauthorized(
    req: esp_idf_svc::http::server::Request<&mut esp_idf_svc::http::server::EspHttpConnection<'_>>,
) -> Result<(), EspIOError> {
    let body = serde_json::to_vec(&ApiError::new(
        "missing or invalid Authorization: Bearer <admin_token>",
    ))
    .unwrap_or_default();
    let mut resp = req.into_response(401, None, JSON_CT)?;
    resp.write_all(&body)?;
    Ok(())
}

const JSON_CT: &[(&str, &str)] = &[("Content-Type", "application/json")];
const HTML_CT: &[(&str, &str)] = &[("Content-Type", "text/html; charset=utf-8")];

pub fn spawn(
    app: App,
    nvs: Arc<dyn NvsStore>,
    port: u16,
) -> Result<EspHttpServer<'static>> {
    let cfg = esp_idf_svc::http::server::Configuration {
        http_port: port,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&cfg)?;

    server.fn_handler::<EspIOError, _>("/", Method::Get, |req| {
        let mut resp = req.into_response(200, None, HTML_CT)?;
        resp.write_all(INDEX_HTML)?;
        Ok(())
    })?;

    {
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::STATUS, Method::Get, move |req| {
            let body = serde_json::to_vec(&app.snapshot()).unwrap_or_default();
            let mut resp = req.into_response(200, None, JSON_CT)?;
            resp.write_all(&body)?;
            Ok(())
        })?;
    }

    {
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::CONFIG, Method::Get, move |req| {
            let body = serde_json::to_vec(&app.config()).unwrap_or_default();
            let mut resp = req.into_response(200, None, JSON_CT)?;
            resp.write_all(&body)?;
            Ok(())
        })?;
    }

    {
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::CONFIG, Method::Put, move |mut req| {
            if require_auth(&req, &app).is_err() {
                return write_unauthorized(req);
            }
            let mut buf = Vec::with_capacity(256);
            let mut chunk = [0u8; READ_BUF_LEN];
            loop {
                let n = req.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BODY {
                    let body = serde_json::to_vec(&ApiError::new("body too large")).unwrap_or_default();
                    let mut resp = req.into_response(413, None, JSON_CT)?;
                    resp.write_all(&body)?;
                    return Ok(());
                }
            }
            match serde_json::from_slice::<ConfigUpdate>(&buf) {
                Ok(u) => {
                    app.replace_config(u.0);
                    let _ = req.into_response(204, None, &[])?;
                }
                Err(e) => {
                    let body = serde_json::to_vec(&ApiError::new(format!("invalid json: {e}"))).unwrap_or_default();
                    let mut resp = req.into_response(400, None, JSON_CT)?;
                    resp.write_all(&body)?;
                }
            }
            Ok(())
        })?;
    }

    {
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::SWITCH, Method::Post, move |mut req| {
            if require_auth(&req, &app).is_err() {
                return write_unauthorized(req);
            }
            let mut buf = Vec::with_capacity(128);
            let mut chunk = [0u8; READ_BUF_LEN];
            loop {
                let n = req.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BODY {
                    break;
                }
            }
            match serde_json::from_slice::<SwitchCommand>(&buf) {
                Ok(cmd) => {
                    let outcome = app.switch_command(cmd);
                    let status = match &outcome {
                        CommandOutcome::Ok => 200,
                        CommandOutcome::Busy { .. } => 409,
                    };
                    let body = serde_json::to_vec(&outcome).unwrap_or_default();
                    let mut resp = req.into_response(status, None, JSON_CT)?;
                    resp.write_all(&body)?;
                }
                Err(e) => {
                    let body = serde_json::to_vec(&ApiError::new(format!("invalid json: {e}"))).unwrap_or_default();
                    let mut resp = req.into_response(400, None, JSON_CT)?;
                    resp.write_all(&body)?;
                }
            }
            Ok(())
        })?;
    }

    // GET /ws/logs → live log streaming via WebSocket. The handler stores a
    // `EspHttpWsDetachedSender` for each new session; a background thread
    // subscribed to the in-memory log ring buffer fans out each record.
    let ws_senders: Arc<Mutex<Vec<EspHttpWsDetachedSender>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let senders = ws_senders.clone();
        // ws_handler's generic order is <H, E> (different from fn_handler's <E, F>).
        server.ws_handler::<_, EspError>(routes::LOGS_WS, move |conn: &mut esp_idf_svc::http::server::ws::EspHttpWsConnection| {
            if conn.is_new() {
                // Send the recent backlog (last 200 records) so clients see
                // history immediately on connect, then enroll the sender for
                // future fan-out.
                if let Some(buf) = log_buffer::global() {
                    for rec in buf.snapshot(200) {
                        let line = rec.formatted();
                        if conn.send(FrameType::Text(false), line.as_bytes()).is_err() {
                            return Ok(());
                        }
                    }
                }
                if let Ok(sender) = conn.create_detached_sender() {
                    senders.lock().unwrap().push(sender);
                }
            }
            // We don't care about incoming frames — the SPA only consumes.
            // Closed sessions are cleaned up lazily by the fan-out thread.
            Ok(())
        })?;

        // Fan-out thread: drain log records, push to every open sender.
        std::thread::Builder::new()
            .name("ws-log-fanout".into())
            .stack_size(8 * 1024)
            .spawn(move || {
                let Some(buf) = log_buffer::global() else {
                    return;
                };
                let (_id, rx) = buf.subscribe(256);
                while let Ok(rec) = rx.recv() {
                    let line = rec.formatted();
                    let bytes = line.as_bytes();
                    let mut guard = ws_senders.lock().unwrap();
                    guard.retain_mut(|s: &mut EspHttpWsDetachedSender| {
                        if s.is_closed() {
                            return false;
                        }
                        s.send(FrameType::Text(false), bytes).is_ok()
                    });
                }
            })
            .ok();
    }

    // POST /api/ota → stream firmware image into the inactive OTA partition.
    {
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::OTA_UPLOAD, Method::Post, move |mut req| {
            if require_auth(&req, &app).is_err() {
                return write_unauthorized(req);
            }
            log::info!("ota: upload starting");
            let mut total = 0usize;
            // Drive crate::net_ota::apply_image with a reader that pulls from
            // the HTTP connection. Note: we cannot use core's apply_image
            // directly because the EspOtaUpdate's lifetime is tied to the
            // outer EspOta — so we inline the loop here.
            use esp_idf_svc::ota::EspOta;
            let mut ota = match EspOta::new() {
                Ok(o) => o,
                Err(e) => {
                    let body = serde_json::to_vec(&ApiError::new(format!("ota init: {e}")))
                        .unwrap_or_default();
                    let mut resp = req.into_response(500, None, JSON_CT)?;
                    resp.write_all(&body)?;
                    return Ok(());
                }
            };
            let mut update = match ota.initiate_update() {
                Ok(u) => u,
                Err(e) => {
                    let body = serde_json::to_vec(&ApiError::new(format!("ota initiate: {e}")))
                        .unwrap_or_default();
                    let mut resp = req.into_response(500, None, JSON_CT)?;
                    resp.write_all(&body)?;
                    return Ok(());
                }
            };
            let mut buf = vec![0u8; 4096];
            loop {
                match req.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = update.write(&buf[..n]) {
                            let _ = update.abort();
                            let body = serde_json::to_vec(&ApiError::new(format!(
                                "ota write @ {total}: {e}"
                            )))
                            .unwrap_or_default();
                            let mut resp = req.into_response(500, None, JSON_CT)?;
                            resp.write_all(&body)?;
                            return Ok(());
                        }
                        total += n;
                    }
                    Err(e) => {
                        let _ = update.abort();
                        let body = serde_json::to_vec(&ApiError::new(format!(
                            "ota recv @ {total}: {e}"
                        )))
                        .unwrap_or_default();
                        let mut resp = req.into_response(500, None, JSON_CT)?;
                        resp.write_all(&body)?;
                        return Ok(());
                    }
                }
            }
            if total == 0 {
                let _ = update.abort();
                let body = serde_json::to_vec(&ApiError::new("empty image")).unwrap_or_default();
                let mut resp = req.into_response(400, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
            if let Err(e) = update.complete() {
                let body = serde_json::to_vec(&ApiError::new(format!("ota complete: {e}")))
                    .unwrap_or_default();
                let mut resp = req.into_response(500, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
            log::info!("ota: applied {total} bytes; rebooting into new slot");
            let body = serde_json::to_vec(&serde_json::json!({
                "result": "ok", "bytes": total
            }))
            .unwrap_or_default();
            let mut resp = req.into_response(202, None, JSON_CT)?;
            resp.write_all(&body)?;
            std::thread::Builder::new()
                .name("ota-reboot".into())
                .stack_size(2048)
                .spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    crate::net_ota::reboot();
                })
                .ok();
            Ok(())
        })?;
    }

    // POST /api/factory_reset → erase NVS config and reboot.
    {
        let nvs = nvs.clone();
        let app = app.clone();
        server.fn_handler::<EspIOError, _>(routes::FACTORY_RESET, Method::Post, move |req| {
            if require_auth(&req, &app).is_err() {
                return write_unauthorized(req);
            }
            log::warn!("factory reset requested via HTTP");
            if let Err(e) = Config::factory_reset(&*nvs) {
                let body = serde_json::to_vec(&ApiError::new(format!("erase failed: {e}")))
                    .unwrap_or_default();
                let mut resp = req.into_response(500, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
            // 202 Accepted; client gets the response, then the device reboots.
            let _ = req.into_response(202, None, &[])?;
            // Schedule restart after a brief delay so the response actually flushes.
            std::thread::Builder::new()
                .name("reset-reboot".into())
                .stack_size(2048)
                .spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    unsafe { esp_idf_svc::sys::esp_restart() };
                })
                .ok();
            Ok(())
        })?;
    }

    Ok(server)
}
