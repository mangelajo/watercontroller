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

/// Well-known captive-portal probe URLs that mobile and desktop OSes hit
/// after joining a new WiFi to decide whether to pop the captive-portal
/// browser. We don't try to differentiate — every probe gets a 302 to the
/// SPA root, which is the trigger every OS recognizes.
const CAPTIVE_PROBE_PATHS: &[&str] = &[
    "/generate_204",                       // Android (Chrome captive)
    "/gen_204",                            // older Android
    "/hotspot-detect.html",                // iOS / macOS
    "/library/test/success.html",          // legacy Apple
    "/connecttest.txt",                    // Windows
    "/ncsi.txt",                           // Windows NCSI
    "/redirect",                           // Windows fallback
    "/success.txt",                        // Firefox (network-check)
];

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

fn leak_cstr(s: &str) -> &'static std::ffi::CStr {
    let mut bytes = s.as_bytes().to_vec();
    if !bytes.last().is_some_and(|&b| b == 0) {
        bytes.push(0);
    }
    let leaked: &'static [u8] = bytes.leak();
    std::ffi::CStr::from_bytes_with_nul(leaked).expect("nul-terminated above")
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

/// Default 6 KiB HTTPD task stack overflowed under PUT /api/config (full
/// Config deserialisation + multiple mutex acquisitions). 12 KiB plain,
/// 16 KiB for TLS (mbedTLS handshake chews a lot of stack).
const HTTP_STACK: usize = 12 * 1024;
const HTTPS_STACK: usize = 16 * 1024;

/// Two server handles — one plain on :80, an optional TLS one on :443. The
/// caller must hold this for the lifetime of the device; dropping shuts the
/// listeners down.
pub struct ServerHandles {
    #[allow(dead_code)]
    pub http: EspHttpServer<'static>,
    #[cfg(esp_idf_esp_https_server_enable)]
    #[allow(dead_code)]
    pub https: Option<EspHttpServer<'static>>,
}

/// Spawn the HTTPD. Always starts plain HTTP on `port`. When `cert_pem` +
/// `key_pem` are both non-empty (and the firmware was built with
/// `CONFIG_ESP_HTTPS_SERVER_ENABLE=y`), additionally starts an HTTPS
/// listener on port 443.
pub fn spawn(
    app: App,
    nvs: Arc<dyn NvsStore>,
    port: u16,
    cert_pem: &str,
    key_pem: &str,
) -> Result<ServerHandles> {
    // Shared WS-fanout sender list — populated by both servers' WS handlers,
    // drained by a single fanout thread spawned at the end of this fn.
    let ws_senders: Arc<Mutex<Vec<EspHttpWsDetachedSender>>> = Arc::new(Mutex::new(Vec::new()));

    // Plain HTTP on `port`.
    let http_cfg = esp_idf_svc::http::server::Configuration {
        http_port: port,
        stack_size: HTTP_STACK,
        ..Default::default()
    };
    let mut http = EspHttpServer::new(&http_cfg)?;
    register_handlers(&mut http, app.clone(), nvs.clone(), ws_senders.clone())?;
    log::info!("http: listening on :{port}");

    // HTTPS on :443 when both PEM blobs are present and the component is
    // compiled in.
    #[cfg(esp_idf_esp_https_server_enable)]
    let https = if !cert_pem.is_empty() && !key_pem.is_empty() {
        // TLS server's parallel-session cap. Browsers (Chromium) speculatively
        // pre-connect with multiple TCP/TLS sessions; with cap=2 they queued
        // behind each other and the SPA load felt sluggish. 4 matches the
        // browser's typical inflight count for an origin and the heap budget
        // (with MBEDTLS_DYNAMIC_BUFFER each idle session is small; only
        // active handshakes burst ~12 KiB).
        let mut tls_cfg = esp_idf_svc::http::server::Configuration {
            http_port: port,
            https_port: 443,
            stack_size: HTTPS_STACK,
            max_open_sockets: 4,
            // The plain server already grabbed the default ctrl_port
            // (32768). esp_https_server uses ctrl_port for an internal
            // signaling socket; running two servers in the same process
            // requires two distinct ports here, otherwise httpd_ssl_start
            // returns ESP_FAIL.
            ctrl_port: 32769,
            ..Default::default()
        };
        tls_cfg.server_certificate = Some(esp_idf_svc::tls::X509::pem(leak_cstr(cert_pem)));
        tls_cfg.private_key = Some(esp_idf_svc::tls::X509::pem(leak_cstr(key_pem)));
        match EspHttpServer::new(&tls_cfg) {
            Ok(mut s) => {
                register_handlers(&mut s, app.clone(), nvs.clone(), ws_senders.clone())?;
                log::info!("https: listening on :443 (TLS)");
                Some(s)
            }
            Err(e) => {
                log::error!("https: failed to start TLS server, continuing HTTP-only: {e}");
                None
            }
        }
    } else {
        log::info!("https: no cert/key configured, HTTP-only on :{port}");
        None
    };
    #[cfg(not(esp_idf_esp_https_server_enable))]
    let _ = (cert_pem, key_pem);

    // Single fan-out thread shared across both servers' WS handlers.
    spawn_ws_fanout(ws_senders);

    Ok(ServerHandles {
        http,
        #[cfg(esp_idf_esp_https_server_enable)]
        https,
    })
}

