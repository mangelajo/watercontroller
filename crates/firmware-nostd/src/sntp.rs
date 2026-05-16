//! Minimal SNTP client — one UDP round-trip to set the wall clock.
//!
//! `EmbassyClock::now()` is otherwise `BASE_EPOCH_S + uptime`, which
//! drifts from real time the moment the build ages. The cron schedule
//! engine needs an honest UTC clock to fire rules at the right hour,
//! so once the network is up we resolve `pool.ntp.org`, do a single
//! SNTP exchange, and publish the derived "unix epoch at boot" offset.
//! `now()` then reports `epoch_at_boot + uptime`.
//!
//! Re-syncs every 6 h to bound crystal drift.

use embassy_net::{
    dns::DnsQueryType,
    udp::{PacketMetadata, UdpSocket},
    IpEndpoint, Stack,
};
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicI64, Ordering};

/// Unix epoch (seconds) at the instant the firmware booted. 0 means
/// "not yet synced" — `EmbassyClock` falls back to its compile-time
/// baseline until this is set.
pub static EPOCH_AT_BOOT: AtomicI64 = AtomicI64::new(0);

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// Lower bound for a sane reply: 2025-01-01. A server returning less
/// than this is garbage (or we mis-parsed) — reject it.
const SANITY_MIN: u64 = 1_735_689_600;

/// Re-sync cadence once an initial sync succeeds.
const RESYNC: Duration = Duration::from_secs(6 * 60 * 60);
/// Retry cadence while no sync has succeeded yet.
const RETRY: Duration = Duration::from_secs(30);

#[embassy_executor::task]
pub async fn sntp_task(stack: Stack<'static>) {
    stack.wait_config_up().await;

    loop {
        match sync_once(stack).await {
            Ok(unix_now) => {
                let at_boot = unix_now as i64 - crate::uptime_secs() as i64;
                EPOCH_AT_BOOT.store(at_boot, Ordering::Relaxed);
                log::info!("sntp: synced, unix now {}", unix_now);
                Timer::after(RESYNC).await;
            }
            Err(e) => {
                log::info!("sntp: sync failed ({}), retry in 30 s", e);
                Timer::after(RETRY).await;
            }
        }
    }
}

/// One DNS-resolve + SNTP request/response. Returns the current Unix
/// time in seconds.
async fn sync_once(stack: Stack<'static>) -> Result<u64, &'static str> {
    let addrs = stack
        .dns_query("pool.ntp.org", DnsQueryType::A)
        .await
        .map_err(|_| "dns")?;
    let server = *addrs.first().ok_or("dns empty")?;

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 128];
    let mut tx_buf = [0u8; 128];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );
    socket.bind(0).map_err(|_| "bind")?;

    // SNTP request: 48 bytes, all zero except the first — LI=0, VN=4,
    // Mode=3 (client).
    let mut req = [0u8; 48];
    req[0] = 0x23;
    socket
        .send_to(&req, IpEndpoint::new(server, 123))
        .await
        .map_err(|_| "send")?;

    let mut resp = [0u8; 48];
    let recv = embassy_futures::select::select(
        socket.recv_from(&mut resp),
        Timer::after(Duration::from_secs(5)),
    )
    .await;
    let n = match recv {
        embassy_futures::select::Either::First(r) => r.map_err(|_| "recv")?.0,
        embassy_futures::select::Either::Second(()) => return Err("timeout"),
    };
    if n < 48 {
        return Err("short");
    }

    // Transmit Timestamp — seconds field at bytes 40..44 (big-endian,
    // NTP epoch). The fractional part (44..48) is ignored: 1 s
    // resolution is plenty for cron scheduling.
    let ntp_secs = u32::from_be_bytes([resp[40], resp[41], resp[42], resp[43]]) as u64;
    let unix = ntp_secs.checked_sub(NTP_UNIX_OFFSET).ok_or("pre-1970")?;
    if unix < SANITY_MIN {
        return Err("insane");
    }
    Ok(unix)
}
