//! Outbound webhook dispatch.
//!
//! `App` and supporting tasks emit `WebhookEvent`s through a
//! `WebhookDispatcher` trait; the firmware implements it with a real
//! HTTP client (with TLS for Slack/Discord-style endpoints) running
//! on its own task. The host build provides a `RecordingDispatcher`
//! used by unit tests + the playwright suite's mock-server flow.
//!
//! Templates use a tiny `{{var}}` substitution so the user can shape
//! the body to whatever the receiver expects (Slack/Discord/n8n/HA
//! webhook/generic). Variables present on every event:
//!
//!   * `event`         — canonical name, e.g. `flow_alarm.fire`
//!   * `event_label`   — human-readable, e.g. `Flow alarm fired`
//!   * `iso_ts`        — ISO-8601 UTC timestamp
//!   * `device`        — `wifi.hostname` from config
//!   * `uptime_s`      — seconds since boot
//!
//! Event-specific extras are documented per `EventKind`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Canonical event identifiers. The on-the-wire JSON name uses the
/// dotted form (`flow_alarm.fire`) so a single webhook can subscribe
/// to multiple events by listing them in `events: [...]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// Flow alarm latched on (sustained high flow with no sprinkler).
    #[serde(rename = "flow_alarm.fire")]
    FlowAlarmFire,
    /// Flow alarm cleared by the user.
    #[serde(rename = "flow_alarm.clear")]
    FlowAlarmClear,
    /// Device booted normally.
    #[serde(rename = "boot")]
    Boot,
    /// Device booted after a panic/wdt — `reset_reason` ∈ {panic,
    /// task_wdt, int_wdt, brownout}. Fires once per boot.
    #[serde(rename = "panic_boot")]
    PanicBoot,
    /// A schedule rule activated a switch.
    /// Extra vars: `rule_id`, `target` (e.g. `sprinkler_1`).
    #[serde(rename = "schedule.fire")]
    ScheduleFire,
    /// WiFi reconnected ≥ N times within a short window — sign of a
    /// flaky AP / interference. Extra vars: `reconnects`, `window_s`.
    #[serde(rename = "wifi.flap")]
    WifiFlap,
    /// MQTT broker has been unreachable for an extended period.
    /// Extra vars: `down_s`.
    #[serde(rename = "mqtt.down")]
    MqttDown,
    /// An OTA finished successfully (the new slot has been marked
    /// valid). Extra vars: `version`, `size_bytes`.
    #[serde(rename = "ota.completed")]
    OtaCompleted,
    /// Bootloader rolled back to the previous slot (we detected on
    /// boot that we did NOT mark the previous boot's app valid).
    /// Extra vars: `previous_version`.
    #[serde(rename = "ota.rollback")]
    OtaRollback,
    /// Daily/weekly water-budget alarm fired. Extra vars: `period`,
    /// `used_l`, `threshold_l`. Reserved — emitted once the water-
    /// budget feature lands.
    #[serde(rename = "water_budget.exceeded")]
    WaterBudgetExceeded,
    /// A configuration section was written via the API. Extra var:
    /// `section` (e.g. `wifi`, `mqtt`, `flow_alarm`, `webhooks`).
    #[serde(rename = "config.changed")]
    ConfigChanged,
    /// Water control was manually turned off (not via schedule + not
    /// via flow-alarm autoclose). Extra var: `source` (e.g. `api`,
    /// `serial`).
    #[serde(rename = "manual.valve_off")]
    ManualValveOff,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::FlowAlarmFire => "flow_alarm.fire",
            EventKind::FlowAlarmClear => "flow_alarm.clear",
            EventKind::Boot => "boot",
            EventKind::PanicBoot => "panic_boot",
            EventKind::ScheduleFire => "schedule.fire",
            EventKind::WifiFlap => "wifi.flap",
            EventKind::MqttDown => "mqtt.down",
            EventKind::OtaCompleted => "ota.completed",
            EventKind::OtaRollback => "ota.rollback",
            EventKind::WaterBudgetExceeded => "water_budget.exceeded",
            EventKind::ConfigChanged => "config.changed",
            EventKind::ManualValveOff => "manual.valve_off",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            EventKind::FlowAlarmFire => "Flow alarm fired",
            EventKind::FlowAlarmClear => "Flow alarm cleared",
            EventKind::Boot => "Device booted",
            EventKind::PanicBoot => "Device rebooted after a crash",
            EventKind::ScheduleFire => "Schedule rule fired",
            EventKind::WifiFlap => "WiFi reconnect storm",
            EventKind::MqttDown => "MQTT broker unreachable",
            EventKind::OtaCompleted => "OTA update completed",
            EventKind::OtaRollback => "OTA rolled back",
            EventKind::WaterBudgetExceeded => "Water budget exceeded",
            EventKind::ConfigChanged => "Configuration changed",
            EventKind::ManualValveOff => "Water valve manually closed",
        }
    }
    /// All event kinds in declaration order — useful for the SPA / test
    /// CLI to enumerate the picker.
    pub fn all() -> &'static [EventKind] {
        &[
            EventKind::FlowAlarmFire,
            EventKind::FlowAlarmClear,
            EventKind::Boot,
            EventKind::PanicBoot,
            EventKind::ScheduleFire,
            EventKind::WifiFlap,
            EventKind::MqttDown,
            EventKind::OtaCompleted,
            EventKind::OtaRollback,
            EventKind::WaterBudgetExceeded,
            EventKind::ConfigChanged,
            EventKind::ManualValveOff,
        ]
    }
}

