//! SoftAP setup mode.
//!
//! When the device can't reach any configured WiFi network (or has
//! none configured), it reboots into AP mode: it brings up a SoftAP,
//! hands joining clients an address, steers them to the setup SPA, and
//! watches for a known network to return so it can reboot back to STA.
//!
//! embassy-net provides the AP-side network stack but no DHCP *server*
//! and no DNS, so both are hand-rolled here over plain UDP sockets
//! (avoids new crates; the wire formats are small and commented below):
//!
//! * `dhcp_server_task` hands every client the fixed lease `192.168.4.2`.
//! * `dns_server_task` answers every A query with the SoftAP address so
//!   a phone's captive-portal probe resolves here and pops the SPA.
//! * `scan_task` brings the SoftAP up and reboots into STA the moment a
//!   configured network is back in range.

use alloc::{string::String, sync::Arc, vec::Vec};

use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    IpAddress, IpEndpoint, Stack,
};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    ap::AccessPointConfig, scan::ScanConfig, sta::StationConfig, Config as WifiConfig,
    WifiController,
};
use watercontroller_core::{app::App, traits::NvsStore};

/// The SoftAP's own address — also its gateway, DNS, and DHCP server id.
pub const AP_IP: [u8; 4] = [192, 168, 4, 1];
/// The single address handed out to a joining client.
const CLIENT_IP: [u8; 4] = [192, 168, 4, 2];
/// Lease time advertised to the client (seconds).
const LEASE_SECS: u32 = 2 * 60 * 60;

// ---- DHCP (RFC 2131) -------------------------------------------------

/// The 4-byte cookie at offset 236 that marks the start of DHCP options.
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// Append one DHCP option `code`/`data` at `i`, returning the new offset.
fn put_opt(out: &mut [u8], i: usize, code: u8, data: &[u8]) -> usize {
    out[i] = code;
    out[i + 1] = data.len() as u8;
    out[i + 2..i + 2 + data.len()].copy_from_slice(data);
    i + 2 + data.len()
}

