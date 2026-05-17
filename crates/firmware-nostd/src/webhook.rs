//! Outbound webhook delivery.
//!
//! `core::App` emits `WebhookEvent`s through the `WebhookDispatcher`
//! trait. Here the trait impl just `try_send`s into a bounded embassy
//! channel — non-blocking, so the producing task (tick loop, HTTP
//! handler, schedule engine) is never stalled by a slow endpoint.
//!
//! `webhook_task` drains that channel and does the actual HTTP work:
//! it reads the *current* `app.config().webhooks` on every event (no
//! caching, so config edits take effect immediately), filters for the
//! subscribers of that event, renders each body template, and POSTs.
//!
//! Plain `http://` is delivered over embassy-net TCP. `https://`
//! requires outbound TLS — see the `tls` feature in Cargo.toml. In a
//! build without it, an `https://` webhook is logged and skipped
//! rather than silently dropped.

use alloc::{string::String, vec::Vec};

use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, IpAddress, IpEndpoint, Stack};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Channel, Sender},
};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write as _;
#[cfg(feature = "tls")]
use embassy_sync::mutex::Mutex;
#[cfg(feature = "tls")]
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider};
use watercontroller_core::{
    app::App,
    webhook::{render_template, WebhookConfig, WebhookDispatcher, WebhookEvent},
};

/// Bounded queue. A burst of events (config-change spam, a schedule
/// firing several rules at once) must not grow heap without limit —
/// when full, `dispatch` drops with a log line instead of blocking.
const QUEUE_CAP: usize = 8;

static CHANNEL: Channel<CriticalSectionRawMutex, WebhookEvent, QUEUE_CAP> = Channel::new();

/// `WebhookDispatcher` impl handed to `App::set_webhook_dispatcher`.
/// Cheap to construct; just borrows the static channel's sender.
pub struct EmbassyWebhookDispatcher {
    tx: Sender<'static, CriticalSectionRawMutex, WebhookEvent, QUEUE_CAP>,
}

impl EmbassyWebhookDispatcher {
    pub fn new() -> Self {
        Self { tx: CHANNEL.sender() }
    }
}

impl WebhookDispatcher for EmbassyWebhookDispatcher {
    fn dispatch(&self, event: WebhookEvent) {
        if self.tx.try_send(event).is_err() {
            log::info!("webhook: queue full, dropping event");
        }
    }
}

#[embassy_executor::task]
pub async fn webhook_task(app: App, stack: Stack<'static>) {
    let rx = CHANNEL.receiver();
    loop {
        let event = rx.receive().await;
        handle_event(&app, stack, event).await;
    }
}

async fn handle_event(app: &App, stack: Stack<'static>, event: WebhookEvent) {
    let kind = event.kind;
    // Snapshot the matching subscribers so the Arc<Config> borrow isn't
    // held across slow HTTP I/O.
    let subs: Vec<WebhookConfig> = {
        let cfg = app.config();
        cfg.webhooks
            .iter()
            .filter(|w| w.enabled && w.events.iter().any(|e| *e == kind))
            .cloned()
            .collect()
    };
    if subs.is_empty() {
        return;
    }
    log::info!(
        "webhook: dispatching {} to {} subscriber(s)",
        kind.as_str(),
        subs.len()
    );
    for wh in &subs {
        let body = render_template(&wh.body_template, &event.vars);
        match deliver(stack, wh, &body).await {
            Ok(status) if (200..300).contains(&status) => {
                log::info!("webhook: {} -> {} OK ({})", kind.as_str(), wh.url, status)
            }
            Ok(status) => {
                log::info!("webhook: {} -> {} HTTP {}", kind.as_str(), wh.url, status)
            }
            Err(e) => log::info!("webhook: {} -> {} failed: {}", kind.as_str(), wh.url, e),
        }
    }
}

/// Parsed pieces of a webhook URL.
struct Url<'a> {
    https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Split `http(s)://host[:port][/path]` into its parts. Returns None on
/// any URL we can't route (unknown scheme, empty host).
fn parse_url(url: &str) -> Option<Url<'_>> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some(Url { https, host, port, path })
}

/// Resolve a host that's either a dotted-IPv4 literal or a DNS name.
async fn resolve(stack: Stack<'static>, host: &str) -> Option<IpAddress> {
    if let Some(ip) = parse_ipv4(host) {
        return Some(ip);
    }
    stack
        .dns_query(host, DnsQueryType::A)
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
}

fn parse_ipv4(s: &str) -> Option<IpAddress> {
    let mut octets = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 {
            return None;
        }
        octets[n] = part.parse().ok()?;
        n += 1;
    }
    if n != 4 {
        return None;
    }
    Some(IpAddress::v4(octets[0], octets[1], octets[2], octets[3]))
}

/// Connect, send the request, return the HTTP status code.
async fn deliver(
    stack: Stack<'static>,
    wh: &WebhookConfig,
    body: &str,
) -> Result<u16, &'static str> {
    let url = parse_url(&wh.url).ok_or("bad url")?;
    if url.https {
        #[cfg(feature = "tls")]
        return deliver_https(stack, wh, &url, body).await;
        // Without the `tls` feature, outbound TLS isn't compiled in —
        // skipping is clearer than a bare connection error on :443.
        #[cfg(not(feature = "tls"))]
        return Err("https requires the `tls` feature");
    }

    let ip = resolve(stack, url.host).await.ok_or("dns")?;

    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(8)));
    socket
        .connect(IpEndpoint::new(ip, url.port))
        .await
        .map_err(|_| "connect")?;

    let request = build_request(wh, &url, body);
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "write")?;
    socket.flush().await.map_err(|_| "flush")?;

    read_status(&mut socket).await
}

