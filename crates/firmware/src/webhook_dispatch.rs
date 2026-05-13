//! Firmware-side webhook dispatcher: implements
//! `watercontroller_core::webhook::WebhookDispatcher` by enqueuing
//! events into an MPSC channel that a dedicated task drains.
//!
//! Dispatch is non-blocking. The producer side runs from `App::tick`,
//! HTTP handlers, etc. — all of those would melt if a webhook POST
//! to a distant Slack endpoint blocked them. The consumer task does
//! the actual HTTP work (handshake, render, send, await response).
//!
//! Single attempt per event for now. A future commit can add a small
//! retry queue; today, a failed POST just logs.

use embedded_svc::http::client::Connection;
use embedded_svc::utils::io::try_read_full;
use esp_idf_svc::http::client::{Configuration as HttpClientConfiguration, EspHttpConnection};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Mutex;
use std::time::Duration;
use watercontroller_core::app::App;
use watercontroller_core::config::Config;
use watercontroller_core::webhook::{
    render_template, WebhookConfig, WebhookDispatcher, WebhookEvent,
};

/// Cap on the channel so a flood of events (e.g. config-change spam)
/// can't pile up in heap. 16 is plenty for the firmware's event rate
/// (alarm fires, config saves, schedule fires) — if the queue fills,
/// we drop with a warn rather than block the producer.
const QUEUE_CAP: usize = 16;

pub struct EspWebhookDispatcher {
    tx: Mutex<SyncSender<WebhookEvent>>,
}

impl EspWebhookDispatcher {
    /// Spawn the dispatch task and return the dispatcher handle. The
    /// task reads the *current* `app.config().webhooks` on every event
    /// (no caching), so config edits via /api/config/webhooks take
    /// effect on the next event without restarting the task.
    pub fn spawn(app: App) -> Self {
        // Bounded so a producer flood can't grow heap unbounded — when
        // full, `try_send` rejects and the dispatch impl below logs +
        // drops rather than blocking the calling task.
        let (tx, rx) = mpsc::sync_channel::<WebhookEvent>(QUEUE_CAP);
        let app_clone = app.clone();
        // 16 KiB stack: the HTTP client + mbedTLS handshake path is
        // hungry (an 8 KiB run overflowed during the first
        // config.changed dispatch on hardware). Headroom is more
        // important than DRAM here — we have plenty in PSRAM.
        crate::task_util::spawn_named(c"webhook-sup", 16 * 1024, move || {
            log::info!("webhook dispatcher task started");
            for event in rx.iter() {
                handle_event(&app_clone, &event);
            }
        });
        Self {
            tx: Mutex::new(tx),
        }
    }
}

impl WebhookDispatcher for EspWebhookDispatcher {
    fn dispatch(&self, event: WebhookEvent) {
        let tx = self.tx.lock().unwrap();
        if let Err(e) = tx.try_send(event) {
            log::warn!("webhook queue full, dropping: {e:?}");
        }
    }
}

fn handle_event(app: &App, event: &WebhookEvent) {
    let cfg: std::sync::Arc<Config> = app.config();
    let kind = event.kind;
    // Snapshot the subscribers for this event up-front so we don't
    // hold the Arc<Config> reference across slow HTTP I/O.
    let subs: Vec<WebhookConfig> = cfg
        .webhooks
        .iter()
        .filter(|w| w.enabled && w.events.iter().any(|e| *e == kind))
        .cloned()
        .collect();
    drop(cfg);
    if subs.is_empty() {
        log::debug!("webhook: no subscribers for {}", kind.as_str());
        return;
    }
    log::info!(
        "webhook: dispatching {} to {} subscriber(s)",
        kind.as_str(),
        subs.len()
    );
    for wh in subs {
        let body = render_template(&wh.body_template, &event.vars);
        match post(&wh, &body) {
            Ok(status) => {
                if (200..300).contains(&status) {
                    log::info!("webhook: {} -> {} OK ({status})", kind.as_str(), wh.url);
                } else {
                    log::warn!(
                        "webhook: {} -> {} HTTP {status}",
                        kind.as_str(),
                        wh.url
                    );
                }
            }
            Err(e) => log::warn!("webhook: {} -> {} failed: {e}", kind.as_str(), wh.url),
        }
    }
}

fn post(wh: &WebhookConfig, body: &str) -> Result<u16, String> {
    let is_https = wh.url.starts_with("https://");
    let cfg = HttpClientConfiguration {
        timeout: Some(Duration::from_secs(8)),
        // For HTTPS we use the global CA bundle baked into IDF.
        // Slack / Discord chain to Let's Encrypt / Amazon / DigiCert
        // which are in the default crt_bundle.
        use_global_ca_store: is_https,
        crt_bundle_attach: if is_https {
            Some(esp_idf_svc::sys::esp_crt_bundle_attach)
        } else {
            None
        },
        ..Default::default()
    };
    let mut conn = EspHttpConnection::new(&cfg).map_err(|e| format!("conn: {e:?}"))?;

    let method = match wh.method.to_ascii_uppercase().as_str() {
        "PUT" => embedded_svc::http::Method::Put,
        "POST" => embedded_svc::http::Method::Post,
        m => return Err(format!("unsupported method {m}")),
    };

    let body_bytes = body.as_bytes();
    let content_len = body_bytes.len().to_string();

    // Build header list, defaulting Content-Type to JSON if the user
    // didn't set it explicitly.
    let mut has_content_type = false;
    let mut header_refs: Vec<(&str, &str)> = Vec::with_capacity(wh.headers.len() + 2);
    for h in &wh.headers {
        if h.name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        header_refs.push((h.name.as_str(), h.value.as_str()));
    }
    if !has_content_type {
        header_refs.push(("Content-Type", "application/json"));
    }
    header_refs.push(("Content-Length", content_len.as_str()));

    conn.initiate_request(method, &wh.url, &header_refs)
        .map_err(|e| format!("initiate: {e:?}"))?;
    embedded_svc::io::Write::write_all(&mut conn, body_bytes)
        .map_err(|e| format!("write: {e:?}"))?;
    conn.initiate_response().map_err(|e| format!("submit: {e:?}"))?;
    let status = conn.status();
    // Drain any response body so the next request on the same task
    // doesn't see leftovers. 256 B is enough — we don't care what the
    // receiver said, only that it was 2xx.
    let mut sink = [0u8; 256];
    let _ = try_read_full(&mut conn, &mut sink);
    Ok(status)
}
