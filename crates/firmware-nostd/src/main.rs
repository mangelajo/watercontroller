//! Watercontroller no_std firmware — N4 spike.
//!
//! Goals: boot on ESP32-PICO, bring up WiFi STA, print uptime + heap
//! stats every 10 s over UART. Once stable on the bench for an hour,
//! N5 adds picoserve.
//!
//! Modelled on esp-hal v1.1.1's `examples/wifi/embassy_dhcp` — the
//! canonical reference for the current esp-rtos + esp-radio API
//! shape.
//!
//! Single-core only by choice: esp-radio has an open bug
//! (esp-rs/esp-wifi-sys#412) where any embassy task on the second
//! core can corrupt WiFi state. Embassy's default executor is
//! single-core so we just don't reach for the multicore spawner.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
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

esp_bootloader_esp_idf::esp_app_desc!();

/// Macro from the upstream example — wraps `StaticCell` for cases
/// where the value is built at runtime but needs `'static` lifetime.
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// Compile-time WiFi credentials. Real config eventually comes from a
// sequential-storage KV store; the spike just wants to reach an IP.
const SSID: &str = env!("WC_WIFI_SSID");
const PASSWORD: &str = env!("WC_WIFI_PASSWORD");

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));

    // 64 KiB reclaimed heap + 36 KiB internal — matches the upstream
    // example's split; gives esp-radio its breathing room.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    println!("wc-nostd: boot");

    let station_cfg = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.into()),
    );

    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_cfg),
    )
    .unwrap();

    let wifi_iface = interfaces.station;
    let net_cfg = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_iface,
        net_cfg,
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );

    // embassy 0.10 task fns return Result<SpawnToken, _>; unwrap, then
    // hand to Spawner::spawn which is infallible.
    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(heartbeat(stack).unwrap());

    // Idle; supervisor tasks do the work.
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
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// Heartbeat: 10 s cadence, prints uptime + heap free + IP. The
/// closest equivalent of our IDF firmware's `alive` line — what the
/// healthcheck script greps for over serial.
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
            None => println!(
                "alive uptime={}s heap_free={} heap_used={} ip=<none>",
                secs, free, used
            ),
        }
        Timer::after(Duration::from_secs(10)).await;
        secs += 10;
    }
}