/// Preset that adjusts default headers + body template at config save
/// time. The firmware doesn't branch on this at runtime — it's UI
/// sugar; the SPA can apply the preset's defaults when the user
/// picks a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookKind {
    Generic,
    Slack,
    Discord,
    HomeAssistant,
}

impl Default for WebhookKind {
    fn default() -> Self {
        WebhookKind::Generic
    }
}

/// A single user-configured header. Stored as a list (not a map) so
/// the serialized form is stable across saves and tests can assert on
/// header order if needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderEntry {
    pub name: String,
    pub value: String,
}

/// One webhook target. Up to `WEBHOOKS_MAX` of these can sit in the
/// `Config::webhooks` vec. Each receives every emitted event that
/// matches its `events` filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint URL. `http://` or `https://`.
    pub url: String,
    #[serde(default)]
    pub kind: WebhookKind,
    /// Which events this webhook subscribes to. Empty = none (off).
    #[serde(default)]
    pub events: Vec<EventKind>,
    /// HTTP method. POST by default. Slack/Discord require POST, but
    /// generic receivers occasionally want PUT.
    #[serde(default = "default_method")]
    pub method: String,
    /// Extra HTTP headers (Content-Type, Authorization, X-Foo, ...).
    /// If no header named `Content-Type` is present we default to
    /// `application/json` at send time.
    #[serde(default)]
    pub headers: Vec<HeaderEntry>,
    /// Body to POST, with `{{var}}` placeholders expanded. The
    /// default template is JSON with all standard variables; the
    /// SPA can prefill kind-specific shapes (e.g. Slack: `{"text":
    /// "..."}` ).
    #[serde(default = "default_body_template")]
    pub body_template: String,
}

fn default_true() -> bool {
    true
}
fn default_method() -> String {
    "POST".into()
}
fn default_body_template() -> String {
    r#"{"event":"{{event}}","label":"{{event_label}}","device":"{{device}}","ts":"{{iso_ts}}","uptime_s":{{uptime_s}}}"#.into()
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            kind: WebhookKind::Generic,
            events: Vec::new(),
            method: default_method(),
            headers: Vec::new(),
            body_template: default_body_template(),
        }
    }
}

/// Cap on the number of configured webhooks. Each entry is ~1 KB in
/// the NVS blob; 4 leaves a tight ceiling so a misconfigured array
/// can't blow the NVS partition.
pub const WEBHOOKS_MAX: usize = 4;

