//! Watercontroller no_std firmware.
//!
//! HTTP server on :80 via picoserve, serving the embedded SPA and the
//! JSON API. WiFi STA + DHCP via esp-radio + embassy-net. Domain logic
//! (state, schedule, switches, valve) comes from `watercontroller-core`.
//! Config + valve state persist across reboot in a flash-backed KV
//! store (see `nvs.rs`).
//!
//! No HTTPS server (the ESP32-PICO-D4 can't host mbedtls server-side —
//! see README "no_std migration"). Outbound client TLS for webhooks is
//! behind the `tls` feature.
//!
//! Single-core only by choice: esp-radio has an open bug
//! (esp-rs/esp-wifi-sys#412) where an embassy task on the second core
//! can corrupt WiFi state.

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
// picoserve's router builds a deeply-nested generic type; the default
// limit overflows well before all the API routes are chained.
#![recursion_limit = "512"]

extern crate alloc;

mod ap;
mod logbuf;
mod mdns;
mod mqtt;
mod nvs;
mod ota;
mod schedule;
mod sensors;
mod serial;
mod sntp;
mod telnet;
mod webapi;
mod webhook;
mod wifiscan;

use alloc::{string::String, sync::Arc};

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Level, Output, OutputConfig},
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{
    sta::StationConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use esp_storage::FlashStorage;
use picoserve::{
    extract::State,
    io::Write,
    response::{Content, File},
    routing::{get, get_service, parse_path_segment, post},
    AppRouter, AppWithStateBuilder,
};
use watercontroller_core::{
    app::App,
    config::Config,
    traits::{Clock, NvsStore, WifiState},
};

use crate::nvs::FlashKv;

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// Credentials come from the workspace-root `.env` via build.rs — never
// hard-coded here. `.env` is gitignored.
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

const SPA_HTML: &str = include_str!("../../firmware/assets/index.html");

/// Epoch baseline for `Clock::now()` before SNTP completes its first
/// sync. Once `sntp` publishes a real offset this is unused.
const BASE_EPOCH_S: i64 = 1_778_803_200; // 2026-05-15T00:00:00Z

static mut BOOT_INSTANT: Option<Instant> = None;

/// Latest WiFi RSSI sample (dBm), taken every ~30 s by `connection_task`.
/// 0 = unknown (no link, or not sampled yet) — a real RSSI is negative.
static WIFI_RSSI: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

/// SSID of the network `connection_task` is currently joined to. The
/// heartbeat reports this so the dashboard shows the real AP rather
/// than always the first configured network. Empty until first connect.
static CONNECTED_SSID: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<33>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

pub(crate) fn uptime_secs() -> u64 {
    unsafe { BOOT_INSTANT }
        .map(|t| (Instant::now() - t).as_secs())
        .unwrap_or(0)
}

/// Human-readable reason for the last reset, for `/api/diag`.
fn reset_reason_str() -> String {
    match esp_hal::system::reset_reason() {
        Some(r) => alloc::format!("{:?}", r),
        None => String::from("unknown"),
    }
}

/// Which radio mode the firmware should bring up. Decided once at boot;
/// switching between them is a reboot (the network stack binds to one
/// interface for its lifetime).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BootMode {
    /// Join a configured WiFi network (normal operation).
    Sta,
    /// Run the SoftAP setup portal (no network reachable / unconfigured).
    Ap,
}

/// NVS key holding the persisted boot-mode hint (1 byte: 0 = STA, 1 = AP).
const BOOT_MODE_KEY: &str = "wc.bootmode";

/// Read the persisted boot-mode hint. Absent / unrecognised → STA, the
/// safe default (after a power cycle you want to try real WiFi first).
fn read_boot_mode(nvs: &dyn NvsStore) -> BootMode {
    match nvs.get(BOOT_MODE_KEY).as_deref() {
        Some([1, ..]) => BootMode::Ap,
        _ => BootMode::Sta,
    }
}

