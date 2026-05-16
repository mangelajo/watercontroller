//! JSON API dispatch.
//!
//! picoserve builds its router as a deeply-nested generic type and
//! `call_path_router` recurses once per registered route — with every
//! frame carrying that huge type, a route-per-endpoint overflows the
//! executor poll stack. So the `/api` surface is reached through a
//! handful of prefix routes (`/api/<seg>`, `/api/config/<seg>`, …) and
//! the functions here dispatch on the captured trailing segment.
//!
//! picoserve also only supports a *single* path parameter alongside
//! the `State`/body extractors, which is why each two-level path
//! (`/api/config/...`, `/api/wifi/...`) needs its own prefix route
//! rather than one generic `/api/<a>/<b>`.
//!
//! Every handler returns a JSON string; transport errors are reported
//! in-band as `{"error":"..."}` with a 200, matching the SPA's
//! `response.ok` checks.

use alloc::string::String;

use serde_json::Value;
use watercontroller_core::{
    api::SwitchCommand,
    config::Config,
    webhook::{EventKind, WebhookEvent},
};

use crate::AppState;

const NOT_FOUND: &str = r#"{"error":"unknown endpoint"}"#;

// ---- /api/<seg> ------------------------------------------------------

pub fn api_get(seg: &str, st: &AppState, all: bool) -> String {
    match seg {
        "diag" => alloc::format!(
            r#"{{"uptime_s":{},"heap":{{"total_free_bytes":{},"total_used_bytes":{}}},"fw":"wc-nostd"}}"#,
            crate::uptime_secs(),
            esp_alloc::HEAP.free(),
            esp_alloc::HEAP.used(),
        ),
        // The SPA reads the snapshot / config fields directly, so both
        // are returned bare — not wrapped — matching the IDF firmware.
        "status" => serde_json::to_string(&st.app.snapshot()).unwrap_or_default(),
        "config" => {
            let cfg = st.app.config();
            if all {
                // `?all` — full config incl. secrets, for the SPA's
                // backup download.
                serde_json::to_string(&*cfg).unwrap_or_default()
            } else {
                serde_json::to_string(&cfg.redact_secrets_for_api()).unwrap_or_default()
            }
        }
        _ => String::from(NOT_FOUND),
    }
}

pub fn api_put(seg: &str, st: &AppState, body: &[u8]) -> String {
    match seg {
        "config" => match serde_json::from_slice::<Config>(body) {
            Ok(cfg) => {
                if let Err(e) = cfg.save(&*st.nvs) {
                    return alloc::format!(r#"{{"error":"nvs save failed: {:?}"}}"#, e);
                }
                st.app.replace_config(cfg);
                String::from(r#"{"result":"ok"}"#)
            }
            Err(e) => alloc::format!(r#"{{"error":"bad config json: {}"}}"#, e),
        },
        _ => String::from(NOT_FOUND),
    }
}

pub fn api_post(seg: &str, st: &AppState, body: &[u8]) -> String {
    match seg {
        "switch" => match serde_json::from_slice::<SwitchCommand>(body) {
            Ok(cmd) => serde_json::to_string(&st.app.switch_command(cmd)).unwrap_or_default(),
            Err(e) => alloc::format!(r#"{{"error":"bad switch command: {}"}}"#, e),
        },
        "factory_reset" => {
            if let Err(e) = Config::factory_reset(&*st.nvs) {
                return alloc::format!(r#"{{"error":"erase failed: {:?}"}}"#, e);
            }
            crate::ota::request_reboot();
            String::from(r#"{"result":"ok","detail":"rebooting"}"#)
        }
        _ => String::from(NOT_FOUND),
    }
}

// ---- /api/wifi/<action>, /api/alarm/<action>, /api/webhooks/<action> -

/// WiFi creds come from `.env` on this firmware, so scan + reconnect
/// are accepted but inert (the SPA's setup wizard is moot here).
pub fn wifi_get(action: &str) -> String {
    match action {
        "scan" => String::from(r#"{"networks":[]}"#),
        _ => String::from(NOT_FOUND),
    }
}
pub fn wifi_post(action: &str) -> String {
    match action {
        "reconnect" => String::from(r#"{"result":"ok"}"#),
        _ => String::from(NOT_FOUND),
    }
}

pub fn alarm_post(action: &str, st: &AppState) -> String {
    match action {
        "clear" => {
            st.app.clear_flow_alarm();
            String::from(r#"{"result":"ok"}"#)
        }
        _ => String::from(NOT_FOUND),
    }
}

/// `/api/webhooks/test` — emit a synthetic event. Body:
/// `{"kind":"...","vars":{...}}`.
pub fn webhooks_post(action: &str, st: &AppState, body: &[u8]) -> String {
    if action != "test" {
        return String::from(NOT_FOUND);
    }
    #[derive(serde::Deserialize)]
    struct TestReq {
        kind: EventKind,
        #[serde(default)]
        vars: alloc::collections::BTreeMap<String, String>,
    }
    match serde_json::from_slice::<TestReq>(body) {
        Ok(tr) => {
            let mut ev = WebhookEvent::new(tr.kind);
            ev.vars = tr.vars;
            st.app.emit_event(ev);
            String::from(r#"{"result":"ok"}"#)
        }
        Err(e) => alloc::format!(r#"{{"error":"bad json: {}"}}"#, e),
    }
}

// ---- /api/config/<section> ------------------------------------------

/// One config section, sliced out of the redacted config.
pub fn config_section_get(section: &str, st: &AppState) -> String {
    let cfg = st.app.config();
    let safe = cfg.redact_secrets_for_api();
    let full = serde_json::to_value(&safe).unwrap_or(Value::Null);
    match full.get(section) {
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| String::from("null")),
        None => String::from(r#"{"error":"unknown config section"}"#),
    }
}

/// Merge an incoming section body into the live config + persist.
pub fn config_section_put(section: &str, st: &AppState, body: &[u8]) -> String {
    let incoming: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return alloc::format!(r#"{{"error":"bad json: {}"}}"#, e),
    };
    // Start from the *current* (non-redacted) config so secret fields
    // absent from the incoming body are preserved.
    let mut full = match serde_json::to_value(&*st.app.config()) {
        Ok(v) => v,
        Err(_) => return String::from(r#"{"error":"config serialize failed"}"#),
    };
    match full.get_mut(section) {
        Some(target) => merge(target, &incoming),
        None => return String::from(r#"{"error":"unknown config section"}"#),
    }
    let cfg: Config = match serde_json::from_value(full) {
        Ok(c) => c,
        Err(e) => return alloc::format!(r#"{{"error":"invalid config: {}"}}"#, e),
    };
    if let Err(e) = cfg.save(&*st.nvs) {
        return alloc::format!(r#"{{"error":"nvs save failed: {:?}"}}"#, e);
    }
    st.app.replace_config(cfg);
    String::from(r#"{"result":"ok"}"#)
}

/// Recursively merge `overlay` into `target`. An empty-string leaf in
/// `overlay` is skipped — that's how a redacted secret round-trips
/// (the GET returns it blank; the PUT must not clobber the real value).
fn merge(target: &mut Value, overlay: &Value) {
    match (target, overlay) {
        (Value::Object(t), Value::Object(o)) => {
            for (k, v) in o {
                if matches!(v, Value::String(s) if s.is_empty()) {
                    continue;
                }
                match t.get_mut(k) {
                    Some(tv) => merge(tv, v),
                    None => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (t, o) => *t = o.clone(),
    }
}
