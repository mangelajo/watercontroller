//! Watercontroller no_std firmware — N6 spike.
//!
//! Adds an HTTPS listener on :443 over the N5 plain-HTTP base, via
//! `mbedtls-rs` with HW crypto acceleration (`EspAccel` registers SHA
//! and RSA hooks). Plain HTTP on :80 still works through picoserve as
//! before; the new HTTPS path is a minimal HTTP/1.0 reply (no router
//! wiring yet — that's a follow-up once we know the TLS layer holds).
//!
//! Bootstrap order matters:
//!   1. embassy timer + esp_rtos
//!   2. heap allocators
//!   3. mbedtls timer + wall-clock hooks (so X.509 validity checks work)
//!   4. EspAccel for HW crypto
//!   5. TrngSource → Trng (mbedtls RNG)
//!   6. wifi + embassy-net
//!   7. picoserve on :80 (existing)
//!   8. mbedtls Tls + https_task pool on :443 (new)

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_net::{
    tcp::TcpSocket, IpListenEndpoint, Runner, Stack, StackResources,
};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    rng::{Trng, TrngSource},
    rtc_cntl::Rtc,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    sta::StationConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use mbedtls_rs::{
    sys::hook::backend::{
        embassy::timer::EmbassyTimer,
        esp::{wall_clock::EspRtcWallClock, EspAccel},
    },
    Certificate, Credentials, PrivateKey, ServerSessionConfig, Session, SessionConfig,
    SessionError, Tls, TlsReference, X509,
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
    ($t:ty) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit()
    }};
}

const SSID: &str = env!("WC_WIFI_SSID");
const PASSWORD: &str = env!("WC_WIFI_PASSWORD");

// Self-signed cert/key, DER-encoded. Regenerate with:
//   openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 -nodes
//   openssl x509 -outform DER -in cert.pem -out certs/cert.der
//   openssl rsa  -outform DER -in key.pem  -out certs/key.der
// In production these come from NVS / the config protocol like the IDF
// firmware does — for the spike a baked-in pair gets us to "curl -k".
const CERT_DER: &[u8] = include_bytes!("../certs/cert.der");
const KEY_DER: &[u8] = include_bytes!("../certs/key.der");

// Wall-clock seed for X.509 validity. Pinned to a known-good baseline
// well after the cert's NotBefore. Real firmware will run SNTP and
// update this at runtime. The cert ours is signed against is fresh
// from openssl-now so any 2026 timestamp satisfies it.
const BOOT_WALL_CLOCK_MS: u64 = 1_780_000_000_000; // ~Apr 2026

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
                    Json(Status { uptime_ms: uptime_secs() * 1000, fw: "wc-nostd-N6" })
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
                        fw: "wc-nostd-N6",
                    })
                }),
            )
    }
}

static SERVER_CONFIG: picoserve::Config = picoserve::Config::const_default().keep_connection_alive();

const WEB_TASK_POOL_SIZE: usize = 4;
const HTTPS_TASK_POOL_SIZE: usize = 2;

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

/// HTTPS task. Each instance accepts one connection at a time on :443,
/// wraps the TCP socket in a mbedtls TLS session, and replies with a
/// minimal HTTP/1.0 GET response. picoserve-over-TLS integration is a
/// later polish — for the spike we just want to prove the TLS layer
/// holds up under our HW-accelerated build.
#[embassy_executor::task(pool_size = HTTPS_TASK_POOL_SIZE)]
async fn https_task(task_id: usize, tls: TlsReference<'static>, stack: Stack<'static>) {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: 443 }).await {
            println!("https[{}]: accept error {:?}", task_id, e);
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        let mut buf = [0u8; 2048];
        if let Err(e) = handle_https(tls, &mut socket, &mut buf).await {
            println!("https[{}]: session error {:?}", task_id, e);
        }
        socket.close();
        socket.abort();
        let _ = socket.flush().await;
    }
}

async fn handle_https<'a>(
    tls: TlsReference<'a>,
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(), SessionError> {
    use mbedtls_rs::io::{Read, Write};
    let cert = Certificate::new_no_copy(CERT_DER).unwrap();
    let conf = ServerSessionConfig::new(Credentials {
        certificate: cert,
        private_key: PrivateKey::new(X509::DER(KEY_DER), None).unwrap(),
    });
    let mut session = Session::new(tls, socket, &SessionConfig::Server(conf))?;

    // Read request headers (look for \r\n\r\n).
    let mut offset = 0usize;
    let _headers_end = loop {
        let n = session.read(&mut buf[offset..]).await?;
        if n == 0 {
            break None;
        }
        offset += n;
        if let Some(end) = buf[..offset].windows(4).position(|s| s == b"\r\n\r\n") {
            break Some(end + 4);
        }
    };

    let body = b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: Close\r\n\r\nhello from no_std HTTPS (mbedtls-rs)\r\n";
    session.write_all(body).await?;
    session.close().await?;
    Ok(())
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));

    // Heap bumped from N5's 100 KiB to 160 KiB total — mbedtls TLS
    // contexts are heavy. Internal DRAM total is ~290 KiB on ESP32.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 96 * 1024);

    unsafe { BOOT_INSTANT = Some(Instant::now()); }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    println!("wc-nostd N6: boot");

    // ----- mbedtls bootstrap ----------------------------------------
    let timer = mk_static!(EmbassyTimer, EmbassyTimer);
    unsafe { mbedtls_rs::sys::hook::timer::hook_timer(Some(timer)); }

    let rtc = &*mk_static!(Rtc<'static>, Rtc::new(peripherals.LPWR));
    rtc.set_current_time_us(BOOT_WALL_CLOCK_MS * 1000);
    let clock = mk_static!(EspRtcWallClock<&'static Rtc<'static>>, EspRtcWallClock::new(rtc));
    unsafe { mbedtls_rs::sys::hook::wall_clock::hook_wall_clock(Some(clock)); }

    // ESP32 has HW RSA acceleration (and HW SHA hooked into mbedtls
    // automatically via the `esp32` feature on mbedtls-rs-sys).
    let mut accel = EspAccel::new(peripherals.RSA);
    let _accel_queue = accel.start();

    // True RNG — required by both mbedtls (key gen, nonces) and
    // embassy-net (smoltcp seed).
    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let trng = mk_static!(Trng, Trng::try_new().unwrap());

    let station_cfg = WifiConfig::Station(
        StationConfig::default().with_ssid(SSID).with_password(PASSWORD.into()),
    );

    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_cfg),
    ).unwrap();
    let wifi_iface = interfaces.station;
    let net_cfg = embassy_net::Config::dhcpv4(Default::default());
    let seed = ((trng.random() as u64) << 32) | trng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_iface,
        net_cfg,
        mk_static!(StackResources<8>, StackResources::<8>::new()),
        seed,
    );

    let tls = mk_static!(Tls, Tls::new(trng).unwrap());
    let app = mk_static!(AppRouter<AppProps>, AppProps.build_app());

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(heartbeat(stack).unwrap());
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(id, stack, app).unwrap());
    }
    for id in 0..HTTPS_TASK_POOL_SIZE {
        spawner.spawn(https_task(id, tls.reference(), stack).unwrap());
    }

    println!("wc-nostd N6: listening on :80 (HTTP) + :443 (HTTPS)");

    // Idle main task.
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
