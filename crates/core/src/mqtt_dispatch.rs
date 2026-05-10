//! MQTT integration: HA Discovery publish + command-topic routing + state
//! publishing. Pure logic that takes any `Mqtt` implementation; tested on
//! the host with `FakeMqtt`, runs identically on firmware against the real
//! `esp-idf-svc` MQTT client.

use crate::app::App;
use crate::api::{CommandOutcome, SwitchCommand};
use crate::config::Config;
use crate::ha_discovery::{all_messages, DeviceContext};
use crate::state::{DeviceSnapshot, WaterControlState};
use crate::traits::{Mqtt, PublishOpts};
use std::sync::Arc;

pub struct MqttIntegration {
    pub app: App,
    pub mqtt: Arc<dyn Mqtt>,
    pub firmware_version: String,
}

impl MqttIntegration {
    /// Run once after a (re)connect: publish HA Discovery configs + the
    /// availability "online" message, subscribe to command topics, and
    /// install the message handler.
    pub fn on_connect(&self) {
        let cfg = self.app.config();
        let ctx = self.context(&cfg);
        let availability = format!("{}/status", &cfg.mqtt.base_topic);

        // Discovery first — HA picks up entities and binds to the state topics.
        for msg in all_messages(&ctx) {
            self.mqtt.publish(&msg.topic, &msg.payload, retained());
        }
        // LWT-equivalent online marker.
        self.mqtt.publish(&availability, b"online", retained());

        // Subscribe to command topics for every switch.
        for key in ["sprinkler_1", "sprinkler_2", "water_control"] {
            self.mqtt.subscribe(&ctx.switch_command_topic(key));
        }

        // Install the routing handler.
        let app = self.app.clone();
        let base_topic = cfg.mqtt.base_topic.clone();
        self.mqtt
            .set_handler(Box::new(move |topic: &str, payload: &[u8]| {
                let Some(cmd) = parse_command(&base_topic, topic, payload) else {
                    log::debug!("mqtt: ignoring unrouted topic {topic}");
                    return;
                };
                match app.switch_command(cmd.clone()) {
                    CommandOutcome::Ok => log::debug!("mqtt cmd applied: {cmd:?}"),
                    CommandOutcome::Busy { reason } => {
                        log::warn!("mqtt cmd rejected ({reason}): {cmd:?}");
                    }
                }
            }));
    }

    /// Publish the current device state to per-entity state topics. Call this
    /// on a cadence (e.g. once per second on firmware) — duplicates are
    /// cheap because retained messages overwrite.
    pub fn publish_state(&self, snap: &DeviceSnapshot) {
        let cfg = self.app.config();
        let ctx = self.context(&cfg);
        let publish = |key: &str, payload: String| {
            self.mqtt.publish(
                &ctx.sensor_state_topic(key),
                payload.as_bytes(),
                retained(),
            );
        };
        if let Some(v) = snap.sensors.battery_v {
            publish("battery", format!("{v:.2}"));
        }
        if let Some(v) = snap.sensors.pressure_bar {
            publish("pressure", format!("{v:.2}"));
        }
        if let Some(v) = snap.sensors.flow_lph {
            publish("water_flow", format!("{v:.1}"));
        }
        if let Some(v) = snap.sensors.total_l {
            publish("water_total", format!("{v:.3}"));
        }

        let sw = |key: &str, on: bool| {
            self.mqtt.publish(
                &ctx.switch_state_topic(key),
                if on { b"ON" } else { b"OFF" },
                retained(),
            );
        };
        sw("sprinkler_1", snap.switches.sprinkler_1);
        sw("sprinkler_2", snap.switches.sprinkler_2);
        // For water control, publish the user-visible state — Transitioning
        // is reported as ON since the user-visible state is "becoming on";
        // alternative would be a separate attribute. Keeping it simple.
        let on = matches!(
            snap.switches.water_control,
            WaterControlState::On | WaterControlState::Transitioning
        );
        sw("water_control", on);
    }

    /// On graceful shutdown / disconnect: best-effort offline marker.
    pub fn publish_offline(&self) {
        let cfg = self.app.config();
        let availability = format!("{}/status", &cfg.mqtt.base_topic);
        self.mqtt.publish(&availability, b"offline", retained());
    }

    fn context<'a>(&'a self, cfg: &'a Config) -> DeviceContext<'a> {
        DeviceContext {
            base_topic: &cfg.mqtt.base_topic,
            discovery_prefix: &cfg.mqtt.ha_discovery_prefix,
            device_id: &cfg.wifi.hostname,
            friendly_name: "Doremorwater",
            sw_version: &self.firmware_version,
            manufacturer: "homebrew",
            model: "watercontroller",
        }
    }
}