/// One pending dispatch. The dispatcher owns string copies so the
/// caller can drop short-lived borrows immediately.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEvent {
    pub kind: EventKind,
    /// Variables available to the body template. `event`, `event_label`,
    /// `iso_ts`, `device`, `uptime_s` are inserted by the dispatcher
    /// if missing.
    pub vars: BTreeMap<String, String>,
}

impl WebhookEvent {
    pub fn new(kind: EventKind) -> Self {
        Self { kind, vars: BTreeMap::new() }
    }
    pub fn with<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }
}

/// Replace `{{key}}` occurrences with the corresponding value from
/// `vars`. Unknown placeholders are left intact so a misspelled var
/// in the template surfaces clearly to the receiver instead of being
/// silently swallowed.
pub fn render_template(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        let placeholder = format!("{{{{{k}}}}}");
        out = out.replace(&placeholder, v);
    }
    out
}

/// Trait that App + supporting tasks call to emit events. Implementors
/// are responsible for: (a) reading the current Config to pick matching
/// webhooks, (b) substituting the body template, (c) doing the HTTP
/// POST. Dispatch must be non-blocking — typical impl is `mpsc::send`
/// into a dedicated dispatcher task.
pub trait WebhookDispatcher: Send + Sync {
    fn dispatch(&self, event: WebhookEvent);
}

/// No-op dispatcher. Used as the default in host tests that don't care
/// about webhooks and for the firmware path before the dispatcher
/// task has spawned (the few events emitted before then would
/// otherwise panic).
#[derive(Default)]
pub struct NoopDispatcher;
impl WebhookDispatcher for NoopDispatcher {
    fn dispatch(&self, _event: WebhookEvent) {}
}

/// Test-only dispatcher: records every event in a vec so unit tests
/// can assert what fired. Thread-safe.
#[derive(Default)]
pub struct RecordingDispatcher {
    events: std::sync::Mutex<Vec<WebhookEvent>>,
}
impl RecordingDispatcher {
    pub fn take(&self) -> Vec<WebhookEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }
}
impl WebhookDispatcher for RecordingDispatcher {
    fn dispatch(&self, event: WebhookEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_known_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("event".into(), "flow_alarm.fire".into());
        vars.insert("flow_lph".into(), "120.5".into());
        let out = render_template(
            r#"{"text":"{{event}} at {{flow_lph}} L/h"}"#,
            &vars,
        );
        assert_eq!(out, r#"{"text":"flow_alarm.fire at 120.5 L/h"}"#);
    }

    #[test]
    fn render_leaves_unknown_placeholders_intact() {
        let mut vars = BTreeMap::new();
        vars.insert("event".into(), "boot".into());
        let out = render_template("{{event}} {{not_a_var}}", &vars);
        // Misspellings surface to the receiver instead of being silently dropped.
        assert_eq!(out, "boot {{not_a_var}}");
    }

    #[test]
    fn render_handles_repeated_placeholders() {
        let mut vars = BTreeMap::new();
        vars.insert("device".into(), "doremorwater".into());
        let out = render_template("{{device}} / {{device}}", &vars);
        assert_eq!(out, "doremorwater / doremorwater");
    }

    #[test]
    fn event_kind_serializes_to_dotted_form() {
        let s = serde_json::to_string(&EventKind::FlowAlarmFire).unwrap();
        assert_eq!(s, "\"flow_alarm.fire\"");
        let s = serde_json::to_string(&EventKind::OtaRollback).unwrap();
        assert_eq!(s, "\"ota.rollback\"");
    }

    #[test]
    fn recording_dispatcher_captures_in_order() {
        let d = RecordingDispatcher::default();
        d.dispatch(WebhookEvent::new(EventKind::Boot));
        d.dispatch(WebhookEvent::new(EventKind::FlowAlarmFire).with("flow_lph", "200"));
        let evs = d.take();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, EventKind::Boot);
        assert_eq!(evs[1].kind, EventKind::FlowAlarmFire);
        assert_eq!(evs[1].vars.get("flow_lph").unwrap(), "200");
    }
}
