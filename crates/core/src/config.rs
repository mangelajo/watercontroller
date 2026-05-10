//! Single serde-able runtime configuration. Persisted in NVS as JSON under the
//! key `wc.cfg`. Defaults match the original ESPHome YAML so a fresh device
//! comes up in a sensible state.

use crate::calibration::Calibration;
use crate::schedule::{default_schedule, Schedule};
use crate::traits::{NvsError, NvsStore, WifiCreds};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HttpsConfig {
    /// PEM-encoded X.509 certificate for the on-device HTTPS server. If
    /// either this or `key_pem` is empty, only HTTP is served (port 80).
    /// Generate a self-signed cert + key with:
    ///   `openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 -nodes`
    pub cert_pem: String,
    /// PEM-encoded private key paired with `cert_pem`.
    pub key_pem: String,
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
}
fn default_sprinkler_1_auto_off() -> u32 { 7 * 60 }
fn default_sprinkler_2_auto_off() -> u32 { 5 * 60 }

impl Default for SwitchesConfig {
    fn default() -> Self {
        Self {
            sprinkler_1_auto_off_secs: default_sprinkler_1_auto_off(),
            sprinkler_2_auto_off_secs: default_sprinkler_2_auto_off(),
        }
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
            timezone: default_timezone(),
            sntp_servers: default_sntp_servers(),
            schedule: default_schedule(),
            wireguard: WireguardConfig::default(),
            admin_token: String::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Nvs(NvsError),
    Json(serde_json::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nvs(e) => write!(f, "nvs error: {e}"),
            Self::Json(e) => write!(f, "json error: {e}"),
        }
    }
}
impl std::error::Error for ConfigError {}

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
}
