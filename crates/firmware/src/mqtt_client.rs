//! `Mqtt` trait implementation backed by ESP-IDF's MQTT client.
//!
//! The client owns the connection lifecycle internally (auto-reconnect with
//! backoff on disconnect). Incoming messages are dispatched to a handler
//! installed via `set_handler`. Publish + subscribe are non-blocking enqueues.
//!
//! TLS is configured implicitly when the broker URL uses `mqtts://` (the
//! ESP-IDF client uses the already-linked mbedTLS).

use anyhow::Result;
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttEvent, EventPayload, MqttClientConfiguration, QoS,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use watercontroller_core::traits::{Mqtt, PublishOpts};

type Handler = Box<dyn Fn(&str, &[u8]) + Send + Sync>;

pub struct EspMqtt {
    inner: Mutex<Option<EspMqttClient<'static>>>,
    handler: Arc<Mutex<Option<Handler>>>,
    connected: Arc<Mutex<bool>>,
}

impl EspMqtt {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            handler: Arc::new(Mutex::new(None)),
            connected: Arc::new(Mutex::new(false)),
        }
    }

    /// (Re)connect to the configured broker. Safe to call multiple times —
    /// previous client is dropped first. PEM cert strings, when non-empty,
    /// are leaked into a `&'static [u8]` (with NUL terminator) so the
    /// underlying mbedTLS retains them for the lifetime of the client.
    pub fn connect(
        &self,
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        client_id: &str,
        ca_cert_pem: &str,
        client_cert_pem: &str,
        client_key_pem: &str,
    ) -> Result<()> {
        let server_cert = if ca_cert_pem.is_empty() {
            None
        } else {
            Some(esp_idf_svc::tls::X509::pem(leak_cstr(ca_cert_pem)))
        };
        let client_cert = if client_cert_pem.is_empty() {
            None
        } else {
            Some(esp_idf_svc::tls::X509::pem(leak_cstr(client_cert_pem)))
        };
        let private_key = if client_key_pem.is_empty() {
            None
        } else {
            Some(esp_idf_svc::tls::X509::pem(leak_cstr(client_key_pem)))
        };

        let cfg = MqttClientConfiguration {
            client_id: Some(client_id),
            username,
            password,
            keep_alive_interval: Some(Duration::from_secs(30)),
            server_certificate: server_cert,
            client_certificate: client_cert,
            private_key,
            ..Default::default()
        };
        // Tear down the previous client BEFORE allocating the new one.
        // EspMqttClient's Drop releases its event-loop queue + internal
        // pthread mutexes. If we let the new client overlap the old one
        // (which the obvious `*self.inner.lock() = Some(new)` does — it
        // drops only after the new is built), we leak event-loop queues
        // on every reconnect. Under an auth-refused retry storm that
        // pool exhausts and esp_event_handler_register_with asserts
        // `event_loop != NULL`. We also saw a use-after-free crash in
        // publish() (pthread_mutex_lock on a freed internal NVS mutex)
        // from the same overlap window.
        *self.inner.lock().unwrap() = None;
        *self.connected.lock().unwrap() = false;

        let handler = self.handler.clone();
        let connected = self.connected.clone();
        let client = EspMqttClient::new_cb(url, &cfg, move |evt: EspMqttEvent| {
            on_event(&handler, &connected, evt)
        })?;
        *self.inner.lock().unwrap() = Some(client);
        Ok(())
    }
}

fn leak_cstr(s: &str) -> &'static std::ffi::CStr {
    let mut bytes = s.as_bytes().to_vec();
    if !bytes.last().is_some_and(|&b| b == 0) {
        bytes.push(0);
    }
    let leaked: &'static [u8] = bytes.leak();
    std::ffi::CStr::from_bytes_with_nul(leaked).expect("nul-terminated above")
}

impl Default for EspMqtt {
    fn default() -> Self {
        Self::new()
    }
}

fn on_event(
    handler: &Arc<Mutex<Option<Handler>>>,
    connected: &Arc<Mutex<bool>>,
    event: EspMqttEvent,
) {
    match event.payload() {
        EventPayload::Connected(_) => {
            *connected.lock().unwrap() = true;
            log::info!("mqtt: connected");
        }
        EventPayload::Disconnected => {
            *connected.lock().unwrap() = false;
            log::warn!("mqtt: disconnected");
        }
        EventPayload::Received { topic, data, .. } => {
            if let Some(h) = handler.lock().unwrap().as_ref() {
                if let Some(t) = topic {
                    h(t, data);
                }
            }
        }
        EventPayload::Error(e) => log::warn!("mqtt error: {e:?}"),
        _ => {}
    }
}

impl Mqtt for EspMqtt {
    fn publish(&self, topic: &str, payload: &[u8], opts: PublishOpts) {
        if let Some(c) = self.inner.lock().unwrap().as_mut() {
            let qos = match opts.qos {
                0 => QoS::AtMostOnce,
                2 => QoS::ExactlyOnce,
                _ => QoS::AtLeastOnce,
            };
            if let Err(e) = c.enqueue(topic, qos, opts.retained, payload) {
                log::warn!("mqtt enqueue {topic} failed: {e:?}");
            }
        }
    }

    fn subscribe(&self, topic: &str) {
        if let Some(c) = self.inner.lock().unwrap().as_mut() {
            if let Err(e) = c.subscribe(topic, QoS::AtLeastOnce) {
                log::warn!("mqtt subscribe {topic} failed: {e:?}");
            }
        }
    }

    fn set_handler(&self, h: Handler) {
        *self.handler.lock().unwrap() = Some(h);
    }

    fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }
}
