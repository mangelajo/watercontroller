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

use alloc::{boxed::Box, string::String, sync::Arc};

use serde::{Serialize, Serializer};
use serde_json::Value;
use watercontroller_core::{
    api::SwitchCommand,
    config::Config,
    webhook::{EventKind, WebhookEvent},
};

use crate::AppState;

const NOT_FOUND: &str = r#"{"error":"unknown endpoint"}"#;

// ---- streaming JSON views --------------------------------------------
//
// picoserve's `Json<T>` serializes `T` chunk-by-chunk through a small
// fixed buffer (measure pass + write passes), so the response is never
// held whole in RAM. `JsonView` is the `Serialize` payload behind
// `ApiResp::Stream`: it lets one handler return either the full config,
// the redacted config, or a single redacted section — all streamed.

/// A top-level `Config` section, identified by its serde field name.
#[derive(Clone, Copy)]
pub enum Section {
    Wifi,
    Mqtt,
    Https,
    Switches,
    Sensors,
    FlowAlarm,
    Timezone,
    SntpServers,
    Schedule,
    Wireguard,
    AdminToken,
    Webhooks,
}

impl Section {
    /// Map a `/api/config/<name>` path segment to a section, or `None`
    /// if it isn't a known config section.
    pub fn parse(name: &str) -> Option<Section> {
        Some(match name {
            "wifi" => Section::Wifi,
            "mqtt" => Section::Mqtt,
            "https" => Section::Https,
            "switches" => Section::Switches,
            "sensors" => Section::Sensors,
            "flow_alarm" => Section::FlowAlarm,
            "timezone" => Section::Timezone,
            "sntp_servers" => Section::SntpServers,
            "schedule" => Section::Schedule,
            "wireguard" => Section::Wireguard,
            "admin_token" => Section::AdminToken,
            "webhooks" => Section::Webhooks,
            _ => return None,
        })
    }
}

/// A JSON body streamed by picoserve — no whole-`Config` string is ever
/// buffered.
///
/// The owned variants box their `Config`: a bare `Config` is several KiB,
/// and `JsonView` rides inside the handler future of every `web_task` in
/// the static pool — embedding it by value bloats `.bss` past the
/// startup-stack cliff (`stack pointer out of range` at boot).
pub enum JsonView {
    /// Full config including secrets — the `?all=1` backup download.
    FullConfig(Arc<Config>),
    /// The secret-redacted full config.
    RedactedConfig(Box<Config>),
    /// One section of the secret-redacted config. The `Config` is the
    /// redacted clone; only the named field is serialized out of it.
    Section(Box<Config>, Section),
}

impl Serialize for JsonView {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // `&Arc<Config>` → `&Config`; serde's `Arc` impl is behind
            // the `rc` feature, which this build doesn't enable.
            JsonView::FullConfig(c) => (**c).serialize(s),
            JsonView::RedactedConfig(c) => c.serialize(s),
            JsonView::Section(c, sec) => match sec {
                Section::Wifi => c.wifi.serialize(s),
                Section::Mqtt => c.mqtt.serialize(s),
                Section::Https => c.https.serialize(s),
                Section::Switches => c.switches.serialize(s),
                Section::Sensors => c.sensors.serialize(s),
                Section::FlowAlarm => c.flow_alarm.serialize(s),
                Section::Timezone => c.timezone.serialize(s),
                Section::SntpServers => c.sntp_servers.serialize(s),
                Section::Schedule => c.schedule.serialize(s),
                Section::Wireguard => c.wireguard.serialize(s),
                Section::AdminToken => c.admin_token.serialize(s),
                Section::Webhooks => c.webhooks.serialize(s),
            },
        }
    }
}

// ---- /api/<seg> ------------------------------------------------------