/// The DHCP message type (option 53) carried by `req`, if any.
fn dhcp_msg_type(req: &[u8]) -> Option<u8> {
    if req.len() < 240 || req[236..240] != DHCP_MAGIC {
        return None;
    }
    let mut i = 240;
    while i + 1 < req.len() {
        match req[i] {
            255 => break,    // end
            0 => {
                i += 1;      // pad
                continue;
            }
            code => {
                let len = req[i + 1] as usize;
                if code == 53 && len >= 1 && i + 2 < req.len() {
                    return Some(req[i + 2]);
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// Build a BOOTREPLY (`msg_type` = 2 OFFER / 5 ACK) into `out`, echoing
/// the client's transaction id + MAC from `req`. Returns the length.
fn build_dhcp_reply(req: &[u8], msg_type: u8, out: &mut [u8; 300]) -> usize {
    out.fill(0);
    out[0] = 2; // op = BOOTREPLY
    out[1] = 1; // htype = ethernet
    out[2] = 6; // hlen
    out[4..8].copy_from_slice(&req[4..8]); // xid
    out[10..12].copy_from_slice(&req[10..12]); // flags
    out[16..20].copy_from_slice(&CLIENT_IP); // yiaddr
    out[20..24].copy_from_slice(&AP_IP); // siaddr
    out[28..44].copy_from_slice(&req[28..44]); // chaddr (MAC + padding)
    out[236..240].copy_from_slice(&DHCP_MAGIC);
    let mut i = 240;
    i = put_opt(out, i, 53, &[msg_type]); // DHCP message type
    i = put_opt(out, i, 54, &AP_IP); // server identifier
    i = put_opt(out, i, 51, &LEASE_SECS.to_be_bytes()); // lease time
    i = put_opt(out, i, 1, &[255, 255, 255, 0]); // subnet mask
    i = put_opt(out, i, 3, &AP_IP); // router
    i = put_opt(out, i, 6, &AP_IP); // DNS server
    out[i] = 255; // end
    i + 1
}

#[embassy_executor::task]
pub async fn dhcp_server_task(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 600];
    let mut tx_buf = [0u8; 600];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if socket.bind(67).is_err() {
        log::info!("ap-dhcp: bind :67 failed");
        return;
    }
    log::info!("ap-dhcp: server ready ({}.{}.{}.{})", AP_IP[0], AP_IP[1], AP_IP[2], AP_IP[3]);

    let mut pkt = [0u8; 600];
    let mut reply = [0u8; 300];
    loop {
        let n = match socket.recv_from(&mut pkt).await {
            Ok((n, _)) => n,
            Err(_) => continue,
        };
        let req = &pkt[..n];
        // op == 1 is BOOTREQUEST (from a client).
        if req.len() < 240 || req[0] != 1 {
            continue;
        }
        let reply_type = match dhcp_msg_type(req) {
            Some(1) => 2, // DISCOVER -> OFFER
            Some(3) => 5, // REQUEST  -> ACK
            _ => continue,
        };
        let len = build_dhcp_reply(req, reply_type, &mut reply);
        // The client has no IP yet — reply via the limited broadcast.
        let dst = IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68);
        if socket.send_to(&reply[..len], dst).await.is_err() {
            log::info!("ap-dhcp: reply send failed");
        } else {
            log::info!(
                "ap-dhcp: {} -> {}.{}.{}.{}",
                if reply_type == 2 { "OFFER" } else { "ACK" },
                CLIENT_IP[0], CLIENT_IP[1], CLIENT_IP[2], CLIENT_IP[3],
            );
        }
    }
}

// ---- Captive DNS -----------------------------------------------------

/// Build a DNS response that points every A query at the SoftAP. Returns
/// the response length, or `None` if the datagram isn't a single-question
/// query we should answer.
fn build_dns_reply(q: &[u8], out: &mut [u8]) -> Option<usize> {
    if q.len() < 12 {
        return None;
    }
    // Bit 15 of the flags word set => already a response.
    if q[2] & 0x80 != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([q[4], q[5]]);
    if qdcount != 1 {
        return None;
    }
    // Walk the QNAME labels (length-prefixed, terminated by a 0 byte).
    let mut i = 12;
    while i < q.len() && q[i] != 0 {
        i += 1 + q[i] as usize;
    }
    let q_end = i + 5; // root label (1) + qtype (2) + qclass (2)
    if q_end > q.len() || q_end + 16 > out.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([q[i + 1], q[i + 2]]);
    let answer = qtype == 1; // A record

    out[0] = q[0];
    out[1] = q[1]; // transaction id
    out[2] = 0x81; // QR=1, recursion desired echoed
    out[3] = 0x80; // recursion available, RCODE=0
    out[4] = 0;
    out[5] = 1; // qdcount
    out[6] = 0;
    out[7] = if answer { 1 } else { 0 }; // ancount
    out[8..12].fill(0); // nscount + arcount
    out[12..q_end].copy_from_slice(&q[12..q_end]); // echo the question

    let mut o = q_end;
    if answer {
        out[o..o + 2].copy_from_slice(&[0xC0, 0x0C]); // name: pointer to offset 12
        out[o + 2..o + 4].copy_from_slice(&[0, 1]); // type A
        out[o + 4..o + 6].copy_from_slice(&[0, 1]); // class IN
        out[o + 6..o + 10].copy_from_slice(&60u32.to_be_bytes()); // TTL 60 s
        out[o + 10..o + 12].copy_from_slice(&[0, 4]); // rdlength
        out[o + 12..o + 16].copy_from_slice(&AP_IP); // rdata
        o += 16;
    }
    Some(o)
}

#[embassy_executor::task]
pub async fn dns_server_task(stack: Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 512];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if socket.bind(53).is_err() {
        log::info!("ap-dns: bind :53 failed");
        return;
    }
    log::info!("ap-dns: captive resolver ready");

    let mut q = [0u8; 512];
    let mut resp = [0u8; 512];
    loop {
        let (n, meta) = match socket.recv_from(&mut q).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(len) = build_dns_reply(&q[..n], &mut resp) {
            let _ = socket.send_to(&resp[..len], meta).await;
        }
    }
}

// ---- STA-return watcher ----------------------------------------------

/// Bring the SoftAP up and reboot into STA the moment a configured
/// network is back in range. Owns the WiFi controller for the lifetime
/// of AP mode.
#[embassy_executor::task]
pub async fn scan_task(
    mut controller: WifiController<'static>,
    app: App,
    nvs: Arc<dyn NvsStore>,
    ap_cfg: AccessPointConfig,
) {
    // APSTA: the SoftAP beacons; the station side stays available for
    // scanning. No STA network stack is created, so the single-stack
    // RAM budget holds. The station SSID is a throwaway — this task
    // only ever scans, it never connects with it.
    let apsta = WifiConfig::AccessPointStation(
        StationConfig::default().with_ssid("wc-ap-scan"),
        ap_cfg,
    );
    if let Err(e) = controller.set_config(&apsta) {
        log::info!("ap-scan: set_config failed: {:?}", e);
        return;
    }
    log::info!("ap-scan: SoftAP up — watching for a configured network");

    loop {
        Timer::after(Duration::from_secs(30)).await;
        let known: Vec<String> = app
            .config()
            .wifi
            .networks
            .iter()
            .map(|n| n.ssid.clone())
            .collect();
        if known.is_empty() {
            continue;
        }
        let found = match controller.scan_async(&ScanConfig::default()).await {
            Ok(aps) => aps,
            Err(e) => {
                log::info!("ap-scan: scan failed: {:?}", e);
                continue;
            }
        };
        if found
            .iter()
            .any(|ap| known.iter().any(|s| s.as_str() == ap.ssid.as_str()))
        {
            log::info!("ap-scan: configured network back in range — rebooting to STA");
            crate::write_boot_mode(&*nvs, crate::BootMode::Sta);
            crate::ota::request_reboot();
            return;
        }
    }
}
