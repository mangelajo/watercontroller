mod assets;
mod hw_adc;
mod hw_clock;
mod hw_gpio;
mod hw_nvs;
mod hw_pcnt;
mod http_server;
mod log_telnet;
mod mqtt_client;
mod net_ota;
mod net_wg;
mod net_wifi;
mod tee_log;

use anyhow::Result;
use chrono::{TimeZone, Utc};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use hw_adc::PlaceholderAdc;
use hw_clock::EspClock;
use hw_nvs::EspNvsStore;
use hw_pcnt::PlaceholderPcnt;
use watercontroller_core::traits::{Adc, PulseCounter};
use log::{info, warn};
use mqtt_client::EspMqtt;
use net_wifi::WifiSupervisor;
use std::sync::Arc;
use std::time::Duration;
use watercontroller_core::app::App;
use watercontroller_core::config::Config;
use watercontroller_core::mqtt_dispatch::MqttIntegration;
use watercontroller_core::traits::{Clock, Mqtt, NvsStore, Wifi};

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

    // Open the "wc" NVS namespace and load runtime config (defaults if absent).
    let nvs = EspNvs::new(nvs_part.clone(), "wc", true)?;
    let nvs_store: Arc<dyn NvsStore> = Arc::new(EspNvsStore::new(nvs));
    let config = match Config::load(&*nvs_store) {
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

    let clock: Arc<dyn Clock> = Arc::new(EspClock);
    let app = App::new(clock.clone(), config.clone());

    // Bring up WiFi (multi-SSID with AP fallback).
    let wifi = WifiSupervisor::spawn(
        peripherals.modem,
        sys_loop.clone(),
        nvs_part.clone(),
        config.wifi.ap_ssid.clone(),
        config.wifi.ap_password.clone(),
        config.wifi.networks.clone(),
    )?;
    spawn_wifi_state_mirror(app.clone(), wifi.clone());

    // MQTT: connect once WiFi is up. Spawned task waits for STA up and (re)connects
    // to the broker on link recovery, then publishes HA Discovery + retained state.
    let mqtt: Arc<EspMqtt> = Arc::new(EspMqtt::new());
    spawn_mqtt_supervisor(app.clone(), mqtt.clone(), wifi.clone());

    log_telnet::spawn(23);
    let _httpd = http_server::spawn(app.clone(), 80)?;

    // Periodic config persistence: save the in-memory config back to NVS once
    // a minute. This catches edits made via PUT /api/config without forcing
    // every request to do an NVS write inline.
    spawn_config_persist(app.clone(), nvs_store.clone());

    // Schedule executor: once-per-minute evaluator.
    spawn_schedule_task(app.clone(), clock.clone());

    // Sensor task — reads ADC/PCNT (currently placeholders), applies the
    // calibration tables from config, and updates the device snapshot.
    spawn_sensor_task(app.clone(), clock.clone());

    // Tick task — drives switches + valve sequencer at 10 ms.
    {
        let app = app.clone();
        std::thread::Builder::new()
            .name("tick".into())
            .stack_size(8 * 1024)
            .spawn(move || loop {
                let _ = app.tick();
                std::thread::sleep(Duration::from_millis(10));
            })
            .ok();
    }

    let started = clock.monotonic_ms();
    loop {
        std::thread::sleep(Duration::from_secs(10));
        let uptime_ms = clock.monotonic_ms().saturating_sub(started);
        app.update_state(|s| {
            s.uptime_ms = uptime_ms;
            if s.firmware_version.is_empty() {
                s.firmware_version = watercontroller_core::version().into();
            }
        });
        info!("alive (uptime {} ms)", uptime_ms);
    }
}

