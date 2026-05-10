//! Builds Home Assistant MQTT Discovery payloads.
//!
//! Each entity gets a retained config message at
//! `<discovery_prefix>/<component>/<unique_id>/config`. HA picks up the topic
//! and auto-creates the entity. All entities share one `device` block so HA
//! groups them under a single device card.

use serde::Serialize;

#[derive(Debug, Clone)]
pub struct DeviceContext<'a> {
    pub base_topic: &'a str,         // e.g. "doremorwater"
    pub discovery_prefix: &'a str,   // e.g. "homeassistant"
    pub device_id: &'a str,          // e.g. "doremorwater"
    pub friendly_name: &'a str,      // e.g. "Doremorwater"
    pub sw_version: &'a str,
    pub manufacturer: &'a str,
    pub model: &'a str,
}

#[derive(Serialize, Debug, Clone)]
struct DeviceBlock<'a> {
    identifiers: [&'a str; 1],
    name: &'a str,
    manufacturer: &'a str,
    model: &'a str,
    sw_version: &'a str,
}

#[derive(Serialize, Debug)]
struct SensorPayload<'a> {
    name: &'a str,
    unique_id: String,
    object_id: String,
    state_topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_of_measurement: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_display_precision: Option<u8>,
    availability_topic: String,
    payload_available: &'static str,
    payload_not_available: &'static str,
    device: DeviceBlock<'a>,
}

#[derive(Serialize, Debug)]
struct SwitchPayload<'a> {
    name: &'a str,
    unique_id: String,
    object_id: String,
    state_topic: String,
    command_topic: String,
    payload_on: &'static str,
    payload_off: &'static str,
    state_on: &'static str,
    state_off: &'static str,
    optimistic: bool,
    availability_topic: String,
    payload_available: &'static str,
    payload_not_available: &'static str,
    device: DeviceBlock<'a>,
}

pub struct DiscoveryMsg {
    pub topic: String,
    pub payload: Vec<u8>,
}

pub struct Sensor<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub unit: Option<&'a str>,
    pub device_class: Option<&'a str>,
    pub state_class: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub precision: Option<u8>,
}

pub struct Switch<'a> {
    pub key: &'a str,
    pub name: &'a str,
}

impl<'a> DeviceContext<'a> {
    fn device_block(&self) -> DeviceBlock<'a> {
        DeviceBlock {
            identifiers: [self.device_id],
            name: self.friendly_name,
            manufacturer: self.manufacturer,
            model: self.model,
            sw_version: self.sw_version,
        }
    }

    fn availability_topic(&self) -> String {
        format!("{}/status", self.base_topic)
    }

    pub fn sensor_state_topic(&self, key: &str) -> String {
        format!("{}/{}/state", self.base_topic, key)
    }

    pub fn switch_state_topic(&self, key: &str) -> String {
        format!("{}/{}/state", self.base_topic, key)
    }

    pub fn switch_command_topic(&self, key: &str) -> String {
        format!("{}/{}/set", self.base_topic, key)
    }

    fn unique_id(&self, key: &str) -> String {
        format!("{}_{}", self.device_id, key)
    }

    pub fn sensor(&self, s: &Sensor<'a>) -> DiscoveryMsg {
        let unique_id = self.unique_id(s.key);
        let payload = SensorPayload {
            name: s.name,
            unique_id: unique_id.clone(),
            object_id: unique_id.clone(),
            state_topic: self.sensor_state_topic(s.key),
            unit_of_measurement: s.unit,
            device_class: s.device_class,
            state_class: s.state_class,
            icon: s.icon,
            suggested_display_precision: s.precision,
            availability_topic: self.availability_topic(),
            payload_available: "online",
            payload_not_available: "offline",
            device: self.device_block(),
        };
        DiscoveryMsg {
            topic: format!(
                "{}/sensor/{}/config",
                self.discovery_prefix, unique_id
            ),
            payload: serde_json::to_vec(&payload).expect("HA sensor payload serializes"),
        }
    }

    pub fn switch(&self, s: &Switch<'a>) -> DiscoveryMsg {
        let unique_id = self.unique_id(s.key);
        let payload = SwitchPayload {
            name: s.name,
            unique_id: unique_id.clone(),
            object_id: unique_id.clone(),
            state_topic: self.switch_state_topic(s.key),
            command_topic: self.switch_command_topic(s.key),
            payload_on: "ON",
            payload_off: "OFF",
            state_on: "ON",
            state_off: "OFF",
            optimistic: false,
            availability_topic: self.availability_topic(),
            payload_available: "online",
            payload_not_available: "offline",
            device: self.device_block(),
        };
        DiscoveryMsg {
            topic: format!(
                "{}/switch/{}/config",
                self.discovery_prefix, unique_id
            ),
            payload: serde_json::to_vec(&payload).expect("HA switch payload serializes"),
        }
    }
}

