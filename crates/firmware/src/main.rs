mod assets;
mod captive_dns;
mod diag;
mod hw_adc;
mod hw_clock;
mod hw_gpio;
mod hw_nvs;
mod hw_pcnt;
mod http_server;
mod log_telnet;
mod mdns_init;
mod mqtt_client;
mod net_ota;
mod net_wg;
mod net_wifi;
mod serial_cli;
#[cfg(feature = "qemu")]
mod qemu_eth;
mod task_util;
mod tee_log;
mod tls_certgen;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::AnyOutputPin;
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use hw_adc::EspAdcChan;
use hw_clock::EspClock;
use hw_gpio::EspGpioOut;
use hw_nvs::EspNvsStore;
use hw_pcnt::EspPulseCounter;
use watercontroller_core::traits::{Adc, GpioOut, PulseCounter};
use log::{info, warn};
use mqtt_client::EspMqtt;
use net_wifi::WifiSupervisor;
use std::sync::Arc;
use std::time::Duration;
use watercontroller_core::app::App;
use watercontroller_core::config::Config;
use watercontroller_core::mqtt_dispatch::MqttIntegration;
use watercontroller_core::traits::{Clock, Mqtt, NvsStore, Wifi};

/// Stub `Wifi` used under the `qemu` feature so the same http_server code
/// can require a Wifi handle on both targets. open_eth provides the actual
/// connectivity; nothing about the radio is reachable from qemu.
#[cfg(feature = "qemu")]
struct NoopWifi;
#[cfg(feature = "qemu")]
impl watercontroller_core::traits::Wifi for NoopWifi {
    fn state(&self) -> watercontroller_core::traits::WifiState {
        watercontroller_core::traits::WifiState::Disconnected
    }
    fn connect(&self, _: &[watercontroller_core::traits::WifiCreds]) {}
    fn reconnect(&self) {}
    fn scan(&self) -> Result<Vec<watercontroller_core::api::WifiScanResult>, String> {
        Err("scan unavailable under qemu".into())
    }
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    let _log_buf = watercontroller_core::log_buffer::init_global(1024);
    tee_log::install(log::LevelFilter::Info);

    info!("{}", watercontroller_core::greeting());
    info!(
        "watercontroller-firmware v{} booted",
        env!("CARGO_PKG_VERSION")
    );
    info!("embedded SPA size: {} bytes", assets::INDEX_HTML.len());

    let peripherals = Peripherals::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;
    let sys_loop = EspSystemEventLoop::take()?;

    // Take application-level peripherals (GPIO outputs, ADC channels,
    // PCNT for the flow meter). These are independent of the WiFi /
    // open_eth choice, so we set them up before the cfg-gated networking.
    let pins = peripherals.pins;
    let valve_open = EspGpioOut::new(AnyOutputPin::from(pins.gpio26))?;
    let valve_close = EspGpioOut::new(AnyOutputPin::from(pins.gpio27))?;
    let drain = EspGpioOut::new(AnyOutputPin::from(pins.gpio25))?;
    let sprinkler1_pin = EspGpioOut::new(AnyOutputPin::from(pins.gpio12))?;
    let sprinkler2_pin = EspGpioOut::new(AnyOutputPin::from(pins.gpio4))?;
    let _led1 = EspGpioOut::new(AnyOutputPin::from(pins.gpio13))?;
    let _led2 = EspGpioOut::new(AnyOutputPin::from(pins.gpio14))?;
    // ADC reads also hang under QEMU. Hardware path uses real driver;
    // qemu path falls back to placeholders so the calibration pipeline
    // still produces visible output.
    #[cfg(not(feature = "qemu"))]
    let (battery_adc, pressure_adc) =
        hw_adc::build_battery_pressure(peripherals.adc1, pins.gpio36, pins.gpio32)?;
    #[cfg(feature = "qemu")]
    let (battery_adc, pressure_adc) = {
        let _ = (peripherals.adc1, pins.gpio36, pins.gpio32);
        (
            hw_adc::PlaceholderAdc(1130), // calibrates to 5.00 V
            hw_adc::PlaceholderAdc(0),    // calibrates to ~0 bar
        )
    };
    // ESP-IDF's legacy PCNT driver crashes at init under QEMU (the qemu
    // PCNT model is incomplete). On real hardware it works; in qemu we
    // substitute the no-op placeholder. Each branch produces a different
    // concrete type — the consumer (spawn_sensor_task) is generic so this
    // is a compile-time choice.
    #[cfg(not(feature = "qemu"))]
    let pcnt = EspPulseCounter::new(peripherals.pcnt0, pins.gpio33.into())?;
    #[cfg(feature = "qemu")]
    let pcnt = {
        let _ = peripherals.pcnt0;
        let _ = pins.gpio33;
        hw_pcnt::PlaceholderPcnt::default()
    };

