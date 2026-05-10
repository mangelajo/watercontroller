//! Platform abstractions. The `firmware` crate implements these against
//! `esp-idf-svc`/`esp-idf-hal`; the `host` crate implements them with fakes.
//! Anything in `core` that needs to touch hardware goes through these traits.

use chrono::{DateTime, Utc};

/// Wall-clock + monotonic time. Implementations must always return increasing
/// `monotonic_ms` values across the lifetime of the program; `now()` is allowed
/// to jump backwards (e.g. when SNTP first syncs).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn monotonic_ms(&self) -> u64;
}

/// A digital output pin. The `set` operation must be cheap and infallible.
pub trait GpioOut: Send {
    fn set(&mut self, high: bool);
}

/// A 12-bit ADC reading. Calibration is applied in `core::calibration`,
/// not here — this is the raw count.
pub trait Adc: Send {
    fn read_raw(&mut self) -> u16;
}

/// A monotonically-increasing pulse counter. Used for the water-flow sensor.
/// Must remain accurate across reads (i.e. counts are never lost).
pub trait PulseCounter: Send + Sync {
    fn count(&self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvsError {
    NotFound,
    Full,
    Io(String),
}

impl std::fmt::Display for NvsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "key not found"),
            Self::Full => write!(f, "storage full"),
            Self::Io(s) => write!(f, "io error: {s}"),
        }
    }
}

impl std::error::Error for NvsError {}

/// Persistent key-value store. Values are arbitrary bytes; serialization is
/// the caller's responsibility (we use serde+serde_json elsewhere).
pub trait NvsStore: Send + Sync {
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<(), NvsError>;
    fn remove(&self, key: &str) -> Result<(), NvsError>;
}

#[derive(Debug, Clone)]
pub struct PublishOpts {
    pub retained: bool,
    pub qos: u8,
}

impl Default for PublishOpts {
    fn default() -> Self {
        Self { retained: false, qos: 1 }
    }
}

/// MQTT client. `set_handler` registers the callback used for every incoming
/// message; only one handler at a time. Implementations must be thread-safe.
pub trait Mqtt: Send + Sync {
    fn publish(&self, topic: &str, payload: &[u8], opts: PublishOpts);
    fn subscribe(&self, topic: &str);
    fn set_handler(&self, handler: Box<dyn Fn(&str, &[u8]) + Send + Sync>);
    fn is_connected(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WifiState {
    Disconnected,
    Connecting { ssid: String },
    Connected { ssid: String, ip: String },
    /// AP fallback active — captive portal is up, station mode is off.
    ApMode { ssid: String, ip: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WifiCreds {
    pub ssid: String,
    pub password: String,
}

pub trait Wifi: Send + Sync {
    fn state(&self) -> WifiState;
    fn connect(&self, networks: &[WifiCreds]);
    /// Force a (re)scan + reconnect attempt.
    fn reconnect(&self);
}

pub mod prelude {
    pub use super::{
        Adc, Clock, GpioOut, Mqtt, NvsError, NvsStore, PublishOpts, PulseCounter, Wifi,
        WifiCreds, WifiState,
    };
    pub use core::time::Duration;
}