/// Register every URI handler on the given server. Called once per
/// `EspHttpServer` instance (HTTP and, when present, HTTPS).
fn register_handlers(
    server: &mut EspHttpServer<'static>,
    app: App,
    nvs: Arc<dyn NvsStore>,
    ws_senders: Arc<Mutex<Vec<EspHttpWsDetachedSender>>>,
) -> Result<()> {

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
            // Strip secrets before serialising — the SPA only needs to see
            // public fields. PUT path uses `merge_preserving_secrets` to
            // avoid wiping stored values when the form posts blanks back.
            let body = serde_json::to_vec(&app.config().redact_secrets_for_api())
                .unwrap_or_default();
            let mut resp = req.into_response(200, None, JSON_CT)?;
            resp.write_all(&body)?;
            Ok(())
        })?;
    }

    // GET /api/diag → heap + per-task stack high-water marks. Read-only,
    // unauthenticated (it's already exposed implicitly via /api/status).
    server.fn_handler::<EspIOError, _>("/api/diag", Method::Get, |req| {
        let body = serde_json::to_vec(&crate::diag::snapshot()).unwrap_or_default();
        let mut resp = req.into_response(200, None, JSON_CT)?;
        resp.write_all(&body)?;
        Ok(())
    })?;

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
                    // Merge so empty secret fields in the incoming PUT keep
                    // the stored value (the SPA gets a redacted view on
                    // GET, so blank password / key fields in the form are
                    // the default state, not an explicit clear).
                    let mut current = app.config();
                    current.merge_preserving_secrets(u.0);
                    app.replace_config(current);
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
    // `EspHttpWsDetachedSender` for each new session; a single fan-out
    // thread (spawned by `spawn_ws_fanout`, after every server is built)
    // subscribes to the log ring buffer and pushes records to all open
    // senders, regardless of which server (HTTP or HTTPS) created them.
    {
        let senders = ws_senders.clone();
        server.ws_handler::<_, EspError>(routes::LOGS_WS, move |conn: &mut esp_idf_svc::http::server::ws::EspHttpWsConnection| {
            if conn.is_new() {
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
            Ok(())
        })?;
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
            crate::task_util::spawn_named(c"ota-reboot", 2048, || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                crate::net_ota::reboot();
            });
            Ok(())
        })?;
    }

    // Captive-portal probe URLs. Phones / OSes hit these well-known paths
    // when joining a new SSID to detect "is this network captive?". Any 3xx
    // response triggers the OS captive-portal popup, which then loads our
    // SPA. We answer all of them with a 302 to the device's root.
    for path in CAPTIVE_PROBE_PATHS {
        server.fn_handler::<EspIOError, _>(path, Method::Get, |req| {
            let _ = req.into_response(302, None, &[("Location", "/")])?;
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
            crate::task_util::spawn_named(c"reset-reboot", 2048, || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe { esp_idf_svc::sys::esp_restart() };
            });
            Ok(())
        })?;
    }

    Ok(())
}

/// Drain the log ring buffer and fan-out each record to every open WS
/// sender. Single thread, shared across both HTTP and HTTPS servers.
fn spawn_ws_fanout(senders: Arc<Mutex<Vec<EspHttpWsDetachedSender>>>) {
    crate::task_util::spawn_named(c"ws-log-fanout", 8 * 1024, move || {
            let Some(buf) = log_buffer::global() else {
                return;
            };
            let (_id, rx) = buf.subscribe(256);
            while let Ok(rec) = rx.recv() {
                let line = rec.formatted();
                let bytes = line.as_bytes();
                let mut guard = senders.lock().unwrap();
                guard.retain_mut(|s: &mut EspHttpWsDetachedSender| {
                    if s.is_closed() {
                        return false;
                    }
                    s.send(FrameType::Text(false), bytes).is_ok()
                });
            }
    });
}
