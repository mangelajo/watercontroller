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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use watercontroller_core::api::WifiScanResult;
use watercontroller_core::traits::{Wifi, WifiCreds, WifiState};

const STA_CONNECT_TIMEOUT_S: u8 = 12;
const SCAN_LOOP_INTERVAL: Duration = Duration::from_secs(30);

type ScanReply = Result<Vec<WifiScanResult>, String>;

pub struct WifiSupervisor {
    state: Arc<Mutex<WifiState>>,
    networks: Arc<Mutex<Vec<WifiCreds>>>,
    rescan_signal: Arc<Mutex<bool>>,
    /// Set by an event-loop callback when WIFI_EVENT_STA_BEACON_TIMEOUT
    /// fires. The supervisor polls this in `run_connected` to detect
    /// the "ghost connected" state (STA association cached, data path
    /// dead) — the IDF driver's auto-recovery doesn't always trigger
    /// a DISCONNECTED event from a beacon timeout under PS_MIN_MODEM,
    /// so we handle the BEACON_TIMEOUT event ourselves and force a
    /// reconnect when it lands. See esp-idf #13491 / #11615.
    beacon_timeout: Arc<AtomicBool>,
    /// One-shot scan request slot. `scan()` installs a sender here and
    /// signals the supervisor; the supervisor performs the scan on its
    /// own task (it owns the EspWifi) and sends the result back. Bounded
    /// capacity 1 — if the slot is already taken the new request is
    /// rejected with "scan already in flight" rather than queued.
    scan_request: Arc<Mutex<Option<SyncSender<ScanReply>>>>,
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
            scan_request: Arc::new(Mutex::new(None)),
            ap_ssid,
            ap_password,
            beacon_timeout: Arc::new(AtomicBool::new(false)),
        });

        // Subscribe to WiFi events on the system event loop so we hear
        // STA_BEACON_TIMEOUT. The callback MUST stay trivial — it runs
        // on the sys_evt task's stack (~3 KiB total; CLAUDE.md
        // documents this trap explicitly). One atomic store, no
        // logging, no formatting.
        let beacon_for_cb = supervisor.beacon_timeout.clone();
        // Hold the subscription for the life of the program by
        // leaking it. The supervisor task already lives forever; no
        // teardown path exists.
        match sys_loop.subscribe::<esp_idf_svc::wifi::WifiEvent, _>(move |event| {
            if matches!(event, esp_idf_svc::wifi::WifiEvent::StaBeaconTimeout) {
                beacon_for_cb.store(true, Ordering::Release);
            }
        }) {
            Ok(sub) => {
                std::mem::forget(sub);
            }
            Err(e) => log::warn!("wifi: failed to subscribe WifiEvent: {e:?}"),
        }

        let s = supervisor.clone();
        // 12 KiB ran with only 176 B headroom on /api/diag — too close to
        // overflow for an event-driven supervisor whose callbacks can chain
        // surprisingly deep. 16 KiB gives ~4 KiB margin. The periodic
        // get_ap_info probe is factored into a `#[inline(never)]` helper
        // (`probe_link`) so its heavy locals don't bloat `run()`'s
        // permanent stack frame.
        // Observed peak ~3 KiB at idle, ~5 KiB during STA→AP
        // transitions / scan. 8 KiB gives ~3 KiB headroom.
        // (Pre-task_util-fix this task was actually getting ~10 KiB
        // and HWM lied at 432 — see commit 43f497a.)
        crate::task_util::spawn_named(c"wifi-sup", 8 * 1024, move || {
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
        let changed = {
            let mut cur = self.networks.lock().unwrap();
            let same = cur.len() == networks.len()
                && cur.iter().zip(networks.iter()).all(|(a, b)| a == b);
            if !same {
                *cur = networks.to_vec();
            }
            !same
        };
        if changed {
            log::info!("wifi: network list updated ({} entries)", networks.len());
            *self.rescan_signal.lock().unwrap() = true;
        }
    }
    fn reconnect(&self) {
        *self.rescan_signal.lock().unwrap() = true;
    }
    fn scan(&self) -> ScanReply {
        // Bounded(1) so we don't block on send; the supervisor reads from
        // the slot, not the channel, so receive-only semantics are fine.
        let (tx, rx) = sync_channel::<ScanReply>(1);
        {
            let mut slot = self.scan_request.lock().unwrap();
            if slot.is_some() {
                return Err("scan already in flight".into());
            }
            *slot = Some(tx);
        }
        // NB: do NOT set rescan_signal here. That flag means "reconnect to
        // the network list now" — coupling it with scan() would tear down
        // the STA association on every scan request. The supervisor polls
        // the scan_request slot on each loop tick (every 2–5 s), so the
        // worst-case latency is one sleep window. Reasonable for an
        // interactive scan; cheap compared to dropping the link.
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(r) => r,
            Err(_) => {
                // Clear the slot so a follow-up scan can be issued.
                *self.scan_request.lock().unwrap() = None;
                Err("scan timed out".into())
            }
        }
    }
}

