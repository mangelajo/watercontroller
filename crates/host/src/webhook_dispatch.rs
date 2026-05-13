//! Host-side webhook dispatcher mirror of the firmware impl.
//!
//! Reads `app.config().webhooks` on every event and POSTs each
//! subscriber with the templated body. Used by the playwright tests
//! to exercise the same webhook contract the device sees.

use std::sync::mpsc::{self, SyncSender};
use std::sync::Mutex;
use std::thread;
use watercontroller_core::app::App;
use watercontroller_core::webhook::{render_template, WebhookConfig, WebhookDispatcher, WebhookEvent};

const QUEUE_CAP: usize = 16;

pub struct HostWebhookDispatcher {
    tx: Mutex<SyncSender<WebhookEvent>>,
}

impl HostWebhookDispatcher {
    pub fn spawn(app: App) -> Self {
        let (tx, rx) = mpsc::sync_channel::<WebhookEvent>(QUEUE_CAP);
        thread::Builder::new()
            .name("webhook-sup".into())
            .spawn(move || {
                for event in rx.iter() {
                    handle_event(&app, &event);
                }
            })
            .expect("spawn webhook-sup");
        Self { tx: Mutex::new(tx) }
    }
}

impl WebhookDispatcher for HostWebhookDispatcher {
    fn dispatch(&self, event: WebhookEvent) {
        if let Err(e) = self.tx.lock().unwrap().try_send(event) {
            log::warn!("webhook queue full, dropping: {e:?}");
        }
    }
}

fn handle_event(app: &App, event: &WebhookEvent) {
    let cfg = app.config();
    let subs: Vec<WebhookConfig> = cfg
        .webhooks
        .iter()
        .filter(|w| w.enabled && w.events.iter().any(|e| *e == event.kind))
        .cloned()
        .collect();
    drop(cfg);
    if subs.is_empty() {
        return;
    }
    log::info!(
        "webhook: dispatching {} to {} subscriber(s)",
        event.kind.as_str(),
        subs.len()
    );
    for wh in subs {
        let body = render_template(&wh.body_template, &event.vars);
        match post(&wh, &body) {
            Ok(status) if (200..300).contains(&status) => {
                log::info!("webhook: {} -> {} OK ({status})", event.kind.as_str(), wh.url);
            }
            Ok(status) => {
                log::warn!("webhook: {} -> {} HTTP {status}", event.kind.as_str(), wh.url);
            }
            Err(e) => log::warn!("webhook: {} -> {} failed: {e}", event.kind.as_str(), wh.url),
        }
    }
}

fn post(wh: &WebhookConfig, body: &str) -> Result<u16, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(8))
        .build();
    let mut req = match wh.method.to_ascii_uppercase().as_str() {
        "POST" => agent.post(&wh.url),
        "PUT" => agent.put(&wh.url),
        m => return Err(format!("unsupported method {m}")),
    };
    let mut has_ct = false;
    for h in &wh.headers {
        if h.name.eq_ignore_ascii_case("content-type") {
            has_ct = true;
        }
        req = req.set(&h.name, &h.value);
    }
    if !has_ct {
        req = req.set("Content-Type", "application/json");
    }
    match req.send_string(body) {
        Ok(resp) => Ok(resp.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(e) => Err(format!("{e}")),
    }
}
