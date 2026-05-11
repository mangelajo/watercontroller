//! WiFi supervisor: tries each configured SSID in order with a short timeout,
//! falling back to AP+captive-portal mode when none of the known SSIDs are
//! reachable. Periodically rescans in AP mode in case the configured network
//! comes back up.
//!
//! The supervisor owns the `EspWifi` driver and runs its own thread; the
//! `Wifi` trait surface used by `core` exposes only the current state and the
//! ability to request a reconnect.

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
    EspWifi, ScanMethod,
};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use watercontroller_core::traits::{Wifi, WifiCreds, WifiState};

const STA_CONNECT_TIMEOUT_S: u8 = 12;
const SCAN_LOOP_INTERVAL: Duration = Duration::from_secs(30);

pub struct WifiSupervisor {
    state: Arc<Mutex<WifiState>>,
    networks: Arc<Mutex<Vec<WifiCreds>>>,
    rescan_signal: Arc<Mutex<bool>>,
    ap_ssid: String,
    ap_password: String,
}

impl WifiSupervisor {
    /// Build the supervisor and spawn its background thread. The thread owns
    /// the `EspWifi` driver for its entire lifetime.
    pub fn spawn(
        modem: Modem,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        ap_ssid: String,
        ap_password: String,
        initial_networks: Vec<WifiCreds>,
    ) -> Result<Arc<Self>> {
        let supervisor = Arc::new(Self {
            state: Arc::new(Mutex::new(WifiState::Disconnected)),
            networks: Arc::new(Mutex::new(initial_networks)),
            rescan_signal: Arc::new(Mutex::new(true)),
            ap_ssid,
            ap_password,
        });

        let s = supervisor.clone();
        // 16 KiB blew up with a stack-overflow once the periodic health
        // probe started calling get_ap_info() + logging the result on this
        // task. 24 KiB restores comfortable headroom for the deepest call
        // chain (lwIP / esp_wifi internals + log format buffer).
        crate::task_util::spawn_named(c"wifi-sup", 32 * 1024, move || {
            if let Err(e) = run(s, modem, sys_loop, nvs) {
                log::error!("wifi supervisor terminated: {e:?}");
            }
        });

        Ok(supervisor)
    }

    fn set_state(&self, new_state: WifiState) {
        *self.state.lock().unwrap() = new_state;
    }
}

impl Wifi for WifiSupervisor {
    fn state(&self) -> WifiState {
        self.state.lock().unwrap().clone()
    }
    fn connect(&self, networks: &[WifiCreds]) {
        *self.networks.lock().unwrap() = networks.to_vec();
        *self.rescan_signal.lock().unwrap() = true;
    }
    fn reconnect(&self) {
        *self.rescan_signal.lock().unwrap() = true;
    }
}