fn run(
    sup: Arc<WifiSupervisor>,
    modem: Modem,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<()> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), Some(nvs))?, sys_loop)?;

    // Each "phase" is its own `#[inline(never)]` function so the locals
    // they need — `WifiState` enum variants (Strings on the stack),
    // 2-arg `log::info!` format buffers, IpAddr-to-String conversion,
    // etc. — only live while that phase is on the stack. Without the
    // split, Rust's prologue would reserve space for *all* phases at
    // entry to `run()` and the wifi-sup task would sit close to its
    // 16 KiB ceiling forever.
    loop {
        let networks = sup.networks.lock().unwrap().clone();
        *sup.rescan_signal.lock().unwrap() = false;

        let connected = try_connect_any(&sup, &mut wifi, &networks);
        if connected {
            run_connected(&sup, &mut wifi);
        } else if !networks.is_empty() {
            run_ap_fallback(&sup, &mut wifi);
        } else {
            run_ap_permanent(&sup, &mut wifi);
        }
    }
}

/// Walk the saved network list and return on the first successful STA
/// association. Returns `false` if none worked.
#[inline(never)]
fn try_connect_any(
    sup: &Arc<WifiSupervisor>,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    networks: &[WifiCreds],
) -> bool {
    for creds in networks {
        log::info!("wifi: trying {}", creds.ssid);
        sup.set_state(WifiState::Connecting { ssid: creds.ssid.clone() });
        if try_connect_sta(wifi, creds).is_ok() {
            announce_connected(sup, wifi, &creds.ssid);
            return true;
        }
        log::warn!("wifi: connect to {} failed", creds.ssid);
    }
    false
}

/// Format + announce the "connected" state. Single-arg log here so the
/// format-buffer cost stays bounded; the 2-arg "connected to {} ({ip})"
/// line we used to have inlined into `run()` was a ~1.5 KiB stack peak
/// that persisted as part of `run()`'s prologue allocation.
#[inline(never)]
fn announce_connected(
    sup: &Arc<WifiSupervisor>,
    wifi: &BlockingWifi<EspWifi<'static>>,
    ssid: &str,
) {
    let ip = ip_string(wifi);
    log::info!("wifi: connected ssid={ssid} ip={ip}");
    // Leave PS_MIN_MODEM enabled (default; required for battery
    // operation). Just tighten the beacon-loss window slightly so a
    // dead AP fires WIFI_EVENT_STA_BEACON_TIMEOUT promptly — our
    // subscribed callback flips supervisor.beacon_timeout and
    // run_connected breaks to force a reconnect. This is the battery-
    // friendly counterpart to esp_wifi_set_ps(NONE): we keep PS on
    // and *react* to the wedge, instead of preventing it.
    unsafe {
        // Inactive time unit: AP beacon intervals (typically 100 ms).
        // Value range 6..=200. Default ~6 (≈ 600 ms — too aggressive,
        // false positives on noisy links). 10 = ~1 s tolerance.
        let rc = esp_idf_svc::sys::esp_wifi_set_inactive_time(
            esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            10,
        );
        if rc != esp_idf_svc::sys::ESP_OK {
            log::warn!("wifi: esp_wifi_set_inactive_time returned {rc}");
        }
    }
    // Reset the flag in case we picked up a stale event during the
    // disconnected period — we only care about beacon timeouts that
    // happen FROM HERE ON.
    sup.beacon_timeout.store(false, Ordering::Release);
    sup.set_state(WifiState::Connected { ssid: ssid.into(), ip });
}

