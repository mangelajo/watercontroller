//! Minimal multicast-DNS responder.
//!
//! Answers `A` queries for `<hostname>.local` so the device is
//! reachable by name (`http://doremorwater.local/`) without a static
//! DHCP reservation. This is deliberately a *responder only* — it
//! doesn't advertise services (no `_http._tcp` PTR/SRV), which the IDF
//! firmware did via the espressif mdns component. Name resolution is
//! the 90% case; service discovery can be layered on later.
//!
//! Protocol: join 224.0.0.251, listen on UDP/5353, parse each query's
//! first question, and if it asks for our name (type A or ANY) emit a
//! standard mDNS answer with the cache-flush bit set.

use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    IpAddress, Stack,
};
use embassy_time::{Duration, Timer};
use esp_println::println;
use heapless::{String, Vec};
use watercontroller_core::app::App;

const MDNS_PORT: u16 = 5353;
const MDNS_GROUP: IpAddress = IpAddress::v4(224, 0, 0, 251);
/// DNS RR type `A` (IPv4 host address) and the wildcard `ANY`.
const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
/// Answer TTL in seconds — short, the standard mDNS default.
const TTL: u32 = 120;

#[embassy_executor::task]
pub async fn mdns_task(stack: Stack<'static>, app: App) {
    // Snapshot the hostname once; the Arc<Config> borrow isn't 'static.
    let hostname: String<32> = {
        let cfg = app.config();
        String::try_from(cfg.wifi.hostname.as_str()).unwrap_or_default()
    };
    if hostname.is_empty() {
        println!("mdns: empty hostname, responder disabled");
        return;
    }

    stack.wait_config_up().await;
    let ipv4 = loop {
        if let Some(c) = stack.config_v4() {
            break c.address.address();
        }
        Timer::after(Duration::from_millis(500)).await;
    };
    if let Err(e) = stack.join_multicast_group(MDNS_GROUP) {
        println!("mdns: join_multicast_group failed: {:?}", e);
        return;
    }

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 512];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );
    if socket.bind(MDNS_PORT).is_err() {
        println!("mdns: bind :5353 failed");
        return;
    }
    println!("mdns: responding for {}.local", hostname.as_str());

    let octets = ipv4.octets();
    let mut pkt = [0u8; 512];
    loop {
        let (n, meta) = match socket.recv_from(&mut pkt).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(resp) = build_response(&pkt[..n], &hostname, octets) {
            // Reply unicast to the querier. RFC 6762 also permits a
            // multicast reply, but unicast reaches the asker just as
            // well and avoids spamming the segment.
            let _ = socket.send_to(&resp, meta.endpoint).await;
        }
    }
}

/// If `query` is a DNS request whose first question matches
/// `<hostname>.local` (type A or ANY), build the answer packet.
fn build_response(query: &[u8], hostname: &str, ip: [u8; 4]) -> Option<Vec<u8, 256>> {
    if query.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount == 0 {
        return None;
    }
    // Walk the first question's name labels (no compression in a
    // question name we'd answer — bail if a 0xC0 pointer appears).
    let mut pos = 12;
    let mut labels: [&[u8]; 4] = [&[]; 4];
    let mut label_count = 0;
    loop {
        let len = *query.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xc0 != 0 || label_count == labels.len() {
            return None;
        }
        let start = pos + 1;
        let end = start + len;
        labels[label_count] = query.get(start..end)?;
        label_count += 1;
        pos = end;
    }
    let qtype = u16::from_be_bytes([*query.get(pos)?, *query.get(pos + 1)?]);
    if qtype != TYPE_A && qtype != TYPE_ANY {
        return None;
    }

    // Expect exactly `<hostname>.local`.
    if label_count != 2
        || !labels[0].eq_ignore_ascii_case(hostname.as_bytes())
        || !labels[1].eq_ignore_ascii_case(b"local")
    {
        return None;
    }

    let mut out: Vec<u8, 256> = Vec::new();
    // Header: id=0, flags=0x8400 (response + authoritative),
    // QD=0, AN=1, NS=0, AR=0.
    out.extend_from_slice(&[0, 0, 0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0]).ok()?;
    // Answer name: <hostname> "local" 0x00
    write_label(&mut out, hostname.as_bytes())?;
    write_label(&mut out, b"local")?;
    out.push(0).ok()?;
    // type A, class IN with cache-flush bit (0x8001).
    out.extend_from_slice(&TYPE_A.to_be_bytes()).ok()?;
    out.extend_from_slice(&0x8001u16.to_be_bytes()).ok()?;
    out.extend_from_slice(&TTL.to_be_bytes()).ok()?;
    // RDLENGTH = 4, RDATA = the IPv4 address.
    out.extend_from_slice(&4u16.to_be_bytes()).ok()?;
    out.extend_from_slice(&ip).ok()?;
    Some(out)
}

fn write_label(out: &mut Vec<u8, 256>, label: &[u8]) -> Option<()> {
    if label.is_empty() || label.len() > 63 {
        return None;
    }
    out.push(label.len() as u8).ok()?;
    out.extend_from_slice(label).ok()?;
    Some(())
}