/// Persist the boot-mode hint — but only when it actually changes, so a
/// steady device never writes flash. `connection_task` flips it to AP
/// when WiFi is unreachable; `ap::scan_task` flips it back to STA when a
/// known network returns. Each flip is followed by a reboot.
fn write_boot_mode(nvs: &dyn NvsStore, mode: BootMode) {
    if read_boot_mode(nvs) == mode {
        return;
    }
    let byte: u8 = match mode {
        BootMode::Sta => 0,
        BootMode::Ap => 1,
    };
    if let Err(e) = nvs.set(BOOT_MODE_KEY, &[byte]) {
        log::info!("bootmode: persist failed: {:?}", e);
    }
}

/// Build the SoftAP radio config from the persisted WiFi settings.
/// Open network when no AP password is set (matches the YAML default).
fn build_ap_config(
    wifi: &watercontroller_core::config::WifiConfig,
) -> esp_radio::wifi::ap::AccessPointConfig {
    use esp_radio::wifi::{ap::AccessPointConfig, AuthenticationMethod};
    let ap = AccessPointConfig::default().with_ssid(wifi.ap_ssid.as_str());
    if wifi.ap_password.is_empty() {
        ap
    } else {
        ap.with_password(wifi.ap_password.clone())
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
    }
}

struct EmbassyClock;
impl Clock for EmbassyClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        // Prefer the SNTP-derived epoch-at-boot; fall back to the
        // compile-time baseline until the first sync lands.
        let at_boot = sntp::EPOCH_AT_BOOT.load(core::sync::atomic::Ordering::Relaxed);
        let base = if at_boot != 0 { at_boot } else { BASE_EPOCH_S };
        chrono::DateTime::from_timestamp(base + uptime_secs() as i64, 0)
            .unwrap_or_default()
    }
    fn monotonic_ms(&self) -> u64 {
        Instant::now().as_millis()
    }
}

/// Pre-serialized JSON body (alloc-backed; no serde-json-core buffer
/// limits — important for the large `Config`).
struct JsonStr(String);
impl Content for JsonStr {
    fn content_type(&self) -> &'static str {
        "application/json"
    }
    fn content_length(&self) -> usize {
        self.0.len()
    }
    async fn write_content<W: Write>(self, mut w: W) -> Result<(), W::Error> {
        w.write_all(self.0.as_bytes()).await
    }
}

/// Handler response that is either a JSON body (200) or an empty
/// `204 No Content` — the latter for action endpoints (`alarm/clear`,
/// `wifi/reconnect`) where the SPA + test suite expect 204.
pub(crate) enum ApiResp {
    Json(String),
    NoContent,
}

impl picoserve::response::IntoResponse for ApiResp {
    async fn write_to<R: picoserve::io::Read, W: picoserve::response::ResponseWriter<Error = R::Error>>(
        self,
        connection: picoserve::response::Connection<'_, R>,
        response_writer: W,
    ) -> Result<picoserve::ResponseSent, W::Error> {
        match self {
            ApiResp::Json(s) => JsonStr(s).write_to(connection, response_writer).await,
            ApiResp::NoContent => {
                (picoserve::response::StatusCode::NO_CONTENT, picoserve::response::NoContent)
                    .write_to(connection, response_writer)
                    .await
            }
        }
    }
}

/// Router state: the domain App, a handle to the NVS store (so the
/// config-write path can persist), and the concrete `FlashKv` (the OTA
/// handler borrows its raw flash). All cheap to clone (Arc).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) app: App,
    pub(crate) nvs: Arc<dyn NvsStore>,
    pub(crate) flash_kv: Arc<FlashKv>,
}

/// WebSocket callback for `/ws/logs` — streams every log line from the
/// `logbuf` channel to the client. A client disconnect surfaces as a
/// send error, which ends the loop.
struct LogStreamer;