/// Stay-connected loop: every 5 s tick checks for a scan request and the
/// rescan signal; every 30 s runs the link probe.
#[inline(never)]
fn run_connected(sup: &Arc<WifiSupervisor>, wifi: &mut BlockingWifi<EspWifi<'static>>) {
    let mut probe_fails: u32 = 0;
    let mut ticks: u32 = 0;
    while wifi.is_connected().unwrap_or(false) {
        // Beacon timeout from our event-loop subscription — the IDF
        // driver under PS_MIN_MODEM sometimes wedges instead of
        // following beacon-timeout → 5 probes → DISCONNECTED. We
        // bypass that by treating the BEACON_TIMEOUT event itself as
        // "link is dead, reconnect now".
        if sup.beacon_timeout.swap(false, Ordering::AcqRel) {
            log::warn!("wifi: WIFI_EVENT_STA_BEACON_TIMEOUT — forcing reconnect");
            break;
        }
        serve_scan_request(sup, wifi);
        if *sup.rescan_signal.lock().unwrap() {
            break;
        }
        thread::sleep(Duration::from_secs(5));
        ticks += 1;
        // Probe every 12 ticks × 5 s = 60 s. Was 30 s, but each
        // gateway TCP probe under bad signal leaves a socket in
        // TIME_WAIT for ~2 min — at 30 s we accumulated up to
        // 4 lingering sockets per outage, contributing to the
        // lwIP socket-pool exhaustion (errno 23 / ENFILE) we hit.
        if ticks % 12 == 0 && !probe_link(wifi, &mut probe_fails) {
            break;
        }
    }
    log::warn!("wifi: link lost, rescanning");
}

/// AP-mode fallback after a failed connect attempt. Holds AP up while
/// **scanning periodically** for known SSIDs; only returns (which lets
/// the outer loop re-attempt STA) once a saved network is actually
/// visible. Avoids hammering the AP↔STA mode-transition path on the
/// IDF WiFi driver — that path triggered an `ieee80211_hostap_attach`
/// null deref panic on a marginal link.
#[inline(never)]
fn run_ap_fallback(sup: &Arc<WifiSupervisor>, wifi: &mut BlockingWifi<EspWifi<'static>>) {
    enter_ap_mode(sup, wifi);
    let mut waited = Duration::ZERO;
    while !*sup.rescan_signal.lock().unwrap() {
        serve_scan_request(sup, wifi);
        thread::sleep(Duration::from_secs(2));
        waited += Duration::from_secs(2);
        if waited >= SCAN_LOOP_INTERVAL {
            // Periodically probe whether any saved SSID is now in
            // range. Only leave AP mode if we find one — saves a
            // mode-transition round trip otherwise.
            waited = Duration::ZERO;
            if known_ssid_visible(sup, wifi) {
                log::info!("wifi: known SSID visible, leaving AP fallback");
                break;
            }
            log::info!("wifi: no known SSID visible, staying in AP mode");
        }
    }
    let _ = wifi.stop();
}

/// Scan from AP mode (yes, the radio can scan while AP is up — the IDF
/// driver handles the channel hopping) and return true if any of our
/// saved SSIDs is in the result. Failure to scan = pessimistic false
/// (we stay in AP mode rather than risk another mode transition).
#[inline(never)]
fn known_ssid_visible(
    sup: &Arc<WifiSupervisor>,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> bool {
    let saved = sup.networks.lock().unwrap().clone();
    if saved.is_empty() {
        return false;
    }
    let aps = match wifi.scan() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("wifi: AP-mode scan failed: {e:?}");
            return false;
        }
    };
    log::info!("wifi: AP-mode scan saw {} ap(s)", aps.len());
    for ap in &aps {
        for s in &saved {
            if ap.ssid.as_str() == s.ssid {
                let rssi: i32 = ap.signal_strength as i32;
                let ssid = &s.ssid;
                log::info!("wifi: saved SSID {ssid} visible at rssi {rssi}");
                return true;
            }
        }
    }
    false
}

/// Permanent AP mode used when no networks are configured at all.
/// Waits on `rescan_signal` (a `connect()` from the API or CLI) before
/// returning to the supervisor loop.
#[inline(never)]
fn run_ap_permanent(sup: &Arc<WifiSupervisor>, wifi: &mut BlockingWifi<EspWifi<'static>>) {
    enter_ap_mode(sup, wifi);
    while !*sup.rescan_signal.lock().unwrap() {
        serve_scan_request(sup, wifi);
        thread::sleep(Duration::from_secs(2));
    }
    let _ = wifi.stop();
}

/// Bring up the AP and publish the corresponding `WifiState`.
#[inline(never)]
fn enter_ap_mode(sup: &Arc<WifiSupervisor>, wifi: &mut BlockingWifi<EspWifi<'static>>) {
    log::warn!("wifi: entering AP mode ssid={}", sup.ap_ssid);
    if start_ap(wifi, &sup.ap_ssid, &sup.ap_password).is_ok() {
        sup.set_state(WifiState::ApMode {
            ssid: sup.ap_ssid.clone(),
            ip: "192.168.4.1".into(),
        });
    }
}

