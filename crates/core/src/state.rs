//! Shared, mutex-protected device state. The single source of truth read by
//! the MQTT publisher, the HTTP API, and the schedule executor.

use crate::traits::WifiState;
use crate::water_valve::WaterState;
use alloc::string::String;
use serde::Serialize;
use spin::Mutex;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Sensors {
    pub battery_v: Option<f32>,
    pub pressure_bar: Option<f32>,
    pub flow_lph: Option<f32>,
    pub total_l: Option<f32>,
    pub loop_time_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Switches {
    pub sprinkler_1: bool,
    pub sprinkler_2: bool,
    pub water_control: WaterControlState,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum WaterControlState {
    Off,
    On,
    /// Mid-sequence — caller should not issue a new command yet.
    Transitioning,
}

impl Default for WaterControlState {
    fn default() -> Self {
        Self::Off
    }
}

impl From<WaterState> for WaterControlState {
    fn from(s: WaterState) -> Self {
        match s {
            WaterState::Off => Self::Off,
            WaterState::On => Self::On,
            WaterState::TurningOn | WaterState::TurningOff => Self::Transitioning,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Network {
    pub wifi: Option<WifiState>,
    pub mqtt_connected: bool,
    pub wifi_rssi_dbm: Option<i8>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Diagnostics {
    pub free_heap_bytes: Option<u32>,
    pub min_free_heap_bytes: Option<u32>,
    pub reset_reason: Option<String>,
}

/// Latched flow-rate alarm. `elapsed_secs` counts the current sustained-
/// above-threshold window (resets when flow drops or any sprinkler is
/// on); `active` becomes `true` once elapsed crosses
/// `config.flow_alarm.duration_secs` and stays true until the user
/// clears it (POST /api/alarm/clear or `alarm clear` over serial).
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct FlowAlarm {
    pub active: bool,
    pub elapsed_secs: u32,
}

/// One recorded flow-alarm fire. Stored in a small ring (most-recent
/// last) on the device so a post-incident look at "what happened last
/// Tuesday" doesn't require trawling HA history.
#[derive(Debug, Clone, serde::Deserialize, Serialize, PartialEq)]
pub struct AlarmEvent {
    /// Wall-clock epoch seconds at the moment of firing. 0 if the
    /// device hasn't synced time yet — uptime_ms is the fallback.
    pub epoch_secs: u64,
    /// Device uptime at fire-time, for ordering when epoch is unset.
    pub uptime_ms: u64,
    /// Flow rate that triggered the latch (L/h).
    pub flow_lph: f32,
    /// Configured sustained-duration that was breached (seconds).
    pub duration_secs: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DeviceSnapshot {
    pub sensors: Sensors,
    pub switches: Switches,
    pub network: Network,
    pub diagnostics: Diagnostics,
    pub alarm: FlowAlarm,
    pub uptime_ms: u64,
    pub firmware_version: String,
}

/// Mutex-protected state. Use `lock` for cheap read-modify-write; for
/// long-running readers, take a `snapshot()` to release the lock quickly.
#[derive(Default)]
pub struct DeviceState {
    inner: Mutex<DeviceSnapshot>,
}

impl DeviceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        self.inner.lock().clone()
    }

    pub fn update<F: FnOnce(&mut DeviceSnapshot)>(&self, f: F) {
        let mut g = self.inner.lock();
        f(&mut g);
    }
}
