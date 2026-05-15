//! Watercontroller no_std firmware.
//!
//! HTTP server on :80 via picoserve, serving the embedded SPA and the
//! JSON API. WiFi STA + DHCP via esp-radio + embassy-net. Domain logic
//! (state, schedule, switches, valve) comes from `watercontroller-core`
//! — the same crate the IDF firmware and host build use.
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

extern crate alloc;

use alloc::string::String;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    sta::StationConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use picoserve::{
    io::Write,
    response::{Content, File},
    routing::{get, get_service},
    AppRouter, AppWithStateBuilder,
};
use watercontroller_core::{
    api::{ConfigResponse, StatusResponse},
    app::App,
    config::Config,
    traits::Clock,
};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = env!("WC_WIFI_SSID");
const PASSWORD: &str = env!("WC_WIFI_PASSWORD");

/// Embedded SPA — same asset the IDF firmware bundles.
const SPA_HTML: &str = include_str!("../../firmware/assets/index.html");

/// Epoch baseline for the `Clock::now()` wall-clock until SNTP lands
/// (N11). 2026-05-15T00:00:00Z. Schedule evaluation only needs
/// *relative* correctness within a day for the cron matcher, and the
/// firmware re-derives absolute time once SNTP syncs.
const BASE_EPOCH_S: i64 = 1_778_976_000;

static mut BOOT_INSTANT: Option<Instant> = None;

fn uptime_secs() -> u64 {
    unsafe { BOOT_INSTANT }
        .map(|t| (Instant::now() - t).as_secs())
        .unwrap_or(0)
}

/// `Clock` backed by embassy's monotonic timer. Wall clock is a fixed
/// baseline + uptime until SNTP; monotonic is exact.
struct EmbassyClock;
impl Clock for EmbassyClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(BASE_EPOCH_S + uptime_secs() as i64, 0)
            .unwrap_or_default()
    }
    fn monotonic_ms(&self) -> u64 {
        Instant::now().as_millis()
    }
}

/// A pre-serialized JSON body. We serialize with `serde_json` (alloc)
/// rather than picoserve's serde-json-core `Json` because the `Config`
/// struct is large + deeply nested and serde-json-core's fixed-buffer
/// serializer is easy to overflow. alloc-backed serialization has no
/// such limit.
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

/// `/api/diag` — heap + uptime. no_std has no per-task HWM table the
/// way FreeRTOS did, so this stays a lean object; the SPA's
/// Diagnostics tab degrades gracefully on the missing `tasks` array.
fn diag_json() -> JsonStr {
    let body = alloc::format!(
        r#"{{"uptime_s":{},"heap":{{"total_free_bytes":{},"total_used_bytes":{}}},"fw":"wc-nostd"}}"#,
        uptime_secs(),
        esp_alloc::HEAP.free(),
        esp_alloc::HEAP.used(),
    );
    JsonStr(body)
}

struct AppProps;

impl AppWithStateBuilder for AppProps {
    type State = App;
    type PathRouter = impl picoserve::routing::PathRouter<App>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, App> {
        picoserve::Router::new()
            .route("/", get_service(File::html(SPA_HTML)))
            .route("/api/diag", get(|| async { diag_json() }))
            .route(
                "/api/status",
                get(|picoserve::extract::State(app): picoserve::extract::State<App>| async move {
                    let snap = app.snapshot();
                    let resp = StatusResponse { state: &snap };
                    JsonStr(serde_json::to_string(&resp).unwrap_or_default())
                }),
            )
            .route(
                "/api/config",
                get(|picoserve::extract::State(app): picoserve::extract::State<App>| async move {
                    let cfg = app.config();
                    let resp = ConfigResponse { config: &cfg };
                    JsonStr(serde_json::to_string(&resp).unwrap_or_default())
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
    app: &'static App,
) -> ! {
    let mut tcp_rx = [0u8; 1024];
    let mut tcp_tx = [0u8; 1024];
    let mut http_buf = [0u8; 2048];
    picoserve::Server::new(&router.shared().with_state(app), &SERVER_CONFIG, &mut http_buf)
        .listen_and_serve(task_id, stack, 80, &mut tcp_rx, &mut tcp_tx)
        .await
        .into_never()
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

    // Domain App — shared, 'static, Clone (Arc inside). Config starts
    // from compile-time defaults; N9 restores it from NVS.
    let clock: alloc::sync::Arc<dyn Clock> = alloc::sync::Arc::new(EmbassyClock);
    let app: &'static App = mk_static!(App, App::new(clock, Config::default()));

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
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(id, stack, router, app).unwrap());
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