impl picoserve::response::ws::WebSocketCallback for LogStreamer {
    async fn run<R: picoserve::io::Read, W: picoserve::io::Write<Error = R::Error>>(
        self,
        mut rx: picoserve::response::ws::SocketRx<R>,
        mut tx: picoserve::response::ws::SocketTx<W>,
    ) -> Result<(), W::Error> {
        use picoserve::futures::Either;
        let mut sub = match logbuf::subscriber() {
            // All subscriber slots taken — close rather than block.
            None => return Ok(()),
            Some(s) => s,
        };
        // `next_message` races the client read against the next log
        // line, so a client disconnect is detected at once — the web
        // worker frees immediately instead of staying parked until the
        // next log line fails to send. The SPA only ever sends a Close.
        let mut rx_buf = [0u8; 128];
        loop {
            match rx.next_message(&mut rx_buf, sub.next_message_pure()).await {
                // Inbound frame (Close) or read error — end the stream.
                Ok(Either::First(_)) | Err(_) => return Ok(()),
                // A log line is ready — forward it.
                Ok(Either::Second(line)) => tx.send_text(&line).await?,
            }
        }
    }
}

struct AppProps;

/// Path-segment capture type for the generic `/api` routes.
type Seg = heapless::String<24>;

/// Query-string flags. `GET /api/config?all=1` opts into the full
/// (un-redacted) config for the SPA's backup download.
#[derive(serde::Deserialize)]
struct ApiQuery {
    #[serde(default)]
    all: Option<String>,
}

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl picoserve::routing::PathRouter<AppState>;

    /// Seven routes. picoserve's `call_path_router` recurses once per
    /// route with the full nested router type in every frame, so a
    /// route per endpoint overflows the executor poll stack — instead
    /// the `/api` surface funnels through prefix routes that capture a
    /// trailing segment and let `webapi` dispatch. picoserve only
    /// supports one path parameter alongside `State`/body extractors,
    /// hence one prefix route per two-level path.
    fn build_app(self) -> picoserve::Router<Self::PathRouter, AppState> {
        picoserve::Router::new()
            .route("/", get_service(File::html(SPA_HTML)))
            .route(
                "/ws/logs",
                get(|upgrade: picoserve::response::ws::WebSocketUpgrade| async move {
                    upgrade.on_upgrade(LogStreamer)
                }),
            )
            .route(
                "/api/ota",
                post(|report: ota::OtaReport| async move {
                    if report.ok {
                        JsonStr(alloc::format!(
                            r#"{{"result":"ok","detail":"{}"}}"#, report.detail
                        ))
                    } else {
                        JsonStr(alloc::format!(r#"{{"error":"{}"}}"#, report.detail))
                    }
                }),
            )
            .route(
                ("/api", parse_path_segment::<Seg>()),
                get(|seg: Seg, State(st): State<AppState>, q: picoserve::extract::Query<ApiQuery>| async move {
                    JsonStr(webapi::api_get(&seg, &st, q.0.all.is_some()))
                })
                .put(|seg: Seg, State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    JsonStr(webapi::api_put(&seg, &st, &body))
                })
                .post(|seg: Seg, State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    JsonStr(webapi::api_post(&seg, &st, &body))
                }),
            )
            .route(
                ("/api/config", parse_path_segment::<Seg>()),
                get(|sec: Seg, State(st): State<AppState>| async move {
                    JsonStr(webapi::config_section_get(&sec, &st))
                })
                .put(|sec: Seg, State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    JsonStr(webapi::config_section_put(&sec, &st, &body))
                }),
            )
            .route(
                ("/api/wifi", parse_path_segment::<Seg>()),
                get(|act: Seg| async move {
                    JsonStr(if act.as_str() == "scan" {
                        wifiscan::request_scan().await
                    } else {
                        webapi::wifi_get(&act)
                    })
                })
                .post(|act: Seg| async move { webapi::wifi_post(&act) }),
            )
            .route(
                ("/api/alarm", parse_path_segment::<Seg>()),
                post(|act: Seg, State(st): State<AppState>| async move {
                    webapi::alarm_post(&act, &st)
                }),
            )
            .route(
                ("/api/webhooks", parse_path_segment::<Seg>()),
                post(|act: Seg, State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    JsonStr(webapi::webhooks_post(&act, &st, &body))
                }),
            )
    }
}