fn run(
    sup: Arc<WifiSupervisor>,
    modem: Modem,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<()> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), Some(nvs))?, sys_loop)?;

    loop {
        let networks = sup.networks.lock().unwrap().clone();
        *sup.rescan_signal.lock().unwrap() = false;

        let mut connected = false;
        for creds in &networks {
            log::info!("wifi: trying {}", creds.ssid);
            sup.set_state(WifiState::Connecting { ssid: creds.ssid.clone() });
            if try_connect_sta(&mut wifi, creds).is_ok() {
                let ip = ip_string(&wifi);
                log::info!("wifi: connected to {} ({ip})", creds.ssid);
                sup.set_state(WifiState::Connected {
                    ssid: creds.ssid.clone(),
                    ip,
                });
                connected = true;
                break;
            }
            log::warn!("wifi: connect to {} failed", creds.ssid);
        }

        if connected {
            // Stay connected. Every 30 s poll the AP record for RSSI + as a
            // health probe: get_ap_info() returns NOT_CONNECT (12303) when the
            // driver knows the link is gone, which catches silent AP drops
            // that don't fire WIFI_EVENT_STA_DISCONNECTED. 3 consecutive
            // failures force a reconnect.
            let mut probe_fails: u32 = 0;
            let mut ticks: u32 = 0;
            while wifi.is_connected().unwrap_or(false) {
                if *sup.rescan_signal.lock().unwrap() {
                    break;
                }
                thread::sleep(Duration::from_secs(5));
                ticks += 1;
                if ticks % 6 == 0 {
                    match wifi.wifi_mut().driver_mut().get_ap_info() {
                        Ok(info) => {
                            probe_fails = 0;
                            log::info!(
                                "wifi: link ok rssi={} dBm ssid={}",
                                info.signal_strength,
                                info.ssid
                            );
                        }
                        Err(e) => {
                            probe_fails += 1;
                            log::warn!(
                                "wifi: ap_info probe failed ({e:?}) consecutive={probe_fails}"
                            );
                            if probe_fails >= 3 {
                                log::warn!("wifi: 3 probe failures — forcing reconnect");
                                break;
                            }
                        }
                    }
                }
            }
            log::warn!("wifi: link lost, rescanning");
        } else if !networks.is_empty() {
            // No known SSID was reachable — bring up AP fallback so the user
            // can still reach the device for re-provisioning.
            log::warn!(
                "wifi: no known SSIDs reachable; entering AP mode '{}'",
                sup.ap_ssid
            );
            if start_ap(&mut wifi, &sup.ap_ssid, &sup.ap_password).is_ok() {
                sup.set_state(WifiState::ApMode {
                    ssid: sup.ap_ssid.clone(),
                    ip: "192.168.4.1".into(),
                });
            }
            // Rescan loop: every SCAN_LOOP_INTERVAL or on explicit rescan signal.
            let mut waited = Duration::ZERO;
            while !*sup.rescan_signal.lock().unwrap() && waited < SCAN_LOOP_INTERVAL {
                thread::sleep(Duration::from_secs(2));
                waited += Duration::from_secs(2);
            }
            // Take down AP before retrying STA.
            let _ = wifi.stop();
        } else {
            // No networks configured at all — sit in AP mode permanently.
            if start_ap(&mut wifi, &sup.ap_ssid, &sup.ap_password).is_ok() {
                sup.set_state(WifiState::ApMode {
                    ssid: sup.ap_ssid.clone(),
                    ip: "192.168.4.1".into(),
                });
            }
            // Wait for a `connect()` call from the API to add networks.
            while !*sup.rescan_signal.lock().unwrap() {
                thread::sleep(Duration::from_secs(2));
            }
            let _ = wifi.stop();
        }
    }
}

fn try_connect_sta(wifi: &mut BlockingWifi<EspWifi<'static>>, creds: &WifiCreds) -> Result<()> {
    let cfg = Configuration::Client(ClientConfiguration {
        ssid: creds.ssid.as_str().try_into().map_err(|_| anyhow::anyhow!("ssid too long"))?,
        password: creds
            .password
            .as_str()
            .try_into()
            .map_err(|_| anyhow::anyhow!("password too long"))?,
        auth_method: if creds.password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        scan_method: ScanMethod::CompleteScan(esp_idf_svc::wifi::ScanSortMethod::Signal),
        ..Default::default()
    });
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    wifi.connect()?;
    // Wait for IP. Failure to acquire IP within timeout = treat as failed connect.
    let _ = wait_for_ip(wifi, STA_CONNECT_TIMEOUT_S);
    if wifi.is_connected()? {
        Ok(())
    } else {
        Err(anyhow::anyhow!("connect timed out"))
    }
}

fn wait_for_ip(wifi: &BlockingWifi<EspWifi<'static>>, secs: u8) -> Result<()> {
    for _ in 0..secs {
        if wifi.is_connected().unwrap_or(false) {
            // Give DHCP a moment.
            thread::sleep(Duration::from_secs(1));
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(anyhow::anyhow!("no link"))
}

fn start_ap(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    ssid: &str,
    password: &str,
) -> Result<()> {
    let cfg = Configuration::AccessPoint(AccessPointConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow::anyhow!("ap ssid too long"))?,
        password: password
            .try_into()
            .map_err(|_| anyhow::anyhow!("ap password too long"))?,
        auth_method: if password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        max_connections: 4,
        ..Default::default()
    });
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    Ok(())
}

fn ip_string(wifi: &BlockingWifi<EspWifi<'static>>) -> String {
    wifi.wifi()
        .sta_netif()
        .get_ip_info()
        .ok()
        .map(|info| info.ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".into())
}