pub fn api_get(seg: &str, st: &AppState, all: bool) -> crate::ApiResp {
    match seg {
        "diag" => {
            // `min_ever_free_bytes` is the heartbeat's running minimum
            // (`diagnostics.min_free_heap_bytes`); allocated == used.
            // `capacity_bytes` is the fixed pre-reserved heap (free+used)
            // — a no_std build has no growable heap. The SPA's Advanced-
            // tab diagnostics panel reads these. Small + bounded, so it's
            // a plain buffered body, not streamed.
            let snap = st.app.snapshot();
            let min_free = snap.diagnostics.min_free_heap_bytes.unwrap_or(0);
            let reset = snap.diagnostics.reset_reason.as_deref().unwrap_or("unknown");
            let free = esp_alloc::HEAP.free();
            let used = esp_alloc::HEAP.used();
            crate::ApiResp::Json(alloc::format!(
                r#"{{"uptime_s":{},"heap":{{"total_free_bytes":{},"total_used_bytes":{},"total_allocated_bytes":{},"capacity_bytes":{},"min_ever_free_bytes":{}}},"reset_reason":"{}","fw":"wc-nostd"}}"#,
                crate::uptime_secs(),
                free,
                used,
                used,
                free + used,
                min_free,
                reset,
            ))
        }
        // Snapshot is small + bounded — buffered. The SPA reads its
        // fields directly (returned bare, not wrapped).
        "status" => {
            crate::ApiResp::Json(serde_json::to_string(&st.app.snapshot()).unwrap_or_default())
        }
        // The config can be large (many schedule rules / webhooks);
        // streamed so it's never held whole in RAM.
        "config" => {
            let cfg = st.app.config();
            if all {
                // `?all` — full config incl. secrets, for the SPA's
                // backup download. The Arc is borrowed, not cloned.
                crate::ApiResp::Stream(JsonView::FullConfig(cfg))
            } else {
                crate::ApiResp::Stream(JsonView::RedactedConfig(Box::new(
                    cfg.redact_secrets_for_api(),
                )))
            }
        }
        _ => crate::ApiResp::Json(String::from(NOT_FOUND)),
    }
}

// `#[inline(never)]`: this builds a whole `Config` on the stack. Keeping
// it out of the route-dispatch frame stops that transient from counting
// against the streaming-response serializer that runs on the same task.
#[inline(never)]
pub fn api_put(seg: &str, st: &AppState, body: &[u8]) -> String {
    match seg {
        "config" => match serde_json::from_slice::<Config>(body) {
            Ok(mut incoming) => {
                // Preserve stored secrets the client sent back blank: a
                // redacted GET /api/config returns WiFi passwords, the
                // admin token and TLS keys empty, so a plain round-trip
                // PUT would otherwise wipe them from NVS. Restore in
                // place against the borrowed live config — cloning the
                // whole Config here would blow the web-task stack.
                incoming.restore_secrets_from(&st.app.config());
                if let Err(e) = incoming.save(&*st.nvs) {
                    return alloc::format!(r#"{{"error":"nvs save failed: {:?}"}}"#, e);
                }
                st.app.replace_config(incoming);
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

/// WiFi `scan` is handled async in the route; `scan` here is a fallback
/// (returns an empty list).
pub fn wifi_get(action: &str) -> String {
    match action {
        "scan" => String::from(r#"{"networks":[]}"#),
        _ => String::from(NOT_FOUND),
    }
}

/// `POST /api/wifi/reconnect` — accepted but inert (WiFi creds come
/// from `.env`). 204 on success, matching the IDF contract.
pub fn wifi_post(action: &str) -> crate::ApiResp {
    match action {
        "reconnect" => crate::ApiResp::NoContent,
        _ => crate::ApiResp::Json(String::from(NOT_FOUND)),
    }
}

/// `POST /api/alarm/clear` — clears the latched flow alarm, 204.
pub fn alarm_post(action: &str, st: &AppState) -> crate::ApiResp {
    match action {
        "clear" => {
            st.app.clear_flow_alarm();
            crate::ApiResp::NoContent
        }
        _ => crate::ApiResp::Json(String::from(NOT_FOUND)),
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

/// One config section, streamed straight out of the redacted config —
/// only that section is serialized, not the whole `Config`.
pub fn config_section_get(section: &str, st: &AppState) -> crate::ApiResp {
    match Section::parse(section) {
        Some(sec) => {
            let redacted = Box::new(st.app.config().redact_secrets_for_api());
            crate::ApiResp::Stream(JsonView::Section(redacted, sec))
        }
        None => crate::ApiResp::Json(String::from(r#"{"error":"unknown config section"}"#)),
    }
}

/// Merge an incoming section body into the live config + persist.
//
// `#[inline(never)]`: materializes the config as a `Value` tree plus a
// full `Config` — kept off the route-dispatch frame for the same reason
// as `api_put`.
#[inline(never)]
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
    let mut cfg: Config = match serde_json::from_value(full) {
        Ok(c) => c,
        Err(e) => return alloc::format!(r#"{{"error":"invalid config: {}"}}"#, e),
    };
    // The Value merge above replaces arrays wholesale, so a redacted
    // `wifi.networks[]` section arrives with blank passwords. Restore
    // the stored secrets (ssid-matched) in place before persisting.
    cfg.restore_secrets_from(&st.app.config());
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