/// Build the full set of discovery messages for the doremorwater device:
/// 3 sensors (battery, pressure, water_flow) + 2 derived sensors
/// (water_total, loop_time) + 3 user-facing switches (sprinkler_1,
/// sprinkler_2, water_control).
pub fn all_messages(ctx: &DeviceContext) -> Vec<DiscoveryMsg> {
    let mut out = Vec::new();
    out.push(ctx.sensor(&Sensor {
        key: "battery",
        name: "Battery",
        unit: Some("V"),
        device_class: Some("voltage"),
        state_class: Some("measurement"),
        icon: None,
        precision: Some(2),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "pressure",
        name: "Pressure",
        unit: Some("bar"),
        device_class: Some("pressure"),
        state_class: Some("measurement"),
        icon: None,
        precision: Some(2),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "water_flow",
        name: "Water flow",
        unit: Some("L/h"),
        device_class: Some("water"),
        state_class: Some("measurement"),
        icon: Some("mdi:water"),
        precision: Some(1),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "water_total",
        name: "Total water",
        unit: Some("L"),
        device_class: Some("water"),
        state_class: Some("total_increasing"),
        icon: Some("mdi:water"),
        precision: Some(3),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "loop_time",
        name: "Loop time",
        unit: Some("ms"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:timer-outline"),
        precision: Some(0),
    }));
    // Diagnostic sensors — published as regular sensors but surfaced under
    // HA's "diagnostic" entity category in the device card.
    out.push(ctx.sensor(&Sensor {
        key: "wifi_rssi",
        name: "WiFi signal",
        unit: Some("dBm"),
        device_class: Some("signal_strength"),
        state_class: Some("measurement"),
        icon: None,
        precision: Some(0),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "free_heap",
        name: "Free heap",
        unit: Some("B"),
        device_class: Some("data_size"),
        state_class: Some("measurement"),
        icon: Some("mdi:memory"),
        precision: Some(0),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "uptime",
        name: "Uptime",
        unit: Some("s"),
        device_class: Some("duration"),
        state_class: Some("total_increasing"),
        icon: Some("mdi:timer-outline"),
        precision: Some(0),
    }));
    out.push(ctx.sensor(&Sensor {
        key: "reset_reason",
        name: "Reset reason",
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:restart-alert"),
        precision: None,
    }));
    out.push(ctx.switch(&Switch { key: "sprinkler_1", name: "Riego exterior" }));
    out.push(ctx.switch(&Switch { key: "sprinkler_2", name: "Riego mobil" }));
    out.push(ctx.switch(&Switch { key: "water_control", name: "Water control" }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> DeviceContext<'static> {
        DeviceContext {
            base_topic: "doremorwater",
            discovery_prefix: "homeassistant",
            device_id: "doremorwater",
            friendly_name: "Doremorwater",
            sw_version: "0.1.0",
            manufacturer: "homebrew",
            model: "watercontroller",
        }
    }

    #[test]
    fn pressure_sensor_topic_and_keys() {
        let m = ctx().sensor(&Sensor {
            key: "pressure",
            name: "Pressure",
            unit: Some("bar"),
            device_class: Some("pressure"),
            state_class: Some("measurement"),
            icon: None,
            precision: Some(2),
        });
        assert_eq!(
            m.topic,
            "homeassistant/sensor/doremorwater_pressure/config"
        );
        let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap();
        assert_eq!(v["unique_id"], "doremorwater_pressure");
        assert_eq!(v["state_topic"], "doremorwater/pressure/state");
        assert_eq!(v["unit_of_measurement"], "bar");
        assert_eq!(v["device_class"], "pressure");
        assert_eq!(v["device"]["identifiers"][0], "doremorwater");
    }

    #[test]
    fn switch_has_command_topic() {
        let m = ctx().switch(&Switch { key: "water_control", name: "Water control" });
        let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap();
        assert_eq!(v["command_topic"], "doremorwater/water_control/set");
        assert_eq!(v["state_topic"], "doremorwater/water_control/state");
        assert_eq!(v["optimistic"], false);
    }

    #[test]
    fn all_messages_includes_expected_entities() {
        let msgs = all_messages(&ctx());
        let topics: Vec<_> = msgs.iter().map(|m| m.topic.clone()).collect();
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_battery")));
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_pressure")));
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_water_flow")));
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_water_total")));
        assert!(topics
            .iter()
            .any(|t| t.contains("switch/doremorwater_water_control")));
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_wifi_rssi")));
        assert!(topics
            .iter()
            .any(|t| t.contains("sensor/doremorwater_uptime")));
        // 5 measurement sensors + 4 diagnostic sensors + 3 switches = 12.
        assert_eq!(msgs.len(), 12);
    }
}
