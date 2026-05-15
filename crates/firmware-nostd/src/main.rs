//! Watercontroller no_std firmware.
//!
//! HTTP server on :80 via picoserve, backed by an embassy task pool.
//! WiFi STA + DHCP via esp-radio + embassy-net.
//!
//! No HTTPS server: the ESP32-PICO-D4 has no esp-hal PSRAM support
//! (the D4 PSRAM pinout isn't implemented — esp-hal panics "PSRAM is
//! unsupported on this chip"), and a mbedtls server session's
//! ~16 KiB×2 SSL buffers + RSA scratch don't fit in the ~69 KiB of
//! free internal DRAM left after WiFi. HTTP-only on a trusted LAN is
//! the accepted trade-off; inbound TLS returns when we move to an
//! ESP32-S3 (PSRAM works there).
//!
//! Outbound TLS — for webhook calls to Slack/Discord/HA over HTTPS —
//! is a separate matter: a single transient client session is
//! controllable for concurrency and will be wired behind the `tls`
//! Cargo feature when the webhook dispatcher is ported.
//!
//! Single-core only by choice: esp-radio has an open bug
//! (esp-rs/esp-wifi-sys#412) where an embassy task on the second core
//! can corrupt WiFi state. Embassy's default executor is single-core.

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

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
use picoserve::{response::Json, routing::get, AppBuilder, AppRouter};
use serde::Serialize;

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

static mut BOOT_INSTANT: Option<Instant> = None;

fn uptime_secs() -> u64 {
    unsafe { BOOT_INSTANT }
        .map(|t| (Instant::now() - t).as_secs())
        .unwrap_or(0)
}

#[derive(Serialize)]
struct Heap {
    total_free_bytes: usize,
    total_used_bytes: usize,
}
#[derive(Serialize)]
struct Diag {
    uptime_s: u64,
    heap: Heap,
    fw: &'static str,
}
#[derive(Serialize)]
struct Status {
    uptime_ms: u64,
    fw: &'static str,
}

struct AppProps;
impl AppBuilder for AppProps {
    type PathRouter = impl picoserve::routing::PathRouter;
    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        picoserve::Router::new()
            .route("/", get(|| async { "hello from no_std watercontroller\n" }))
            .route(
                "/api/status",
                get(|| async {
                    Json(Status { uptime_ms: uptime_secs() * 1000, fw: "wc-nostd" })
                }),
            )
            .route(
                "/api/diag",
                get(|| async {
                    Json(Diag {
                        uptime_s: uptime_secs(),
                        heap: Heap {
                            total_free_bytes: esp_alloc::HEAP.free(),
                            total_used_bytes: esp_alloc::HEAP.used(),
                        },
                        fw: "wc-nostd",
                    })
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
    app: &'static AppRouter<AppProps>,
) -> ! {
    let mut tcp_rx = [0u8; 1024];
    let mut tcp_tx = [0u8; 1024];
    let mut http_buf = [0u8; 2048];
    picoserve::Server::new(app, &SERVER_CONFIG, &mut http_buf)
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

    let app = mk_static!(AppRouter<AppProps>, AppProps.build_app());

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(heartbeat(stack).unwrap());
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(id, stack, app).unwrap());
    }

    println!("wc-nostd: listening on :80");

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