fn retained() -> PublishOpts {
    PublishOpts { retained: true, qos: 1 }
}

/// Parses a topic + payload into a `SwitchCommand`, or `None` if the topic
/// is not a recognised command topic for this device.
pub fn parse_command(base_topic: &str, topic: &str, payload: &[u8]) -> Option<SwitchCommand> {
    let prefix = format!("{base_topic}/");
    let rest = topic.strip_prefix(&prefix)?;
    let (key, suffix) = rest.split_once('/')?;
    if suffix != "set" {
        return None;
    }
    let on = match payload {
        b"ON" | b"on" | b"1" | b"true" => true,
        b"OFF" | b"off" | b"0" | b"false" => false,
        _ => return None,
    };
    match key {
        "sprinkler_1" => Some(SwitchCommand::Sprinkler1 { on }),
        "sprinkler_2" => Some(SwitchCommand::Sprinkler2 { on }),
        "water_control" => Some(SwitchCommand::WaterControl { on }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Mqtt, PublishOpts};
    use std::sync::Mutex;

    struct CaptureMqtt {
        published: Mutex<Vec<(String, Vec<u8>, PublishOpts)>>,
        subscriptions: Mutex<Vec<String>>,
        handler: Mutex<Option<Box<dyn Fn(&str, &[u8]) + Send + Sync>>>,
    }
    impl CaptureMqtt {
        fn new() -> Self {
            Self {
                published: Mutex::new(Vec::new()),
                subscriptions: Mutex::new(Vec::new()),
                handler: Mutex::new(None),
            }
        }
        fn deliver(&self, topic: &str, payload: &[u8]) {
            if let Some(h) = self.handler.lock().unwrap().as_ref() {
                h(topic, payload);
            }
        }
    }
    impl Mqtt for CaptureMqtt {
        fn publish(&self, topic: &str, payload: &[u8], opts: PublishOpts) {
            self.published.lock().unwrap().push((topic.into(), payload.to_vec(), opts));
        }
        fn subscribe(&self, topic: &str) {
            self.subscriptions.lock().unwrap().push(topic.into());
        }
        fn set_handler(&self, h: Box<dyn Fn(&str, &[u8]) + Send + Sync>) {
            *self.handler.lock().unwrap() = Some(h);
        }
        fn is_connected(&self) -> bool {
            true
        }
    }

    fn fake_app() -> App {
        use crate::traits::Clock;
        use chrono::{DateTime, Utc};
        use std::sync::atomic::{AtomicU64, Ordering};

        struct C { ms: AtomicU64 }
        impl Clock for C {
            fn now(&self) -> DateTime<Utc> { DateTime::from_timestamp(0,0).unwrap() }
            fn monotonic_ms(&self) -> u64 { self.ms.load(Ordering::SeqCst) }
        }
        App::new(Arc::new(C { ms: AtomicU64::new(0) }), Config::default())
    }

    #[test]
    fn parse_command_routes_correctly() {
        let cmd = parse_command("doremorwater", "doremorwater/water_control/set", b"ON").unwrap();
        assert_eq!(cmd, SwitchCommand::WaterControl { on: true });

        let cmd = parse_command("doremorwater", "doremorwater/sprinkler_1/set", b"OFF").unwrap();
        assert_eq!(cmd, SwitchCommand::Sprinkler1 { on: false });

        // Wrong base
        assert!(parse_command("doremorwater", "elsewhere/water_control/set", b"ON").is_none());
        // Wrong suffix
        assert!(parse_command("doremorwater", "doremorwater/water_control/state", b"ON").is_none());
        // Unknown switch
        assert!(parse_command("doremorwater", "doremorwater/heater/set", b"ON").is_none());
        // Bad payload
        assert!(parse_command("doremorwater", "doremorwater/water_control/set", b"maybe").is_none());
    }

    #[test]
    fn on_connect_publishes_discovery_subscribes_routes_commands() {
        let app = fake_app();
        let mqtt = Arc::new(CaptureMqtt::new());
        let integ = MqttIntegration {
            app: app.clone(),
            mqtt: mqtt.clone(),
            firmware_version: "test".into(),
        };
        integ.on_connect();

        // 8 discovery messages + 1 availability online
        let pubs = mqtt.published.lock().unwrap();
        assert_eq!(pubs.len(), 9);
        // Subscriptions for each command topic
        let subs = mqtt.subscriptions.lock().unwrap();
        assert_eq!(subs.len(), 3);
        drop(pubs);
        drop(subs);

        // Inject a command — should be routed into App
        mqtt.deliver("doremorwater/sprinkler_1/set", b"ON");
        app.tick();
        assert!(app.snapshot().switches.sprinkler_1);
    }
}
