//! HTTP server: serves the embedded SPA and the JSON API.
//!
//! Routes mirror `core::api::routes`. WebSocket logs deferred until a
//! follow-up — the SPA polls `/api/status` for now and uses the telnet log
//! port for live logs.

use crate::assets::INDEX_HTML;
use anyhow::Result;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use watercontroller_core::api::{routes, ApiError, CommandOutcome, ConfigUpdate, SwitchCommand};
use watercontroller_core::app::App;

const READ_BUF_LEN: usize = 1024;
const MAX_BODY: usize = 32 * 1024;

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

    // POST /api/factory_reset → erase NVS config and reboot.
    {
        let nvs = nvs.clone();
        server.fn_handler::<EspIOError, _>(routes::FACTORY_RESET, Method::Post, move |req| {
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
