//! Single serde-able runtime configuration. Persisted in NVS as JSON under the
//! key `wc.cfg`. Defaults match the original ESPHome YAML so a fresh device
//! comes up in a sensible state.

use crate::calibration::Calibration;
use crate::schedule::{default_schedule, Schedule};
use crate::traits::{NvsError, NvsStore, WifiCreds};
use alloc::{string::String, vec, vec::Vec};
use serde::{Deserialize, Serialize};

const NVS_KEY: &str = "wc.cfg";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WifiConfig {
    pub networks: Vec<WifiCreds>,
    /// Override hostname; default is `doremorwater`.
    #[serde(default = "default_hostname")]
    pub hostname: String,
    /// AP fallback SSID; default matches the YAML.
    #[serde(default = "default_ap_ssid")]
    pub ap_ssid: String,
    /// AP fallback password (empty for open AP, matches YAML default).
    #[serde(default)]
    pub ap_password: String,
}

fn default_hostname() -> String {
    "doremorwater".into()
}
fn default_ap_ssid() -> String {
    "Doremorwater Fallback Hotspot".into()
}

impl Default for WifiConfig {
    fn default() -> Self {
        // Optional build-time WiFi seed: if SSID and PASSWORD are set in
        // `.env` at the workspace root (which `crates/core/build.rs`
        // forwards via `cargo:rustc-env=`), bake them into the default
        // config so a freshly-flashed device joins the lab network on
        // first boot. Empty / unset = no networks (AP fallback on boot).
        let seed = match (
            option_env!("WC_WIFI_SSID"),
            option_env!("WC_WIFI_PASSWORD"),
        ) {
            (Some(ssid), Some(password)) if !ssid.is_empty() => vec![WifiCreds {
                ssid: ssid.into(),
                password: password.into(),
            }],
            _ => Vec::new(),
        };
        Self {
            networks: seed,
            hostname: default_hostname(),
            ap_ssid: default_ap_ssid(),
            ap_password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MqttConfig {
    /// e.g. `mqtt://homeassistant.local:1883` or `mqtts://broker:8883` for TLS.
    pub broker_url: String,
    pub username: String,
    pub password: String,
    /// Base topic for all device-published topics. Defaults to the hostname.
    pub base_topic: String,
    pub ha_discovery_prefix: String, // typically "homeassistant"
    pub enabled: bool,
    /// PEM-encoded CA certificate to trust the broker (server-side TLS).
    /// Required for `mqtts://` unless your broker uses a public CA the
    /// device's bundle already trusts. Empty = no custom CA.
    #[serde(default)]
    pub ca_cert_pem: String,
    /// PEM-encoded client certificate, for mutual TLS. Empty = no client cert.
    #[serde(default)]
    pub client_cert_pem: String,
    /// PEM-encoded client private key, paired with `client_cert_pem`.
    #[serde(default)]
    pub client_key_pem: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker_url: String::new(),
            username: String::new(),
            password: String::new(),
            base_topic: "doremorwater".into(),
            ha_discovery_prefix: "homeassistant".into(),
            enabled: false,
            ca_cert_pem: String::new(),
            client_cert_pem: String::new(),
            client_key_pem: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SensorsConfig {
    pub battery: Calibration,
    pub pressure_stage1: Calibration,
    pub pressure_stage2: Calibration,
    /// L per pulse (cumulative). YAML used `1 / 516.5` ≈ 0.001936...
    pub flow_l_per_pulse: f32,
    /// L/hr per pulse-per-second. YAML used `0.00225012 * 60` per minute window.
    pub flow_lph_per_pps: f32,
}

impl Default for SensorsConfig {
    fn default() -> Self {
        Self {
            battery: Calibration::new([(1130.0, 5.00), (2931.0, 12.2)]).unwrap(),
            pressure_stage1: Calibration::new([(0.37, 0.54), (2.62, 3.98)]).unwrap(),
            pressure_stage2: Calibration::new([(0.54, 0.0), (4.50, 10.34214)]).unwrap(),
            flow_l_per_pulse: 1.0 / 516.5,
            flow_lph_per_pps: 0.00225012 * 60.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireguardConfig {
    pub enabled: bool,
    pub address: String,             // device's tunnel IP, e.g. "10.6.0.5"
    pub private_key: String,
    pub peer_endpoint: String,
    pub peer_public_key: String,
    pub peer_preshared_key: String,
    pub peer_allowed_ips: Vec<String>,
    pub keepalive_secs: u16,
}

impl Default for WireguardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Match the YAML's historical address as a starting point — keys
            // remain empty until configured via the web UI.
            address: "10.6.0.5".into(),
            private_key: String::new(),
            peer_endpoint: String::new(),
            peer_public_key: String::new(),
            peer_preshared_key: String::new(),
            peer_allowed_ips: vec!["192.168.1.0/24".into(), "10.6.0.1/32".into()],
            keepalive_secs: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpsConfig {
    /// Master enable for the HTTPS listener on :443. When `false`, the device
    /// serves HTTP only even if a cert + key are present. Provides an escape
    /// hatch when something on the LAN hammers :443 with bad TLS handshakes:
    /// each failed `mbedtls_ssl_handshake` allocates an ~12 KiB context out
    /// of internal DRAM, and at sustained load the fragmentation starves
    /// FreeRTOS task creation (we've observed `esp_mqtt_client_start` →
    /// "Error create mqtt task" once enough internal DRAM has fragmented).
    #[serde(default = "default_https_enabled")]
    pub enabled: bool,
    /// PEM-encoded X.509 certificate for the on-device HTTPS server. If
    /// either this or `key_pem` is empty, only HTTP is served (port 80).
    /// Generate a self-signed cert + key with:
    ///   `openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 -nodes`
    pub cert_pem: String,
    /// PEM-encoded private key paired with `cert_pem`.
    pub key_pem: String,
}
fn default_https_enabled() -> bool { true }

impl Default for HttpsConfig {
    fn default() -> Self {
        Self { enabled: true, cert_pem: String::new(), key_pem: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwitchesConfig {
    /// Auto-off duration for sprinkler_1, in seconds. YAML legacy default
    /// 7 min. Set to 0 to disable auto-off (manual only — risky for a
    /// physical sprinkler).
    #[serde(default = "default_sprinkler_1_auto_off")]
    pub sprinkler_1_auto_off_secs: u32,
    /// Auto-off duration for sprinkler_2, in seconds. YAML legacy default 5 min.
    #[serde(default = "default_sprinkler_2_auto_off")]
    pub sprinkler_2_auto_off_secs: u32,
    /// Water-valve motor pulse + drain timing. Defaults to ESPHome legacy
    /// values (1 s settle + 14 s pulse + 1 s settle + 5 min drain).
    #[serde(default)]
    pub valve_timing: crate::water_valve::ValveTiming,
}
fn default_sprinkler_1_auto_off() -> u32 { 7 * 60 }
fn default_sprinkler_2_auto_off() -> u32 { 5 * 60 }

impl Default for SwitchesConfig {
    fn default() -> Self {
        Self {
            sprinkler_1_auto_off_secs: default_sprinkler_1_auto_off(),
            sprinkler_2_auto_off_secs: default_sprinkler_2_auto_off(),
            valve_timing: crate::water_valve::ValveTiming::default(),
        }
    }
}

/// Flow-rate alarm. Fires when `sensors.flow_lph` stays at or above
/// `threshold_lph` for at least `duration_secs`, but ignores periods
/// while any sprinkler is on (sprinklers cause expected high flow).
/// Fire → latched active state + forced water_control off. Cleared
/// via /api/alarm/clear or the `alarm clear` serial command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowAlarmConfig {
    pub enabled: bool,
    pub threshold_lph: f32,
    pub duration_secs: u32,
}

impl Default for FlowAlarmConfig {
    fn default() -> Self {
        // Disabled by default — the user opts in via the UI / API and
        // picks numbers appropriate to their plumbing. 100 L/h for 60 s
        // is a reasonable starting point for a household feed.
        Self { enabled: false, threshold_lph: 100.0, duration_secs: 60 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub wifi: WifiConfig,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub https: HttpsConfig,
    #[serde(default)]
    pub switches: SwitchesConfig,
    #[serde(default)]
    pub sensors: SensorsConfig,
    #[serde(default)]
    pub flow_alarm: FlowAlarmConfig,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Bare SNTP server hostnames.
    #[serde(default = "default_sntp_servers")]
    pub sntp_servers: Vec<String>,
    #[serde(default = "default_schedule")]
    pub schedule: Schedule,
    #[serde(default)]
    pub wireguard: WireguardConfig,
    /// If non-empty, every mutating HTTP request (POST/PUT) must carry an
    /// `Authorization: Bearer <token>` header matching this value. Empty
    /// (default) means the API is unauthenticated — fine for a freshly
    /// flashed device behind a trusted network, but you should set this
    /// before exposing the device to anything wider.
    #[serde(default)]
    pub admin_token: String,
    /// Outbound webhooks. See `crate::webhook` for the per-entry shape
    /// and the catalog of events. Capped at `webhook::WEBHOOKS_MAX`.
    #[serde(default)]
    pub webhooks: Vec<crate::webhook::WebhookConfig>,
}

fn default_timezone() -> String {
    "Europe/Madrid".into()
}
fn default_sntp_servers() -> Vec<String> {
    vec![
        "0.es.pool.ntp.org".into(),
        "1.es.pool.ntp.org".into(),
        "2.es.pool.ntp.org".into(),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::default(),
            mqtt: MqttConfig::default(),
            https: HttpsConfig::default(),
            switches: SwitchesConfig::default(),
            sensors: SensorsConfig::default(),
            flow_alarm: FlowAlarmConfig::default(),
            timezone: default_timezone(),
            sntp_servers: default_sntp_servers(),
            schedule: default_schedule(),
            wireguard: WireguardConfig::default(),
            admin_token: String::new(),
            webhooks: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Nvs(NvsError),
    Json(serde_json::Error),
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Nvs(e) => write!(f, "nvs error: {e}"),
            Self::Json(e) => write!(f, "json error: {e}"),
        }
    }
}
impl core::error::Error for ConfigError {}

impl From<NvsError> for ConfigError {
    fn from(e: NvsError) -> Self {
        Self::Nvs(e)
    }
}
impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl Config {
    /// Load from NVS, returning defaults if not present.
    pub fn load(nvs: &dyn NvsStore) -> Result<Self, ConfigError> {
        match nvs.get(NVS_KEY) {
            None => Ok(Config::default()),
            Some(bytes) => {
                let cfg: Config = serde_json::from_slice(&bytes)?;
                Ok(cfg)
            }
        }
    }

    pub fn save(&self, nvs: &dyn NvsStore) -> Result<(), ConfigError> {
        let bytes = serde_json::to_vec(self)?;
        nvs.set(NVS_KEY, &bytes)?;
        Ok(())
    }

    /// Erase the persisted config so the next boot uses defaults.
    pub fn factory_reset(nvs: &dyn NvsStore) -> Result<(), ConfigError> {
        nvs.remove(NVS_KEY)?;
        Ok(())
    }

    /// Return a clone with every sensitive field blanked, suitable for
    /// `GET /api/config`. Public certs (cert_pem, peer_public_key,
    /// ca_cert_pem) are kept — they're not secrets and the UI shows them so
    /// users can verify what's installed.
    pub fn redact_secrets_for_api(&self) -> Self {
        let mut out = self.clone();
        for net in out.wifi.networks.iter_mut() {
            net.password.clear();
        }
        out.mqtt.password.clear();
        out.mqtt.client_key_pem.clear();
        out.https.key_pem.clear();
        out.wireguard.private_key.clear();
        out.wireguard.peer_preshared_key.clear();
        out.admin_token.clear();
        out
    }

    /// Apply an incoming config update from the API while preserving stored
    /// secrets that came back empty (because [`Self::redact_secrets_for_api`]
    /// blanks them on the way out). Per field:
    ///
    ///   - WiFi network passwords: matched by SSID. Empty incoming password
    ///     + matching stored SSID → keep stored. New SSIDs with empty
    ///     passwords pass through (legitimate intent: open network).
    ///   - All other secrets (mqtt password / client key, https key,
    ///     wireguard keys, admin token): empty in incoming → keep stored;
    ///     non-empty in incoming → overwrite.
    pub fn merge_preserving_secrets(&mut self, mut incoming: Self) {
        incoming.restore_secrets_from(self);
        *self = incoming;
    }

    /// In-place form of [`Self::merge_preserving_secrets`]: fill any secret
    /// field left blank on `self` (a freshly deserialized incoming API
    /// update) from `stored`, which is borrowed rather than consumed.
    ///
    /// This is the variant a firmware handler should use — it keeps only
    /// the single incoming `Config` on the (tight) task stack instead of
    /// cloning the whole live `Config` to get an owned `self`.
    pub fn restore_secrets_from(&mut self, stored: &Self) {
        for net in self.wifi.networks.iter_mut() {
            if net.password.is_empty() {
                if let Some(existing) =
                    stored.wifi.networks.iter().find(|n| n.ssid == net.ssid)
                {
                    net.password = existing.password.clone();
                }
            }
        }
        if self.mqtt.password.is_empty() {
            self.mqtt.password = stored.mqtt.password.clone();
        }
        if self.mqtt.client_key_pem.is_empty() {
            self.mqtt.client_key_pem = stored.mqtt.client_key_pem.clone();
        }
        if self.https.key_pem.is_empty() {
            self.https.key_pem = stored.https.key_pem.clone();
        }
        if self.wireguard.private_key.is_empty() {
            self.wireguard.private_key = stored.wireguard.private_key.clone();
        }
        if self.wireguard.peer_preshared_key.is_empty() {
            self.wireguard.peer_preshared_key = stored.wireguard.peer_preshared_key.clone();
        }
        if self.admin_token.is_empty() {
            self.admin_token = stored.admin_token.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MemNvs(Mutex<std::collections::HashMap<String, Vec<u8>>>);
    impl NvsStore for MemNvs {
        fn get(&self, key: &str) -> Option<Vec<u8>> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn set(&self, key: &str, value: &[u8]) -> Result<(), NvsError> {
            self.0.lock().unwrap().insert(key.into(), value.to_vec());
            Ok(())
        }
        fn remove(&self, key: &str) -> Result<(), NvsError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn round_trip_defaults_through_nvs() {
        let nvs = MemNvs(Default::default());
        let cfg = Config::default();
        cfg.save(&nvs).unwrap();
        let restored = Config::load(&nvs).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn missing_nvs_returns_defaults() {
        let nvs = MemNvs(Default::default());
        let cfg = Config::load(&nvs).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_json_uses_defaults_for_missing_fields() {
        let nvs = MemNvs(Default::default());
        nvs.set(NVS_KEY, br#"{"timezone":"UTC"}"#).unwrap();
        let cfg = Config::load(&nvs).unwrap();
        assert_eq!(cfg.timezone, "UTC");
        assert_eq!(cfg.sensors, SensorsConfig::default());
    }

    fn cfg_with_secrets() -> Config {
        let mut c = Config::default();
        c.wifi.networks = vec![
            WifiCreds { ssid: "home".into(), password: "wifi-secret".into() },
            WifiCreds { ssid: "guest".into(), password: "guest-secret".into() },
        ];
        c.mqtt.password = "mqtt-secret".into();
        c.mqtt.client_key_pem = "-----BEGIN MQTT KEY-----".into();
        c.https.key_pem = "-----BEGIN HTTPS KEY-----".into();
        c.wireguard.private_key = "wg-private".into();
        c.wireguard.peer_preshared_key = "wg-psk".into();
        c.admin_token = "admin-bearer".into();
        c
    }

    #[test]
    fn redact_blanks_every_secret() {
        let r = cfg_with_secrets().redact_secrets_for_api();
        assert!(r.wifi.networks.iter().all(|n| n.password.is_empty()));
        assert_eq!(r.mqtt.password, "");
        assert_eq!(r.mqtt.client_key_pem, "");
        assert_eq!(r.https.key_pem, "");
        assert_eq!(r.wireguard.private_key, "");
        assert_eq!(r.wireguard.peer_preshared_key, "");
        assert_eq!(r.admin_token, "");
    }

    #[test]
    fn redact_keeps_public_certs_and_ssids() {
        let mut c = cfg_with_secrets();
        c.https.cert_pem = "-----BEGIN HTTPS CERT-----".into();
        c.mqtt.ca_cert_pem = "-----BEGIN CA-----".into();
        c.wireguard.peer_public_key = "wg-pub".into();
        let r = c.redact_secrets_for_api();
        assert_eq!(r.wifi.networks[0].ssid, "home");
        assert_eq!(r.https.cert_pem, "-----BEGIN HTTPS CERT-----");
        assert_eq!(r.mqtt.ca_cert_pem, "-----BEGIN CA-----");
        assert_eq!(r.wireguard.peer_public_key, "wg-pub");
    }

    #[test]
    fn merge_preserves_empty_secrets_from_redacted_round_trip() {
        let mut stored = cfg_with_secrets();
        // Simulate: SPA fetched redacted view, user changed one non-secret
        // field, posted back. All secrets in incoming are empty.
        let mut incoming = stored.redact_secrets_for_api();
        incoming.timezone = "UTC".into();
        stored.merge_preserving_secrets(incoming);
        assert_eq!(stored.timezone, "UTC");
        assert_eq!(stored.https.key_pem, "-----BEGIN HTTPS KEY-----");
        assert_eq!(stored.mqtt.password, "mqtt-secret");
        assert_eq!(stored.admin_token, "admin-bearer");
        assert_eq!(stored.wifi.networks[0].password, "wifi-secret");
        assert_eq!(stored.wifi.networks[1].password, "guest-secret");
    }

    #[test]
    fn merge_overwrites_when_incoming_provides_new_secret() {
        let mut stored = cfg_with_secrets();
        let mut incoming = stored.redact_secrets_for_api();
        incoming.https.key_pem = "-----BEGIN NEW HTTPS KEY-----".into();
        incoming.admin_token = "rotated".into();
        stored.merge_preserving_secrets(incoming);
        assert_eq!(stored.https.key_pem, "-----BEGIN NEW HTTPS KEY-----");
        assert_eq!(stored.admin_token, "rotated");
        // Untouched secrets still preserved.
        assert_eq!(stored.mqtt.password, "mqtt-secret");
    }

    #[test]
    fn merge_wifi_password_match_by_ssid() {
        let mut stored = cfg_with_secrets();
        // User reordered networks and added a new open one.
        let mut incoming = stored.redact_secrets_for_api();
        incoming.wifi.networks = vec![
            WifiCreds { ssid: "guest".into(), password: "".into() },
            WifiCreds { ssid: "home".into(), password: "".into() },
            WifiCreds { ssid: "openCafe".into(), password: "".into() }, // new
        ];
        stored.merge_preserving_secrets(incoming);
        assert_eq!(stored.wifi.networks[0].password, "guest-secret");
        assert_eq!(stored.wifi.networks[1].password, "wifi-secret");
        assert_eq!(stored.wifi.networks[2].password, ""); // new + open, kept empty
    }
}
