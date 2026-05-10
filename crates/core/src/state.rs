//! Shared, mutex-protected device state. The single source of truth read by
//! the MQTT publisher, the HTTP API, and the schedule executor.

use crate::traits::WifiState;
use crate::water_valve::WaterState;
use serde::Serialize;
use std::sync::Mutex;

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

#[derive(Debug, Clone, Serialize, Default)]
pub struct DeviceSnapshot {
    pub sensors: Sensors,
    pub switches: Switches,
    pub network: Network,
    pub diagnostics: Diagnostics,
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
        self.inner.lock().unwrap().clone()
    }

    pub fn update<F: FnOnce(&mut DeviceSnapshot)>(&self, f: F) {
        let mut g = self.inner.lock().unwrap();
        f(&mut g);
    }
}
