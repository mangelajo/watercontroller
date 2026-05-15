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
// limit overflows once we chain GET+PUT on a route.
#![recursion_limit = "256"]

extern crate alloc;

mod mqtt;
mod nvs;
mod sntp;
mod webhook;

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
use esp_println::println;
use esp_radio::wifi::{
    sta::StationConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use esp_storage::FlashStorage;
use picoserve::{
    extract::State,
    io::Write,
    response::{Content, File},
    routing::{get, get_service, post},
    AppRouter, AppWithStateBuilder,
};
use watercontroller_core::{
    api::{ConfigResponse, StatusResponse, SwitchCommand},
    app::App,
    config::Config,
    traits::{Clock, NvsStore},
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

pub(crate) fn uptime_secs() -> u64 {
    unsafe { BOOT_INSTANT }
        .map(|t| (Instant::now() - t).as_secs())
        .unwrap_or(0)
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

/// Router state: the domain App + a handle to the NVS store (so the
/// config-write path can persist). Both are cheap to clone (Arc).
#[derive(Clone)]
struct AppState {
    app: App,
    nvs: Arc<dyn NvsStore>,
}

fn diag_json() -> JsonStr {
    JsonStr(alloc::format!(
        r#"{{"uptime_s":{},"heap":{{"total_free_bytes":{},"total_used_bytes":{}}},"fw":"wc-nostd"}}"#,
        uptime_secs(),
        esp_alloc::HEAP.free(),
        esp_alloc::HEAP.used(),
    ))
}

struct AppProps;

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl picoserve::routing::PathRouter<AppState>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, AppState> {
        picoserve::Router::new()
            .route("/", get_service(File::html(SPA_HTML)))
            .route("/api/diag", get(|| async { diag_json() }))
            .route(
                "/api/status",
                get(|State(st): State<AppState>| async move {
                    let snap = st.app.snapshot();
                    JsonStr(serde_json::to_string(&StatusResponse { state: &snap }).unwrap_or_default())
                }),
            )
            .route(
                "/api/config",
                get(|State(st): State<AppState>| async move {
                    // Secrets (WiFi/MQTT passwords, TLS key, admin
                    // token) are redacted — the SPA never needs them
                    // back. The IDF firmware gated the full dump
                    // behind ?all=1; the no_std build just always
                    // redacts (the backup/restore flow can come later).
                    let cfg = st.app.config();
                    let safe = cfg.redact_secrets_for_api();
                    JsonStr(serde_json::to_string(&ConfigResponse { config: &safe }).unwrap_or_default())
                })
                .put(|State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    match serde_json::from_slice::<Config>(&body) {
                        Ok(cfg) => {
                            // Persist first, then apply in-RAM.
                            if let Err(e) = cfg.save(&*st.nvs) {
                                return JsonStr(alloc::format!(
                                    r#"{{"error":"nvs save failed: {:?}"}}"#, e
                                ));
                            }
                            st.app.replace_config(cfg);
                            JsonStr(String::from(r#"{"result":"ok"}"#))
                        }
                        Err(e) => JsonStr(alloc::format!(
                            r#"{{"error":"bad config json: {}"}}"#, e
                        )),
                    }
                }),
            )
            .route(
                "/api/switch",
                post(|State(st): State<AppState>, body: alloc::vec::Vec<u8>| async move {
                    match serde_json::from_slice::<SwitchCommand>(&body) {
                        Ok(cmd) => {
                            let outcome = st.app.switch_command(cmd);
                            JsonStr(serde_json::to_string(&outcome).unwrap_or_default())
                        }
                        Err(e) => JsonStr(alloc::format!(
                            r#"{{"error":"bad switch command: {}"}}"#, e
                        )),
                    }
                }),
            )
    }
}

static SERVER_CONFIG: picoserve::Config = picoserve::Config::const_default().keep_connection_alive();

const WEB_TASK_POOL_SIZE: usize = 4;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    task_id: usize,
    stack: Stack<'static>,
    router: &'static AppRouter<AppProps>,
    state: &'static AppState,
) -> ! {
    let mut tcp_rx = [0u8; 1024];
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
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    unsafe { BOOT_INSTANT = Some(Instant::now()); }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    println!("wc-nostd: boot");

    // Flash-backed NVS. Config is restored from it (or compile-time
    // defaults on a blank store / parse failure).
    let flash = FlashStorage::new(peripherals.FLASH);
    let nvs: Arc<dyn NvsStore> = Arc::new(FlashKv::new(flash));
    let config = Config::load(&*nvs).unwrap_or_else(|_| {
        println!("nvs: no stored config, using defaults");
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

    let state: &'static AppState = mk_static!(AppState, AppState { app: app.clone(), nvs });

    let station_cfg = WifiConfig::Station(
        StationConfig::default().with_ssid(SSID).with_password(PASSWORD.into()),
    );

    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_cfg),
    ).unwrap();
    let wifi_iface = interfaces.station;
    let net_cfg = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_iface,
        net_cfg,
        mk_static!(StackResources<8>, StackResources::<8>::new()),
        seed,
    );

    let router = mk_static!(AppRouter<AppProps>, AppProps.build_app());

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(heartbeat(stack).unwrap());
    spawner.spawn(tick_task(app.clone(), valve_pins).unwrap());
    spawner.spawn(mqtt::mqtt_task(app.clone(), stack).unwrap());
    spawner.spawn(sntp::sntp_task(stack).unwrap());
    spawner.spawn(webhook::webhook_task(app, stack).unwrap());
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(id, stack, router, state).unwrap());
    }

    println!("wc-nostd: listening on :80 (SPA + /api/*)");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    println!("wifi: connection task started");
    loop {
        match controller.connect_async().await {
            Ok(info) => {
                println!("wifi: connected: {:?}", info);
                let why = controller.wait_for_disconnect_async().await.ok();
                println!("wifi: disconnected: {:?}", why);
            }
            Err(e) => println!("wifi: connect failed: {:?}", e),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) { runner.run().await }

#[embassy_executor::task]
async fn heartbeat(stack: Stack<'static>) {
    let mut secs = 0u64;
    loop {
        let free = esp_alloc::HEAP.free();
        let used = esp_alloc::HEAP.used();
        match stack.config_v4() {
            Some(c) => println!(
                "alive uptime={}s heap_free={} heap_used={} ip={}",
                secs, free, used, c.address.address()
            ),
            None => println!("alive uptime={}s heap_free={} heap_used={} ip=<none>", secs, free, used),
        }
        Timer::after(Duration::from_secs(10)).await;
        secs += 10;
    }
}