    // Initialize the netif layer up-front. Both the WiFi supervisor (real hw
    // path) and the qemu/ETH path need this to be done before any network
    // service spawns; doing it here is a no-op if it's already initialized.
    unsafe {
        let _ = esp_idf_svc::sys::esp_netif_init();
    }

    // Open the "wc" NVS namespace and load runtime config (defaults if absent).
    let nvs = EspNvs::new(nvs_part.clone(), "wc", true)?;
    let nvs_store: Arc<dyn NvsStore> = Arc::new(EspNvsStore::new(nvs));
    let mut config = match Config::load(&*nvs_store) {
        Ok(c) => {
            info!("config loaded from NVS");
            c
        }
        Err(e) => {
            warn!("config load failed ({e:?}); using defaults");
            let defaults = Config::default();
            // Persist defaults so the web UI shows what we'd start with.
            if let Err(e) = defaults.save(&*nvs_store) {
                warn!("failed to persist default config: {e:?}");
            }
            defaults
        }
    };

    // First-boot self-signed cert generation for HTTPS. Skipped if the user
    // has already pasted their own cert+key via the Settings tab. The
    // generated PEM blobs are written back to NVS so the cert (and its
    // public-key fingerprint) stays stable across reboots — important since
    // browsers cache the per-cert "I trust this self-signed CA" decision.
    if config.https.cert_pem.is_empty() && config.https.key_pem.is_empty() {
        let cn = if config.wifi.hostname.is_empty() {
            "doremorwater".to_string()
        } else {
            format!("{}.local", config.wifi.hostname)
        };
        info!("https: no cert in config, generating self-signed (CN={cn})");
        let t0 = std::time::Instant::now();
        match tls_certgen::generate_self_signed(&cn) {
            Ok((cert, key)) => {
                info!(
                    "https: self-signed cert generated in {} ms ({} B cert, {} B key)",
                    t0.elapsed().as_millis(),
                    cert.len(),
                    key.len(),
                );
                config.https.cert_pem = cert;
                config.https.key_pem = key;
                if let Err(e) = config.save(&*nvs_store) {
                    warn!("https: failed to persist generated cert: {e:?}");
                }
            }
            Err(e) => {
                warn!("https: self-signed cert generation failed: {e:?}");
            }
        }
    }

    let clock: Arc<dyn Clock> = Arc::new(EspClock);
    let app = App::with_nvs(clock.clone(), config.clone(), Some(nvs_store.clone()));
    if let Some(state) = app.restored_valve_state() {
        info!(
            "boot: restored composite water control state = {}",
            if state { "ON" } else { "OFF" }
        );
    }

    // Captive-portal DNS responder runs from boot. The redirect target is
    // updated by the wifi-mirror task below as state changes (None unless
    // the device is in AP mode).
    let captive_redirect: captive_dns::RedirectIp =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    captive_dns::spawn(captive_redirect.clone());

