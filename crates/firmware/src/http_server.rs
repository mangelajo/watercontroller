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
use watercontroller_core::config::{
    Config, HttpsConfig, MqttConfig, SensorsConfig, SwitchesConfig, WifiConfig, WireguardConfig,
};
use watercontroller_core::log_buffer;
use watercontroller_core::schedule::Schedule;
use watercontroller_core::traits::NvsStore;

#[derive(serde::Serialize, serde::Deserialize)]
struct TimeSection {
    timezone: String,
    sntp_servers: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AuthSection {
    admin_token: String,
}

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
/// Config deserialisation + multiple mutex acquisitions). With per-section
/// config endpoints (added Q2 2026), the largest body the dispatcher
/// deserialises is the HTTPS section (cert + key ~750 B) — so 8 KiB plain
/// and 12 KiB TLS comfortably cover peak usage (measured ~3 KiB) with
/// headroom for mbedTLS handshake stack on the secure side.
const HTTP_STACK: usize = 8 * 1024;
const HTTPS_STACK: usize = 12 * 1024;

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

    // Plain HTTP on `port`. max_uri_handlers default is 32 — we register
    // ~34 routes (8 captive-portal probes + 9 per-section pairs + 8 base
    // routes + WS), so bump. Built as explicit field assignment because
    // struct-literal-with-spread (`..Default::default()`) was silently
    // dropping the override in this codebase — boot config printed 32.
    let mut http_cfg = esp_idf_svc::http::server::Configuration::default();
    http_cfg.http_port = port;
    http_cfg.stack_size = HTTP_STACK;
    http_cfg.max_uri_handlers = 64;
    let mut http = EspHttpServer::new(&http_cfg)?;
    register_handlers(&mut http, app.clone(), nvs.clone(), ws_senders.clone())?;
    log::info!("http: listening on :{port}");