fn spawn_sensor_task(app: App, clock: Arc<dyn Clock>) {
    std::thread::Builder::new()
        .name("sensors".into())
        .stack_size(8 * 1024)
        .spawn(move || {
            // Placeholder peripherals — replaced with real ADC/PCNT
            // wrappers in a future milestone (see crates/firmware/src/hw_adc.rs
            // and hw_pcnt.rs). The pipeline is identical; only the trait
            // implementations change.
            let mut battery_adc = PlaceholderAdc(1130); // → 5.00 V via default cal
            let mut pressure_adc = PlaceholderAdc(0); // → ~0 bar default
            let pcnt = PlaceholderPcnt::default();

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
    std::thread::Builder::new()
        .name("mqtt-sup".into())
        .stack_size(8 * 1024)
        .spawn(move || {
            use watercontroller_core::traits::WifiState;
            let mut last_attempt: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let cfg = app.config();
                let connected_via_sta = matches!(wifi.state(), WifiState::Connected { .. });
                if !cfg.mqtt.enabled || cfg.mqtt.broker_url.is_empty() || !connected_via_sta {
                    continue;
                }

                if !mqtt.is_connected() {
                    let now = unsafe { esp_idf_svc::sys::esp_timer_get_time() } as u64 / 1000;
                    if now.saturating_sub(last_attempt) < 10_000 {
                        continue; // back off
                    }
                    last_attempt = now;
                    log::info!("mqtt: connecting to {}", cfg.mqtt.broker_url);
                    if let Err(e) = mqtt.connect(
                        &cfg.mqtt.broker_url,
                        Some(cfg.mqtt.username.as_str()).filter(|s| !s.is_empty()),
                        Some(cfg.mqtt.password.as_str()).filter(|s| !s.is_empty()),
                        &cfg.wifi.hostname,
                    ) {
                        log::warn!("mqtt connect failed: {e:?}");
                        continue;
                    }
                }

                if mqtt.is_connected() {
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
        })
        .ok();
}

fn spawn_wifi_state_mirror(app: App, wifi: Arc<WifiSupervisor>) {
    std::thread::Builder::new()
        .name("wifi-mirror".into())
        .stack_size(4 * 1024)
        .spawn(move || loop {
            let st = wifi.state();
            app.update_state(|s| s.network.wifi = Some(st.clone()));
            std::thread::sleep(Duration::from_secs(2));
        })
        .ok();
}

fn spawn_config_persist(app: App, nvs: Arc<dyn NvsStore>) {
    std::thread::Builder::new()
        .name("config-persist".into())
        .stack_size(8 * 1024)
        .spawn(move || {
            let mut last_saved_json = serde_json::to_vec(&app.config()).unwrap_or_default();
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let cfg_json = serde_json::to_vec(&app.config()).unwrap_or_default();
                if cfg_json != last_saved_json {
                    if let Err(e) = app.config().save(&*nvs) {
                        warn!("nvs save failed: {e:?}");
                    } else {
                        info!("config persisted to NVS ({} bytes)", cfg_json.len());
                        last_saved_json = cfg_json;
                    }
                }
            }
        })
        .ok();
}

fn spawn_schedule_task(app: App, clock: Arc<dyn Clock>) {
    std::thread::Builder::new()
        .name("schedule".into())
        .stack_size(8 * 1024)
        .spawn(move || {
            // Evaluator works in *local* time. SNTP sets the system TZ via
            // CONFIG_NEWLIB_LIBC_TZ_BUILTIN; chrono::Utc::now() returns UTC,
            // we apply a fixed-offset based on the configured TZ name only
            // for fallback (Europe/Madrid: +01:00 winter / +02:00 summer).
            // Proper TZ resolution lands when chrono-tz is added.
            let mut last_local = local_now(&*clock);
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let now_local = local_now(&*clock);
                let cfg = app.config();
                let hits = cfg.schedule.evaluate_range(last_local, now_local);
                for rule in hits {
                    info!("schedule fire: {} → {:?}", rule.id, rule.action);
                    use watercontroller_core::api::SwitchCommand;
                    use watercontroller_core::schedule::Action;
                    let cmd = match &rule.action {
                        Action::Switch { id } => match id.as_str() {
                            "sprinkler_1" => Some(SwitchCommand::Sprinkler1 { on: true }),
                            "sprinkler_2" => Some(SwitchCommand::Sprinkler2 { on: true }),
                            _ => None,
                        },
                        Action::WaterControl { on } => {
                            Some(SwitchCommand::WaterControl { on: *on })
                        }
                    };
                    if let Some(c) = cmd {
                        let _ = app.switch_command(c);
                    }
                }
                last_local = now_local;
            }
        })
        .ok();
}

/// Approximate local time using the device's UTC clock + a default
/// Europe/Madrid offset. Replace with chrono-tz lookup once the dependency
/// is added (deferred for binary-size reasons in initial bring-up).
fn local_now(clock: &dyn Clock) -> chrono::NaiveDateTime {
    let utc = clock.now();
    // Naïve fixed offset; not DST-correct. Schedules are minute-resolution
    // and the SNTP-synced UTC clock is authoritative for the time we publish
    // to MQTT, so a wrong-by-1h schedule fire is noticeable but not damaging.
    let offset = chrono::FixedOffset::east_opt(3600).unwrap();
    Utc.timestamp_opt(utc.timestamp(), 0)
        .single()
        .map(|t| t.with_timezone(&offset).naive_local())
        .unwrap_or_else(|| utc.naive_utc())
}