    // Bring up WiFi (multi-SSID with AP fallback). The `qemu` feature skips
    // this — qemu doesn't simulate the WiFi peripheral well enough to
    // initialize `EspWifi`. open_eth provides networking instead.
    #[cfg(not(feature = "qemu"))]
    let wifi: Arc<dyn watercontroller_core::traits::Wifi> = {
        let sup = WifiSupervisor::spawn(
            peripherals.modem,
            sys_loop.clone(),
            nvs_part.clone(),
            config.wifi.ap_ssid.clone(),
            config.wifi.ap_password.clone(),
            config.wifi.networks.clone(),
        )?;
        spawn_wifi_state_mirror(app.clone(), sup.clone(), captive_redirect.clone());

        // MQTT: connect once WiFi is up. Spawned task waits for STA up and (re)connects
        // to the broker on link recovery, then publishes HA Discovery + retained state.
        let mqtt: Arc<EspMqtt> = Arc::new(EspMqtt::new());
        spawn_mqtt_supervisor(app.clone(), mqtt.clone(), sup.clone());

        sup
    };
    #[cfg(feature = "qemu")]
    let wifi: Arc<dyn watercontroller_core::traits::Wifi> = Arc::new(NoopWifi);
    #[cfg(feature = "qemu")]
    let _eth = {
        let _ = (&peripherals.modem, &nvs_part);
        log::info!("qemu feature enabled: bringing up open_eth instead of WiFi");
        let eth = qemu_eth::start(peripherals.mac, sys_loop.clone())?;
        let ip = eth
            .eth()
            .netif()
            .get_ip_info()
            .map(|i| i.ip.to_string())
            .unwrap_or_else(|_| "0.0.0.0".into());
        app.update_state(|s| {
            s.network.wifi = Some(watercontroller_core::traits::WifiState::Connected {
                ssid: "qemu-open-eth".into(),
                ip,
            });
        });
        eth
    };

    // mdns: skipped under qemu because the component's linked-in init code
    // null-derefs early in ESP-IDF startup there (PC=0 right after
    // spi_flash init). Real hardware works.
    #[cfg(not(feature = "qemu"))]
    if let Err(e) = mdns_init::start(&config.wifi.hostname) {
        warn!("mdns init failed: {e:?}");
    }

    log_telnet::spawn(23);
    serial_cli::spawn(app.clone(), nvs_store.clone(), wifi.clone());
    let _httpd = http_server::spawn(
        app.clone(),
        nvs_store.clone(),
        wifi.clone(),
        80,
        &config.https.cert_pem,
        &config.https.key_pem,
    )?;

    // Config persistence used to be a periodic polling task; PUT /api/config
    // now saves inline (and tls_certgen / valve-state already do), so there
    // are no remaining sites that bypass NVS. One pthread stack reclaimed.

    // Schedule executor: once-per-minute evaluator.
    spawn_schedule_task(app.clone(), clock.clone());

    // Sensor task — reads ADC/PCNT, applies calibration, updates state.
    spawn_sensor_task(app.clone(), clock.clone(), battery_adc, pressure_adc, pcnt);

    // Tick task — drives switches + valve sequencer at 10 ms, applies the
    // resulting outputs to actual GPIO pins.
    {
        let app = app.clone();
        let mut valve_open = valve_open;
        let mut valve_close = valve_close;
        let mut drain = drain;
        let mut sprinkler1_pin = sprinkler1_pin;
        let mut sprinkler2_pin = sprinkler2_pin;
        std::thread::Builder::new()
            .name("tick".into())
            .stack_size(8 * 1024)
            .spawn(move || loop {
                let outputs = app.tick();
                valve_open.set(outputs.valve.open_motor);
                valve_close.set(outputs.valve.close_motor);
                drain.set(outputs.valve.drain);
                sprinkler1_pin.set(outputs.sprinkler_1);
                sprinkler2_pin.set(outputs.sprinkler_2);
                std::thread::sleep(Duration::from_millis(10));
            })
            .ok();
    }

    let started = clock.monotonic_ms();
    let reset_reason = unsafe { esp_idf_svc::sys::esp_reset_reason() };
    let reset_reason_str = reset_reason_label(reset_reason);
    info!("reset reason: {reset_reason_str}");