/// One health-probe tick. Returns `false` when the caller should leave the
/// connected loop and force a reconnect (three consecutive failures).
///
/// Kept in its own function (`#[inline(never)]`) so the heavy locals it
/// needs — `wifi_ap_record_t` (~80 B), `AccessPointInfo`, the log-format
/// argument buffer (~700 B per `{}`) — only live on the stack while the
/// probe is running, not as part of `run()`'s permanent stack frame.
/// Inlining would prologue-allocate all of this on every tick of the
/// connected loop, which previously pushed wifi-sup past 32 KiB.
#[inline(never)]
fn probe_link(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    probe_fails: &mut u32,
) -> bool {
    // Two-layer probe:
    // 1. get_ap_info() — confirms the STA association is intact.
    //    Cheap, no packets on the air.
    // 2. End-to-end TCP probe to the gateway — confirms the data
    //    path is alive. ESP-IDF has a well-documented bug
    //    (#13491, #11615) where the driver keeps reporting a
    //    healthy association after beacon timeout but stops being
    //    able to send frames ("wifi:m f null" flood). Our
    //    WIFI_EVENT_STA_BEACON_TIMEOUT subscription catches the
    //    common case; the gateway probe is the backstop for when
    //    even that event doesn't fire.
    let assoc_ok = match wifi.wifi_mut().driver_mut().get_ap_info() {
        Ok(info) => {
            let rssi: i32 = info.signal_strength as i32;
            log::info!("wifi: rssi {rssi}");
            true
        }
        Err(_) => false,
    };
    let gateway_ok = if assoc_ok { gateway_reachable(wifi) } else { false };
    if assoc_ok && gateway_ok {
        *probe_fails = 0;
        return true;
    }
    *probe_fails += 1;
    let n = *probe_fails;
    let reason = if !assoc_ok {
        "no AP info"
    } else {
        "gateway unreachable"
    };
    log::warn!("wifi: probe failed ({reason}, consecutive {n})");
    if n >= 3 {
        log::warn!("wifi: forcing reconnect after probe failures");
        false
    } else {
        true
    }
}

/// Try to TCP-connect to the netif's default gateway as a data-path
/// liveness check. Any TCP response counts as "alive" — even RST
/// (port closed) means a packet round-tripped. Only a connect timeout
/// or "network unreachable" counts as dead.
///
/// Cheap: short 2 s timeout, single SYN packet on success/RST.
/// Heavy: ICMP ping would be cleaner but requires the IDF ping
/// component + a raw socket; TCP works through std::net.
#[inline(never)]
fn gateway_reachable(wifi: &BlockingWifi<EspWifi<'static>>) -> bool {
    use std::net::{IpAddr, SocketAddr, TcpStream};
    use std::time::Duration as StdDuration;
    let gw_v4 = match wifi.wifi().sta_netif().get_ip_info() {
        Ok(info) => info.subnet.gateway,
        Err(_) => return true, // can't tell → benefit of the doubt
    };
    if gw_v4.octets() == [0, 0, 0, 0] {
        return true; // no gateway configured yet, don't penalise
    }
    let gw_octets = gw_v4.octets();
    let gw = std::net::Ipv4Addr::new(gw_octets[0], gw_octets[1], gw_octets[2], gw_octets[3]);
    let addr = SocketAddr::new(IpAddr::V4(gw), 80);
    match TcpStream::connect_timeout(&addr, StdDuration::from_secs(2)) {
        Ok(_) => true,
        Err(e) => {
            use std::io::ErrorKind;
            match e.kind() {
                // ConnectionRefused = port closed but packets flow — link OK.
                ErrorKind::ConnectionRefused => true,
                // TimedOut / NetworkUnreachable / HostUnreachable = link bad.
                _ => {
                    let kind = e.kind();
                    log::warn!("wifi: gateway TCP probe failed: {kind:?}");
                    false
                }
            }
        }
    }
}