// `read_request` is bumped well above the 3 s default: the OTA handler
// interleaves ~40 ms flash-sector erase/writes with body reads, which
// blocks the single-threaded executor in bursts. 30 s comfortably
// absorbs that without the connection being aborted mid-upload.
static SERVER_CONFIG: picoserve::Config = picoserve::Config::new(picoserve::Timeouts {
    start_read_request: Duration::from_secs(5),
    persistent_start_read_request: Duration::from_secs(1),
    read_request: Duration::from_secs(30),
    write: Duration::from_secs(10),
})
.keep_connection_alive();

// Each `web_task` future is ~19 KiB of static RAM, so the pool is kept
// lean. The SPA is tuned to match: it polls lightly and only holds the
// `/ws/logs` socket open while the Logs tab is in view. 3 is the
// minimum that reliably serves one browser — a browser opens a few
// parallel connections per page load, so 2 isn't enough. Down from 4
// (the freed DRAM leaves headroom for future features).
const WEB_TASK_POOL_SIZE: usize = 3;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    task_id: usize,
    stack: Stack<'static>,
    router: &'static AppRouter<AppProps>,
    state: &'static AppState,
) -> ! {
    // RX is generous (4 KiB) so a streamed OTA upload keeps a healthy
    // TCP window open across the handler's flash-write stalls.
    let mut tcp_rx = [0u8; 4096];
    let mut tcp_tx = [0u8; 1024];
    let mut http_buf = [0u8; 2048];
    picoserve::Server::new(&router.shared().with_state(state), &SERVER_CONFIG, &mut http_buf)
        .listen_and_serve(task_id, stack, 80, &mut tcp_rx, &mut tcp_tx)
        .await
        .into_never()
}

/// The five actuator GPIOs. Pin map matches the ESPHome reference +
/// the IDF firmware: sprinkler 1 = GPIO12, sprinkler 2 = GPIO4,
/// valve OPEN coil = GPIO26, valve CLOSE coil = GPIO27, drain = GPIO25.
struct ValvePins {
    sprinkler1: Output<'static>,
    sprinkler2: Output<'static>,
    valve_open: Output<'static>,
    valve_close: Output<'static>,
    drain: Output<'static>,
}

fn level(on: bool) -> Level {
    if on {
        Level::High
    } else {
        Level::Low
    }
}