    // Rollback safety: defer marking the running slot valid until we've
    // proven we can actually run. If the firmware panics or wedges before
    // this fires, the bootloader will roll back on the next reboot.
    // Criteria: uptime >= 60s + WiFi reached Connected once + heap above
    // floor. mark_app_valid is idempotent.
    let mut app_marked_valid = false;
    const HEALTHY_UPTIME_MS: u64 = 60_000;
    const HEAP_FLOOR_BYTES: u32 = 20 * 1024;

    loop {
        std::thread::sleep(Duration::from_secs(10));
        let uptime_ms = clock.monotonic_ms().saturating_sub(started);
        let free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
        let min_free_heap = unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() };

        if !app_marked_valid && uptime_ms >= HEALTHY_UPTIME_MS && min_free_heap >= HEAP_FLOOR_BYTES {
            let wifi_ok = matches!(
                wifi.state(),
                watercontroller_core::traits::WifiState::Connected { .. }
            );
            if wifi_ok {
                let up_s = uptime_ms / 1000;
                info!("ota: healthy runtime, marking slot valid");
                info!("  uptime    : {up_s}s");
                info!("  min heap  : {min_free_heap}B");
                net_ota::mark_app_valid();
                app_marked_valid = true;
            }
        }
        app.update_state(|s| {
            s.uptime_ms = uptime_ms;
            if s.firmware_version.is_empty() {
                s.firmware_version = watercontroller_core::version().into();
            }
            s.diagnostics.free_heap_bytes = Some(free_heap);
            s.diagnostics.min_free_heap_bytes = Some(min_free_heap);
            if s.diagnostics.reset_reason.is_none() {
                s.diagnostics.reset_reason = Some(reset_reason_str.into());
            }
        });
        info!(
            "alive (uptime {}s, heap free {}B, min {}B)",
            uptime_ms / 1000, free_heap, min_free_heap
        );
    }
}

fn reset_reason_label(r: esp_idf_svc::sys::esp_reset_reason_t) -> &'static str {
    use esp_idf_svc::sys::*;
    #[allow(non_upper_case_globals)]
    match r {
        esp_reset_reason_t_ESP_RST_POWERON => "power-on",
        esp_reset_reason_t_ESP_RST_EXT => "external reset",
        esp_reset_reason_t_ESP_RST_SW => "software restart",
        esp_reset_reason_t_ESP_RST_PANIC => "panic / exception",
        esp_reset_reason_t_ESP_RST_INT_WDT => "interrupt watchdog",
        esp_reset_reason_t_ESP_RST_TASK_WDT => "task watchdog",
        esp_reset_reason_t_ESP_RST_WDT => "other watchdog",
        esp_reset_reason_t_ESP_RST_DEEPSLEEP => "wake from deep sleep",
        esp_reset_reason_t_ESP_RST_BROWNOUT => "brownout",
        esp_reset_reason_t_ESP_RST_SDIO => "sdio reset",
        _ => "unknown",
    }
}

