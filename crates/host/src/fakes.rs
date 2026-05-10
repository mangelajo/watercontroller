//! Fake trait implementations for the native (host) build. These let us run
//! the SPA + business logic locally without an ESP32. Mocks are intentionally
//! shallow — anything that needs to behave like real hardware (e.g. WiFi
//! reconnection) is stubbed and exercised on real hardware instead.
//!
//! Several fakes are unused in the current host wiring; they exist so tests
//! and per-feature integration runs (FakeMqtt for MQTT routing, etc.) can
//! import and use them. The dead-code allow keeps the warnings quiet without
//! deleting fakes that are part of the contract surface.

#![allow(dead_code)]

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use watercontroller_core::traits::{
    Adc, Clock, GpioOut, Mqtt, NvsError, NvsStore, PublishOpts, PulseCounter, Wifi, WifiCreds,
    WifiState,
};

pub struct WallClock {
    started: Instant,
}
impl WallClock {
    pub fn new() -> Self {
        Self { started: Instant::now() }
    }
}
impl Default for WallClock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock for WallClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
    fn monotonic_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

pub struct FakeGpio {
    pub label: &'static str,
    pub state: Arc<Mutex<bool>>,
}
impl FakeGpio {
    pub fn new(label: &'static str) -> Self {
        Self { label, state: Arc::new(Mutex::new(false)) }
    }
    pub fn handle(&self) -> Arc<Mutex<bool>> {
        self.state.clone()
    }
}
impl GpioOut for FakeGpio {
    fn set(&mut self, high: bool) {
        let mut g = self.state.lock();
        if *g != high {
            log::debug!("[fake-gpio:{}] -> {}", self.label, if high { "HIGH" } else { "LOW" });
        }
        *g = high;
    }
}

/// ADC that returns a fixed (settable) value. Useful for driving the SPA from
/// the host without a real sensor.
pub struct FakeAdc {
    pub value: Arc<AtomicU16>,
}
impl FakeAdc {
    pub fn new(initial: u16) -> Self {
        Self { value: Arc::new(AtomicU16::new(initial)) }
    }
}
impl Adc for FakeAdc {
    fn read_raw(&mut self) -> u16 {
        self.value.load(Ordering::Relaxed)
    }
}

pub struct FakePulseCounter {
    pub count: Arc<AtomicU64>,
}
impl FakePulseCounter {
    pub fn new(initial: u64) -> Self {
        Self { count: Arc::new(AtomicU64::new(initial)) }
    }
}
impl PulseCounter for FakePulseCounter {
    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

pub struct FakeNvs {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}
impl FakeNvs {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }
}
impl Default for FakeNvs {
    fn default() -> Self {
        Self::new()
    }
}
impl NvsStore for FakeNvs {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().get(key).cloned()
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), NvsError> {
        self.inner.lock().insert(key.into(), value.to_vec());
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<(), NvsError> {
        self.inner.lock().remove(key);
        Ok(())
    }
}

/// MQTT fake that records every publish + maintains a list of subscriptions.
/// Inbound messages can be injected via `inject` for command-routing tests.
pub struct FakeMqtt {
    pub published: Mutex<Vec<(String, Vec<u8>, PublishOpts)>>,
    pub subscriptions: Mutex<Vec<String>>,
    pub handler: Mutex<Option<Box<dyn Fn(&str, &[u8]) + Send + Sync>>>,
    pub connected: bool,
}
impl FakeMqtt {
    pub fn new() -> Self {
        Self {
            published: Mutex::new(Vec::new()),
            subscriptions: Mutex::new(Vec::new()),
            handler: Mutex::new(None),
            connected: true,
        }
    }
    pub fn inject(&self, topic: &str, payload: &[u8]) {
        if let Some(h) = &*self.handler.lock() {
            h(topic, payload);
        }
    }
    pub fn publishes(&self) -> Vec<(String, Vec<u8>)> {
        self.published
            .lock()
            .iter()
            .map(|(t, p, _)| (t.clone(), p.clone()))
            .collect()
    }
}
impl Default for FakeMqtt {
    fn default() -> Self {
        Self::new()
    }
}
impl Mqtt for FakeMqtt {
    fn publish(&self, topic: &str, payload: &[u8], opts: PublishOpts) {
        self.published.lock().push((topic.into(), payload.to_vec(), opts));
    }
    fn subscribe(&self, topic: &str) {
        self.subscriptions.lock().push(topic.into());
    }
    fn set_handler(&self, handler: Box<dyn Fn(&str, &[u8]) + Send + Sync>) {
        *self.handler.lock() = Some(handler);
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
}

/// "WiFi is always connected" stub. We do not invest in faking radio behavior.
pub struct FakeWifi {
    pub state: Mutex<WifiState>,
}
impl FakeWifi {
    pub fn connected_to(ssid: &str, ip: &str) -> Self {
        Self {
            state: Mutex::new(WifiState::Connected {
                ssid: ssid.into(),
                ip: ip.into(),
            }),
        }
    }
}
impl Wifi for FakeWifi {
    fn state(&self) -> WifiState {
        self.state.lock().clone()
    }
    fn connect(&self, _networks: &[WifiCreds]) {}
    fn reconnect(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_gpio_records_changes() {
        let mut g = FakeGpio::new("test");
        let h = g.handle();
        assert!(!*h.lock());
        g.set(true);
        assert!(*h.lock());
        g.set(false);
        assert!(!*h.lock());
    }

    #[test]
    fn fake_mqtt_routes_handler() {
        let m = FakeMqtt::new();
        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let r = received.clone();
        m.set_handler(Box::new(move |topic, _| {
            r.lock().push(topic.into());
        }));
        m.inject("doremorwater/water_control/set", b"ON");
        assert_eq!(received.lock().len(), 1);
    }
}