/// 1 Hz App tick — runs the switch auto-off timers + valve sequencer
/// and applies the resulting actuator states to the GPIOs. This is the
/// firmware's actual control loop.
#[embassy_executor::task]
async fn tick_task(app: App, mut pins: ValvePins) {
    loop {
        let out = app.tick();
        pins.sprinkler1.set_level(level(out.sprinkler_1));
        pins.sprinkler2.set_level(level(out.sprinkler_2));
        pins.valve_open.set_level(level(out.valve.open_motor));
        pins.valve_close.set_level(level(out.valve.close_motor));
        pins.drain.set_level(level(out.valve.drain));
        Timer::after(Duration::from_secs(1)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    logbuf::init();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));

    // The reclaimed-RAM pool (64 KiB) sits in a separate segment; the
    // regular pool shares the RWDATA segment with the executor stack,
    // so it's kept small — 16 KiB — to leave the stack room. picoserve
    // polls its deeply-nested router on that stack and a route per
    // endpoint plus a serde_json `Config` deserialize would otherwise
    // trip the stack guard.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 16 * 1024);

    unsafe { BOOT_INSTANT = Some(Instant::now()); }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    log::info!("wc-nostd: boot");

    // Flash-backed NVS. Config is restored from it (or compile-time
    // defaults on a blank store / parse failure). The same `FlashKv`
    // also lends its raw flash to the OTA writer.
    let flash = FlashStorage::new(peripherals.FLASH);
    let flash_kv = Arc::new(FlashKv::new(flash));
    let nvs: Arc<dyn NvsStore> = flash_kv.clone();
    // Confirm the running image so a rollback bootloader keeps it (and
    // so a just-OTA'd slot is marked Valid).
    ota::confirm_running(&flash_kv);
    let config = Config::load(&*nvs).unwrap_or_else(|_| {
        log::info!("nvs: no stored config, using defaults");
        Config::default()
    });

    let clock: Arc<dyn Clock> = Arc::new(EmbassyClock);
    let app = App::with_nvs(clock, config, Some(nvs.clone()));
    app.set_webhook_dispatcher(Arc::new(webhook::EmbassyWebhookDispatcher::new()));

    // Actuator GPIOs — all start LOW (everything off / coils idle).
    let oc = OutputConfig::default();
    let valve_pins = ValvePins {
        sprinkler1: Output::new(peripherals.GPIO12, Level::Low, oc),
        sprinkler2: Output::new(peripherals.GPIO4, Level::Low, oc),
        valve_open: Output::new(peripherals.GPIO26, Level::Low, oc),
        valve_close: Output::new(peripherals.GPIO27, Level::Low, oc),
        drain: Output::new(peripherals.GPIO25, Level::Low, oc),
    };

    // Analog sensors: ADC1 on GPIO36 (battery) + GPIO32 (pressure).
    let analog = sensors::Analog::new(peripherals.ADC1, peripherals.GPIO36, peripherals.GPIO32);

    let state: &'static AppState =
        mk_static!(AppState, AppState { app: app.clone(), nvs, flash_kv });

    // Decide the radio mode for this boot: station to join a configured
    // network, or the SoftAP setup portal when there's nothing to join
    // or a prior STA failure persisted the hint. Switching modes is a
    // reboot — the network stack binds one interface for its lifetime,
    // and keeping only one alive avoids the second-stack RAM cost.
    let cfg = app.config();
    let boot_ap = cfg.wifi.networks.is_empty()
        || read_boot_mode(&*state.nvs) == BootMode::Ap;

    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(WifiConfig::Station(
            StationConfig::default().with_ssid(SSID).with_password(PASSWORD.into()),
        )),
    ).unwrap();

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let resources = mk_static!(StackResources<16>, StackResources::<16>::new());

    let (stack, runner) = if boot_ap {
        log::info!("wc-nostd: AP setup mode — SoftAP '{}'", cfg.wifi.ap_ssid);
        let ap_net = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(
                embassy_net::Ipv4Address::new(
                    ap::AP_IP[0], ap::AP_IP[1], ap::AP_IP[2], ap::AP_IP[3],
                ),
                24,
            ),
            gateway: None,
            dns_servers: Default::default(),
        });
        embassy_net::new(interfaces.access_point, ap_net, resources, seed)
    } else {
        log::info!("wc-nostd: station mode");
        embassy_net::new(
            interfaces.station,
            embassy_net::Config::dhcpv4(Default::default()),
            resources,
            seed,
        )
    };

    let router = mk_static!(AppRouter<AppProps>, AppProps.build_app());

    // Network-stack-agnostic tasks — identical in either mode.
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(heartbeat(app.clone(), stack).unwrap());
    spawner.spawn(tick_task(app.clone(), valve_pins).unwrap());
    spawner.spawn(ota::reboot_task().unwrap());
    spawner.spawn(
        sensors::sensor_task(app.clone(), analog, peripherals.PCNT, peripherals.GPIO33).unwrap(),
    );
    spawner.spawn(
        serial::serial_task(
            app.clone(),
            state.nvs.clone(),
            peripherals.UART0,
            peripherals.GPIO3,
        )
        .unwrap(),
    );
    spawner.spawn(schedule::schedule_task(app.clone()).unwrap());
    spawner.spawn(telnet::telnet_task(stack).unwrap());
    spawner.spawn(mdns::mdns_task(stack, app.clone()).unwrap());

    if boot_ap {
        // SoftAP services: DHCP + captive DNS, plus a scanner that
        // reboots into STA the moment a configured network reappears.
        let ap_cfg = build_ap_config(&cfg.wifi);
        spawner.spawn(ap::dhcp_server_task(stack).unwrap());
        spawner.spawn(ap::dns_server_task(stack).unwrap());
        spawner.spawn(
            ap::scan_task(controller, app.clone(), state.nvs.clone(), ap_cfg).unwrap(),
        );
    } else {
        // Station services: multi-SSID connect + the internet-facing tasks.
        spawner.spawn(connection_task(controller, app.clone(), state.nvs.clone()).unwrap());
        spawner.spawn(mqtt::mqtt_task(app.clone(), stack).unwrap());
        spawner.spawn(sntp::sntp_task(stack).unwrap());
        spawner.spawn(webhook::webhook_task(app.clone(), stack).unwrap());
    }
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(id, stack, router, state).unwrap());
    }

    log::info!("wc-nostd: listening on :80 (SPA + /api/*)");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn connection_task(
    mut controller: WifiController<'static>,
    app: App,
    nvs: Arc<dyn NvsStore>,
) {
    use embassy_futures::select::{select3, Either3};
    log::info!("wifi: connection task started");
    // Round-robin over the configured networks: each failed attempt
    // advances to the next, so a dead/renamed AP doesn't wedge us.
    let mut idx = 0usize;
    // Consecutive failed attempts. After ~3 full passes over every
    // network with no success, persist an AP hint and reboot into the
    // SoftAP setup portal — `ap::scan_task` reboots back to STA once a
    // configured network is in range again.
    let mut fails = 0u32;
    loop {
        let networks = app.config().wifi.networks.clone();
        if networks.is_empty() {
            log::warn!("wifi: no networks configured");
            Timer::after(Duration::from_secs(15)).await;
            continue;
        }
        if fails >= networks.len() as u32 * 3 {
            log::warn!("wifi: all networks unreachable — falling back to AP mode");
            write_boot_mode(&*nvs, BootMode::Ap);
            ota::request_reboot();
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }
        idx %= networks.len();
        let net = networks[idx].clone();
        let sta = WifiConfig::Station(
            StationConfig::default()
                .with_ssid(net.ssid.as_str())
                .with_password(net.password.as_str().into()),
        );
        if let Err(e) = controller.set_config(&sta) {
            log::info!("wifi: set_config '{}' failed: {:?}", net.ssid, e);
            idx += 1;
            fails += 1;
            Timer::after(Duration::from_secs(3)).await;
            continue;
        }
        log::info!(
            "wifi: connecting to '{}' ({}/{})",
            net.ssid,
            idx + 1,
            networks.len(),
        );
        match controller.connect_async().await {
            Ok(info) => {
                log::info!("wifi: connected: {:?}", info);
                fails = 0;
                write_boot_mode(&*nvs, BootMode::Sta);
                {
                    let mut g = CONNECTED_SSID.lock().await;
                    g.clear();
                    let _ = g.push_str(&net.ssid);
                }
                // Stay connected; serve scan requests in between. The
                // disconnect wait only borrows `&controller`, so it
                // composes with the scan-request signal — and once the
                // signal wins, that borrow is released for the `&mut`
                // `scan_async` call.
                loop {
                    match select3(
                        controller.wait_for_disconnect_async(),
                        wifiscan::SCAN_REQ.wait(),
                        Timer::after(Duration::from_secs(30)),
                    )
                    .await
                    {
                        Either3::First(why) => {
                            log::info!("wifi: disconnected: {:?}", why.ok());
                            WIFI_RSSI.store(0, core::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        Either3::Third(()) => {
                            // Periodic link-quality sample for /api/diag.
                            if let Ok(r) = controller.rssi() {
                                WIFI_RSSI.store(r, core::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        Either3::Second(()) => {
                            let results = match controller
                                .scan_async(&esp_radio::wifi::scan::ScanConfig::default())
                                .await
                            {
                                Ok(aps) => aps.iter().map(ap_to_result).collect(),
                                Err(e) => {
                                    log::info!("wifi: scan failed: {:?}", e);
                                    alloc::vec::Vec::new()
                                }
                            };
                            log::info!("wifi: scan found {} AP(s)", results.len());
                            wifiscan::SCAN_RESULT.signal(results);
                        }
                    }
                }
            }
            Err(e) => {
                log::info!("wifi: connect to '{}' failed: {:?}", net.ssid, e);
                idx += 1;
                fails += 1;
            }
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

/// Convert an esp-radio scan entry into the SPA's `WifiScanResult`.
fn ap_to_result(
    ap: &esp_radio::wifi::ap::AccessPointInfo,
) -> watercontroller_core::api::WifiScanResult {
    use esp_radio::wifi::AuthenticationMethod as A;
    let auth = match ap.auth_method {
        None | Some(A::None) => "open",
        Some(A::Wep) => "wep",
        Some(A::Wpa) => "wpa",
        Some(A::Wpa2Personal) | Some(A::WpaWpa2Personal) | Some(A::Wpa2Enterprise) => "wpa2",
        Some(_) => "wpa3",
    };
    watercontroller_core::api::WifiScanResult {
        ssid: String::from(ap.ssid.as_str()),
        rssi_dbm: ap.signal_strength,
        auth: String::from(auth),
        channel: ap.channel,
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) { runner.run().await }

/// Serial heartbeat + the periodic refresh of the device-state snapshot
/// that `/api/status` (and thus the SPA dashboard) reads — uptime, fw
/// version, free heap, WiFi link, MQTT link. Sensors/switches/alarm are
/// kept current by `App` itself.
#[embassy_executor::task]
async fn heartbeat(app: App, stack: Stack<'static>) {
    let mut secs = 0u64;
    let reset_reason = reset_reason_str();
    let mut min_free = usize::MAX;
    loop {
        let free = esp_alloc::HEAP.free();
        let used = esp_alloc::HEAP.used();
        min_free = min_free.min(free);
        let conn_ssid = {
            let g = CONNECTED_SSID.lock().await;
            if g.is_empty() {
                String::from(SSID)
            } else {
                String::from(g.as_str())
            }
        };
        let wifi = match stack.config_v4() {
            Some(c) => {
                log::info!(
                    "alive uptime={}s heap_free={} heap_used={} ip={}",
                    secs, free, used, c.address.address()
                );
                Some(WifiState::Connected {
                    ssid: conn_ssid,
                    ip: alloc::format!("{}", c.address.address()),
                })
            }
            None => {
                log::info!("alive uptime={}s heap_free={} heap_used={} ip=<none>", secs, free, used);
                Some(WifiState::Disconnected)
            }
        };
        let rssi = match WIFI_RSSI.load(core::sync::atomic::Ordering::Relaxed) {
            0 => None,
            v => Some(v as i8),
        };
        app.update_state(|s| {
            s.uptime_ms = secs * 1000;
            s.firmware_version = String::from("wc-nostd");
            s.diagnostics.free_heap_bytes = Some(free as u32);
            s.diagnostics.min_free_heap_bytes = Some(min_free as u32);
            s.diagnostics.reset_reason = Some(reset_reason.clone());
            s.network.wifi = wifi;
            s.network.wifi_rssi_dbm = rssi;
            s.network.mqtt_connected =
                mqtt::MQTT_UP.load(core::sync::atomic::Ordering::Relaxed);
        });
        Timer::after(Duration::from_secs(10)).await;
        secs += 10;
    }
}