    // HTTPS on :443 when both PEM blobs are present and the component is
    // compiled in.
    #[cfg(esp_idf_esp_https_server_enable)]
    let https = if !cert_pem.is_empty() && !key_pem.is_empty() {
        // TLS server's parallel-session cap. Browsers (Chromium) speculatively
        // pre-connect with up to 6 TCP/TLS sessions per origin. 4 is the
        // sweet spot for our heap budget: with cfg-persist removed and
        // MBEDTLS_DYNAMIC_BUFFER on, we have ~28 KiB free / 16 KiB largest
        // contiguous, comfortably above one TLS handshake's ~12 KiB peak.
        // Explicit field assignment (see http_cfg comment for why we
        // don't use struct-literal-with-spread).
        let mut tls_cfg = esp_idf_svc::http::server::Configuration::default();
        tls_cfg.http_port = port;
        tls_cfg.https_port = 443;
        tls_cfg.stack_size = HTTPS_STACK;
        tls_cfg.max_open_sockets = 4;
        tls_cfg.max_uri_handlers = 64;
        // The plain server already grabbed the default ctrl_port (32768).
        // esp_https_server uses ctrl_port for an internal signaling
        // socket; running two servers in the same process requires two
        // distinct ports here, otherwise httpd_ssl_start returns ESP_FAIL.
        tls_cfg.ctrl_port = 32769;
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
        let nvs = nvs.clone();
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
                    // Persist to NVS inline. PUTs are rare (a user clicking
                    // Save) so the ~50-200 ms NVS write latency is fine,
                    // and writing here lets us drop a dedicated polling
                    // task — saves 8 KiB of pthread stack at idle.
                    if let Err(e) = current.save(&*nvs) {
                        log::error!("config: NVS save failed: {e}");
                        let body = serde_json::to_vec(&ApiError::new(format!(
                            "nvs save: {e}"
                        )))
                        .unwrap_or_default();
                        let mut resp = req.into_response(500, None, JSON_CT)?;
                        resp.write_all(&body)?;
                        return Ok(());
                    }
                    log::info!("config: saved to NVS ({} bytes)", buf.len());
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

    // Per-section config endpoints. Each section is GET-able (with secrets
    // redacted same way as full /api/config) and PUT-able with merge-on-
    // empty for the secret fields belonging to that section. Splitting like
    // this lets the SPA save a single tab without round-tripping the full
    // config payload (~1.5 KiB → 100 B–700 B per section), which means
    // smaller request buffers on the server side.
    register_section(
        server,
        "/api/config/wifi",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.wifi.clone(),
        |cfg, mut new: WifiConfig| {
            // Per-network password merge: incoming password empty +
            // matching SSID in stored config → keep stored.
            for net in new.networks.iter_mut() {
                if net.password.is_empty() {
                    if let Some(o) = cfg.wifi.networks.iter().find(|n| n.ssid == net.ssid) {
                        net.password = o.password.clone();
                    }
                }
            }
            cfg.wifi = new;
        },
    )?;
    register_section(
        server,
        "/api/config/mqtt",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.mqtt.clone(),
        |cfg, mut new: MqttConfig| {
            if new.password.is_empty() { new.password = cfg.mqtt.password.clone(); }
            if new.client_key_pem.is_empty() { new.client_key_pem = cfg.mqtt.client_key_pem.clone(); }
            cfg.mqtt = new;
        },
    )?;
    register_section(
        server,
        "/api/config/switches",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.switches.clone(),
        |cfg, new: SwitchesConfig| { cfg.switches = new; },
    )?;
    register_section(
        server,
        "/api/config/sensors",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.sensors.clone(),
        |cfg, new: SensorsConfig| { cfg.sensors = new; },
    )?;
    register_section(
        server,
        "/api/config/schedule",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.schedule.clone(),
        |cfg, new: Schedule| { cfg.schedule = new; },
    )?;
    register_section(
        server,
        "/api/config/https",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.https.clone(),
        |cfg, mut new: HttpsConfig| {
            if new.key_pem.is_empty() { new.key_pem = cfg.https.key_pem.clone(); }
            cfg.https = new;
        },
    )?;
    register_section(
        server,
        "/api/config/wireguard",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| cfg.wireguard.clone(),
        |cfg, mut new: WireguardConfig| {
            if new.private_key.is_empty() { new.private_key = cfg.wireguard.private_key.clone(); }
            if new.peer_preshared_key.is_empty() {
                new.peer_preshared_key = cfg.wireguard.peer_preshared_key.clone();
            }
            cfg.wireguard = new;
        },
    )?;
    register_section(
        server,
        "/api/config/time",
        app.clone(), app.clone(), nvs.clone(),
        |cfg| TimeSection { timezone: cfg.timezone.clone(), sntp_servers: cfg.sntp_servers.clone() },
        |cfg, new: TimeSection| {
            cfg.timezone = new.timezone;
            cfg.sntp_servers = new.sntp_servers;
        },
    )?;
    register_section(
        server,
        "/api/config/auth",
        app.clone(), app.clone(), nvs.clone(),
        |_cfg| AuthSection { admin_token: String::new() }, // GET always returns redacted
        |cfg, new: AuthSection| {
            // Empty incoming token preserves the stored one (consistent
            // with redact-on-GET → empty-on-PUT semantics).
            if !new.admin_token.is_empty() {
                cfg.admin_token = new.admin_token;
            }
        },
    )?;

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
            let started_at = std::time::Instant::now();
            log::info!("ota: upload starting (peer requested)");
            let mut total = 0usize;
            let mut next_progress_at: usize = 256 * 1024; // log every 256 KiB
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
                            log::error!("ota: write failed at {} bytes: {e}", total);
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
                        if total >= next_progress_at {
                            log::info!("ota: {} KiB written", total / 1024);
                            next_progress_at = total + 256 * 1024;
                        }
                    }
                    Err(e) => {
                        log::error!("ota: recv failed at {} bytes: {e}", total);
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
                log::warn!("ota: empty image, aborting");
                let _ = update.abort();
                let body = serde_json::to_vec(&ApiError::new("empty image")).unwrap_or_default();
                let mut resp = req.into_response(400, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
            if let Err(e) = update.complete() {
                log::error!("ota: complete() failed: {e}");
                let body = serde_json::to_vec(&ApiError::new(format!("ota complete: {e}")))
                    .unwrap_or_default();
                let mut resp = req.into_response(500, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
            let dur = started_at.elapsed();
            log::info!(
                "ota: image applied — {} KiB in {:.1}s ({:.0} KiB/s); rebooting into new slot",
                total / 1024,
                dur.as_secs_f64(),
                (total as f64 / 1024.0) / dur.as_secs_f64()
            );
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

/// Register a `GET /api/config/<path>` and `PUT /api/config/<path>` pair
/// for a single config section. The section type `T` must be `Serialize +
/// DeserializeOwned`; `get` extracts the section from the live (redacted)
/// config for the response, `apply` merges incoming → current Config
/// (most callers preserve secret fields that came back empty).
fn register_section<T, G, A>(
    server: &mut EspHttpServer<'static>,
    path: &'static str,
    app_get: App,
    app_put: App,
    nvs: Arc<dyn NvsStore>,
    get: G,
    apply: A,
) -> Result<()>
where
    T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    G: Fn(&Config) -> T + Send + 'static,
    A: Fn(&mut Config, T) + Send + 'static,
{
    server.fn_handler::<EspIOError, _>(path, Method::Get, move |req| {
        let cfg = app_get.config().redact_secrets_for_api();
        let body = serde_json::to_vec(&get(&cfg)).unwrap_or_default();
        let mut resp = req.into_response(200, None, JSON_CT)?;
        resp.write_all(&body)?;
        Ok(())
    })?;
    server.fn_handler::<EspIOError, _>(path, Method::Put, move |mut req| {
        if require_auth(&req, &app_put).is_err() {
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
                let body = serde_json::to_vec(&ApiError::new("body too large"))
                    .unwrap_or_default();
                let mut resp = req.into_response(413, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
        }
        let section: T = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                let body = serde_json::to_vec(&ApiError::new(format!("invalid json: {e}")))
                    .unwrap_or_default();
                let mut resp = req.into_response(400, None, JSON_CT)?;
                resp.write_all(&body)?;
                return Ok(());
            }
        };
        let mut cfg = app_put.config();
        apply(&mut cfg, section);
        if let Err(e) = cfg.save(&*nvs) {
            log::error!("config[{path}]: NVS save failed: {e}");
            let body = serde_json::to_vec(&ApiError::new(format!("nvs save: {e}")))
                .unwrap_or_default();
            let mut resp = req.into_response(500, None, JSON_CT)?;
            resp.write_all(&body)?;
            return Ok(());
        }
        log::info!("config[{path}]: saved ({} bytes)", buf.len());
        app_put.replace_config(cfg);
        let _ = req.into_response(204, None, &[])?;
        Ok(())
    })?;
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