// ---- HTTPS (outbound TLS 1.3 client) ---------------------------------
//
// `embedded-tls` — pure Rust, no C. The record buffers are a single
// static guarded by `TLS_BUFS`, so only one webhook TLS session is ever
// live and the ~18 KiB of buffers sit in `.bss`, never the heap: a
// handshake allocates almost nothing and can't fragment the heap.

/// The ESP32 hardware RNG wrapped to satisfy `rand_core` 0.6 (the
/// version embedded-tls 0.18 expects) for the TLS handshake.
#[cfg(feature = "tls")]
struct EspRng(esp_hal::rng::Rng);

#[cfg(feature = "tls")]
impl rand_core::RngCore for EspRng {
    fn next_u32(&mut self) -> u32 {
        self.0.random()
    }
    fn next_u64(&mut self) -> u64 {
        (u64::from(self.0.random()) << 32) | u64::from(self.0.random())
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let word = self.0.random().to_ne_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
#[cfg(feature = "tls")]
impl rand_core::CryptoRng for EspRng {}

/// TLS record buffers. `read` must hold one whole encrypted record — a
/// server's Certificate handshake record can approach the 16 640-byte
/// TLS maximum, and embedded-tls can't skip bytes it can't buffer.
/// `write` only carries our own (small) request records.
#[cfg(feature = "tls")]
struct TlsBufs {
    read: [u8; 16640],
    write: [u8; 2048],
}

/// One static set of TLS buffers; the `Mutex` doubles as the
/// single-flight guard — one webhook TLS handshake at a time.
#[cfg(feature = "tls")]
static TLS_BUFS: Mutex<CriticalSectionRawMutex, TlsBufs> =
    Mutex::new(TlsBufs { read: [0; 16640], write: [0; 2048] });

/// Deliver an `https://` webhook over TLS 1.3.
#[cfg(feature = "tls")]
async fn deliver_https(
    stack: Stack<'static>,
    wh: &WebhookConfig,
    url: &Url<'_>,
    body: &str,
) -> Result<u16, &'static str> {
    let ip = resolve(stack, url.host).await.ok_or("dns")?;

    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    // The handshake runs ECDHE/ECDSA in software on the Xtensa core —
    // seconds, not milliseconds — so allow well beyond the HTTP 8 s.
    socket.set_timeout(Some(Duration::from_secs(20)));
    socket
        .connect(IpEndpoint::new(ip, url.port))
        .await
        .map_err(|_| "connect")?;

    let mut bufs = TLS_BUFS.lock().await;
    let TlsBufs { read, write } = &mut *bufs;
    let mut tls: TlsConnection<_, Aes128GcmSha256> = TlsConnection::new(socket, read, write);

    let config = TlsConfig::new().with_server_name(url.host);
    // PROTOTYPE: `UnsecureProvider` performs NO certificate verification.
    // Fine for measuring handshake RAM/stability; must be replaced with
    // a verifying provider before this is production-trusted.
    tls.open(TlsContext::new(
        &config,
        UnsecureProvider::new::<Aes128GcmSha256>(EspRng(esp_hal::rng::Rng::new())),
    ))
    .await
    .map_err(|e| {
        log::warn!("webhook: tls handshake failed: {e:?}");
        "tls handshake"
    })?;

    let request = build_request(wh, url, body);
    tls.write_all(request.as_bytes())
        .await
        .map_err(|_| "tls write")?;
    tls.flush().await.map_err(|_| "tls flush")?;

    let status = read_status(&mut tls).await;
    let _ = tls.close().await;
    status
}

/// Build a complete HTTP/1.1 request. `Content-Type` defaults to JSON
/// when the user didn't set it; `Connection: close` lets the server
/// signal end-of-response by closing.
fn build_request(wh: &WebhookConfig, url: &Url<'_>, body: &str) -> String {
    let method = if wh.method.eq_ignore_ascii_case("put") {
        "PUT"
    } else {
        "POST"
    };
    let mut req = String::with_capacity(256 + body.len());
    req.push_str(method);
    req.push(' ');
    req.push_str(url.path);
    req.push_str(" HTTP/1.1\r\nHost: ");
    req.push_str(url.host);
    req.push_str("\r\n");

    let mut has_content_type = false;
    for h in &wh.headers {
        if h.name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        req.push_str(&h.name);
        req.push_str(": ");
        req.push_str(&h.value);
        req.push_str("\r\n");
    }
    if !has_content_type {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("Content-Length: ");
    let mut len_buf: heapless::String<10> = heapless::String::new();
    let _ = core::fmt::Write::write_fmt(&mut len_buf, format_args!("{}", body.len()));
    req.push_str(&len_buf);
    req.push_str("\r\nConnection: close\r\n\r\n");
    req.push_str(body);
    req
}

/// Read just enough of the response to parse the status line
/// (`HTTP/1.1 NNN ...`). The body is ignored — we only report 2xx/non.
async fn read_status<R: embedded_io_async::Read>(r: &mut R) -> Result<u16, &'static str> {
    let mut buf = [0u8; 64];
    let recv = embassy_futures::select::select(
        r.read(&mut buf),
        Timer::after(Duration::from_secs(8)),
    )
    .await;
    let n = match recv {
        embassy_futures::select::Either::First(r) => r.map_err(|_| "read")?,
        embassy_futures::select::Either::Second(()) => return Err("timeout"),
    };
    // "HTTP/1.1 200 ..." — the code is the second space-delimited token.
    let line = core::str::from_utf8(&buf[..n]).map_err(|_| "non-utf8")?;
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("no status")
}