fn spawn_sensor_task<B, P, C>(
    app: App,
    clock: Arc<dyn Clock>,
    mut battery_adc: B,
    mut pressure_adc: P,
    pcnt: C,
) where
    B: Adc + Send + 'static,
    P: Adc + Send + 'static,
    C: PulseCounter + Send + 'static,
{
    std::thread::Builder::new()
        .name("sensors".into())
        .stack_size(8 * 1024)
        .spawn(move || {

            // Battery uses sliding-window moving avg, window=15.
            let mut bat_window: std::collections::VecDeque<f32> = std::collections::VecDeque::with_capacity(15);
            let mut last_battery_ms = 0u64;
            let mut last_pressure_ms = 0u64;
            let mut last_flow_ms = 0u64;
            let mut last_pulse_count = pcnt.count();

            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now_ms = clock.monotonic_ms();
                let cfg = app.config();

                // Battery — every 10 min, average of 15 samples.
                if now_ms.saturating_sub(last_battery_ms) >= 10 * 60 * 1000 || last_battery_ms == 0 {
                    let raw = battery_adc.read_raw() as f32;
                    let v = cfg.sensors.battery.apply(raw);
                    bat_window.push_back(v);
                    if bat_window.len() > 15 {
                        bat_window.pop_front();
                    }
                    let avg = bat_window.iter().sum::<f32>() / bat_window.len() as f32;
                    app.update_state(|s| s.sensors.battery_v = Some(avg));
                    last_battery_ms = now_ms;
                }

                // Pressure — every 1 min. Two-stage calibration chain.
                if now_ms.saturating_sub(last_pressure_ms) >= 60 * 1000 || last_pressure_ms == 0 {
                    let raw = pressure_adc.read_raw() as f32;
                    let stage1 = cfg.sensors.pressure_stage1.apply(raw);
                    let bar = cfg.sensors.pressure_stage2.apply(stage1);
                    app.update_state(|s| s.sensors.pressure_bar = Some(bar));
                    last_pressure_ms = now_ms;
                }

                // Flow + total water — every 1 min using pulse delta.
                if now_ms.saturating_sub(last_flow_ms) >= 60 * 1000 || last_flow_ms == 0 {
                    let pulses_now = pcnt.count();
                    let delta = pulses_now.saturating_sub(last_pulse_count) as f32;
                    let elapsed_s = (now_ms.saturating_sub(last_flow_ms).max(1)) as f32 / 1000.0;
                    let pps = delta / elapsed_s;
                    let lph = pps * cfg.sensors.flow_lph_per_pps;
                    let total = pulses_now as f32 * cfg.sensors.flow_l_per_pulse;
                    app.update_state(|s| {
                        s.sensors.flow_lph = Some(lph);
                        s.sensors.total_l = Some(total);
                    });
                    last_pulse_count = pulses_now;
                    last_flow_ms = now_ms;
                }
            }
        })
        .ok();
}

