//! HTTP API types + pure-function handlers shared between firmware and host
//! HTTP servers. Each server adapter (`firmware::http_server` and
//! `host::http_server`) is responsible only for protocol plumbing — request
//! parsing, response writing — and routes everything through these handlers.

use crate::config::Config;
use crate::state::DeviceSnapshot;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SwitchCommand {
    Sprinkler1 { on: bool },
    Sprinkler2 { on: bool },
    WaterControl { on: bool },
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub message: String,
}

impl ApiError {
    pub fn new<S: Into<String>>(s: S) -> Self {
        Self { message: s.into() }
    }
}

/// Outcome of a switch command. Servers should return 200 + JSON for `Ok`
/// and 409 + JSON for `Busy` (water control mid-sequence).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum CommandOutcome {
    Ok,
    Busy { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse<'a> {
    pub state: &'a DeviceSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigResponse<'a> {
    pub config: &'a Config,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigUpdate(pub Config);

/// Routes for the HTTP API. The firmware/host adapters dispatch on (method, path)
/// and call into application code.
pub mod routes {
    pub const STATUS: &str = "/api/status";
    pub const CONFIG: &str = "/api/config";
    pub const SWITCH: &str = "/api/switch";
    pub const LOGS_WS: &str = "/ws/logs";
    pub const OTA_UPLOAD: &str = "/api/ota";
    /// POST: erase NVS config and reboot the device. Returns 202 Accepted then
    /// proceeds with the reboot. Implemented on firmware only — the host build
    /// returns 501 since there's no persistent storage to wipe.
    pub const FACTORY_RESET: &str = "/api/factory_reset";
    /// GET: trigger a WiFi scan and return the discovered SSIDs. Used by the
    /// AP-mode setup wizard to populate the network picker.
    pub const WIFI_SCAN: &str = "/api/wifi/scan";
    /// POST: clear the latched flow-rate alarm.
    pub const ALARM_CLEAR: &str = "/api/alarm/clear";
    /// GET: list past flow-alarm fires (oldest first), persisted in NVS.
    pub const ALARM_HISTORY: &str = "/api/alarm/history";
    /// POST: emit a synthetic webhook event for testing the wiring,
    /// e.g. `{"kind":"flow_alarm.fire"}`. Goes through the normal
    /// dispatcher path so it exercises every subscribed webhook.
    pub const WEBHOOKS_TEST: &str = "/api/webhooks/test";
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WifiScanResult {
    pub ssid: String,
    pub rssi_dbm: i8,
    pub auth: String, // "open" | "wep" | "wpa" | "wpa2" | "wpa3" | "unknown"
    pub channel: u8,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WifiScanResponse {
    pub networks: Vec<WifiScanResult>,
}