/// Service a pending scan request, if any. Kept `#[inline(never)]` because
/// `BlockingWifi::scan()` returns a `Vec<AccessPointInfo>` containing
/// heapless strings + per-AP metadata, plus we map each into our
/// `WifiScanResult` — sizable transient locals we don't want bloating the
/// supervisor's permanent frame.
#[inline(never)]
fn serve_scan_request(sup: &Arc<WifiSupervisor>, wifi: &mut BlockingWifi<EspWifi<'static>>) {
    let tx = match sup.scan_request.lock().unwrap().take() {
        Some(t) => t,
        None => return,
    };
    log::info!("wifi: scan requested");
    let result = match wifi.scan() {
        Ok(aps) => {
            let mut out = Vec::with_capacity(aps.len());
            for ap in aps {
                out.push(WifiScanResult {
                    ssid: ap.ssid.to_string(),
                    rssi_dbm: ap.signal_strength,
                    auth: auth_label(ap.auth_method),
                    channel: ap.channel,
                });
            }
            log::info!("wifi: scan returned {} ap(s)", out.len());
            Ok(out)
        }
        Err(e) => {
            log::warn!("wifi: scan failed: {e}");
            Err(format!("scan failed: {e}"))
        }
    };
    let _ = tx.send(result);
}

fn auth_label(auth: Option<AuthMethod>) -> String {
    match auth {
        None => "unknown",
        Some(AuthMethod::None) => "open",
        Some(AuthMethod::WEP) => "wep",
        Some(AuthMethod::WPA) => "wpa",
        Some(AuthMethod::WPA2Personal) => "wpa2",
        Some(AuthMethod::WPAWPA2Personal) => "wpa2",
        Some(AuthMethod::WPA2Enterprise) => "wpa2-ent",
        Some(AuthMethod::WPA3Personal) => "wpa3",
        Some(AuthMethod::WPA2WPA3Personal) => "wpa3",
        Some(_) => "unknown",
    }
    .into()
}

#[inline(never)]
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
    // IDF-canonical lifecycle: stop → set_configuration → start →
    // connect. Skipping the stop on a running driver causes panics:
    //   * On STA→STA reconnect: pthread_mutex_unlock null deref
    //     inside BlockingWifi::start (EXCVADDR=0x3).
    //   * On AP→STA mode switch: ieee80211_hostap_attach null
    //     deref inside wifi_set_mode_process.
    // Stop is safe to call when not started (returns error we ignore);
    // it also handles the "started in a different mode" case which
    // set_configuration alone cannot.
    if wifi.is_started().unwrap_or(false) {
        let _ = wifi.stop();
    }
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    wifi.connect()?;
    // Wait for IP — `wait_for_ip` returns Err on DHCP timeout, which
    // we MUST propagate. Previously this line dropped the error with
    // `let _ = …` and then just checked `wifi.is_connected()` (=
    // association), causing the reconnect to claim success at
    // ip=0.0.0.0 and the supervisor to loop forever with no L3.
    wait_for_ip(wifi, STA_CONNECT_TIMEOUT_S)?;
    Ok(())
}

fn wait_for_ip(wifi: &BlockingWifi<EspWifi<'static>>, secs: u8) -> Result<()> {
    for _ in 0..secs {
        if wifi.is_connected().unwrap_or(false) {
            // Association is up — but we also need DHCP to land,
            // otherwise we'll "connect" with ip=0.0.0.0 and stay
            // unreachable indefinitely. Wait for a non-zero IP for
            // up to the rest of the budget, polling every 500 ms.
            // Without this check, today's pattern was: reconnect →
            // association OK → DHCP DISCOVER goes out but no
            // response comes back fast enough → we return Ok →
            // run_connected sees is_connected==true → loops forever
            // with no IP, no traffic, no way out.
            for _ in 0..10 {
                if let Ok(info) = wifi.wifi().sta_netif().get_ip_info() {
                    if info.ip.octets() != [0, 0, 0, 0] {
                        return Ok(());
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
            return Err(anyhow::anyhow!("DHCP timed out"));
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(anyhow::anyhow!("no link"))
}

#[inline(never)]
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
    // Stop the driver before set_configuration when we may be
    // switching modes (STA → AP). The IDF driver panics in
    // `wifi_set_mode_process → wifi_softap_start →
    // ieee80211_hostap_attach` (null deref) if you push a new mode
    // onto a running driver. Decoded today after a probe-induced
    // STA reconnect failed and we fell into AP fallback. Stop is
    // safe to call when not started (returns an error we ignore).
    if wifi.is_started().unwrap_or(false) {
        let _ = wifi.stop();
    }
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    Ok(())
}

#[inline(never)]
fn ip_string(wifi: &BlockingWifi<EspWifi<'static>>) -> String {
    wifi.wifi()
        .sta_netif()
        .get_ip_info()
        .ok()
        .map(|info| info.ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".into())
}