fn spawn_mqtt_supervisor(app: App, mqtt: Arc<EspMqtt>, wifi: Arc<WifiSupervisor>) {
    // 8 KiB ran with only 376 B headroom — mbedTLS handshake during the
    // initial broker connect is hungry. 12 KiB leaves ~4 KiB margin.
    task_util::spawn_named(c"mqtt-sup", 12 * 1024, move || {
            use watercontroller_core::traits::WifiState;
            // Exponential backoff on failed connects. The IDF MQTT
            // client's auth-refused failure surfaces as a Disconnected
            // event *after* connect() returns Ok — from our vantage
            // it just looks like the client never reaches connected.
            // Without backoff we'd hammer the broker every 10s and
            // recreate the client each time, eventually exhausting the
            // IDF event-loop pool (see the two panics in serial-logs
            // /serial-current.log from the earlier run).
            const BACKOFF_INITIAL_MS: u64 = 5_000;
            const BACKOFF_MAX_MS: u64 = 300_000; // 5 min cap
            let mut last_attempt: u64 = 0;
            let mut backoff_ms: u64 = BACKOFF_INITIAL_MS;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let cfg = app.config();
                let connected_via_sta = matches!(wifi.state(), WifiState::Connected { .. });
                if !cfg.mqtt.enabled || cfg.mqtt.broker_url.is_empty() || !connected_via_sta {
                    continue;
                }

                if !mqtt.is_connected() {
                    let now = unsafe { esp_idf_svc::sys::esp_timer_get_time() } as u64 / 1000;
                    if now.saturating_sub(last_attempt) < backoff_ms {
                        continue;
                    }
                    last_attempt = now;
                    log::info!("mqtt: connecting to {}", cfg.mqtt.broker_url);
                    if let Err(e) = mqtt.connect(
                        &cfg.mqtt.broker_url,
                        Some(cfg.mqtt.username.as_str()).filter(|s| !s.is_empty()),
                        Some(cfg.mqtt.password.as_str()).filter(|s| !s.is_empty()),
                        &cfg.wifi.hostname,
                        &cfg.mqtt.ca_cert_pem,
                        &cfg.mqtt.client_cert_pem,
                        &cfg.mqtt.client_key_pem,
                    ) {
                        log::warn!("mqtt connect failed: {e:?}");
                        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                        continue;
                    }
                    // Give the IDF client a moment to settle. If the
                    // broker rejects (bad creds), is_connected() stays
                    // false and we extend backoff below.
                    std::thread::sleep(Duration::from_secs(3));
                    if !mqtt.is_connected() {
                        log::warn!(
                            "mqtt: broker did not accept connect; backing off {}s",
                            backoff_ms / 1000
                        );
                        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                        continue;
                    }
                }

                if mqtt.is_connected() {
                    backoff_ms = BACKOFF_INITIAL_MS; // reset on success
                    let integ = MqttIntegration {
                        app: app.clone(),
                        mqtt: mqtt.clone() as Arc<dyn Mqtt>,
                        firmware_version: env!("CARGO_PKG_VERSION").into(),
                    };
                    integ.on_connect();
                    // Loop publishing snapshots until disconnected.
                    while mqtt.is_connected() {
                        integ.publish_state(&app.snapshot());
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
    });
}

fn spawn_wifi_state_mirror(
    app: App,
    wifi: Arc<WifiSupervisor>,
    captive_redirect: captive_dns::RedirectIp,
) {
    task_util::spawn_named(c"wifi-mirror", 4 * 1024, move || loop {
        let st = wifi.state();
        // Captive DNS only redirects when the device itself is the AP —
        // never in STA mode (that would hijack legitimate resolution).
        let new_redirect = match &st {
            watercontroller_core::traits::WifiState::ApMode { ip, .. } => ip.parse().ok(),
            _ => None,
        };
        *captive_redirect.lock().unwrap() = new_redirect;
        app.update_state(|s| s.network.wifi = Some(st.clone()));
        std::thread::sleep(Duration::from_secs(2));
    });
}

fn spawn_schedule_task(app: App, clock: Arc<dyn Clock>) {
    task_util::spawn_named(c"schedule", 8 * 1024, move || {
            // Evaluator works in *local* time. SNTP sets the system TZ via
            // CONFIG_NEWLIB_LIBC_TZ_BUILTIN; chrono::Utc::now() returns UTC,
            // we apply a fixed-offset based on the configured TZ name only
            // for fallback (Europe/Madrid: +01:00 winter / +02:00 summer).
            // Proper TZ resolution lands when chrono-tz is added.
            let mut last_local = local_now(&*clock, &app.config().timezone);
            let active = app
                .config()
                .schedule
                .rules
                .iter()
                .filter(|r| r.enabled)
                .count();
            info!(
                "schedule: starting evaluator, {} active rule(s), tz={}",
                active,
                app.config().timezone
            );
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let cfg = app.config();
                let now_local = local_now(&*clock, &cfg.timezone);
                let hits = cfg.schedule.evaluate_range(last_local, now_local);
                log::debug!(
                    "schedule: eval window {} → {} ({} hit{})",
                    last_local.format("%H:%M:%S"),
                    now_local.format("%H:%M:%S"),
                    hits.len(),
                    if hits.len() == 1 { "" } else { "s" }
                );
                for rule in hits {
                    info!(
                        "schedule: fire id='{}' action={:?} at {}",
                        rule.id,
                        rule.action,
                        now_local.format("%Y-%m-%d %H:%M:%S")
                    );
                    use watercontroller_core::api::SwitchCommand;
                    use watercontroller_core::schedule::Action;
                    match &rule.action {
                        Action::Switch { id } => {
                            if !app.fire_schedule_sprinkler(id, rule.duration_secs) {
                                log::warn!(
                                    "schedule: rule '{}' references unknown switch '{}', skipping",
                                    rule.id, id
                                );
                            }
                        }
                        Action::WaterControl { on } => {
                            let _ = app.switch_command(SwitchCommand::WaterControl { on: *on });
                        }
                    }
                }
                last_local = now_local;
            }
    });
}

/// Local time for the configured timezone (DST-correct via chrono-tz).
fn local_now(clock: &dyn Clock, tz: &str) -> chrono::NaiveDateTime {
    watercontroller_core::schedule::to_local(clock.now(), tz)
}
