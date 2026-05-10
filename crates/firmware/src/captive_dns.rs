//! Tiny DNS responder for the captive-portal flow. Binds UDP/53 and
//! answers any IN-A query with a fixed IP (the device's AP-side IP).
//! Non-A queries are ignored. lwIP doesn't need a full DNS server here —
//! we just want every name lookup from a phone freshly joined to our AP
//! to resolve to us, so its captive-portal probes hit our HTTPD.
//!
//! Runs unconditionally — when the device is in STA mode and clients
//! aren't using us as their DNS, the socket sits idle. When in AP mode,
//! ESP-IDF's DHCP server hands out our IP as the DNS too, so phones
//! joining the AP query us and we redirect them.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;

/// Shared "where should DNS replies point" — updated by the WiFi supervisor
/// as STA / AP state changes. When set, the DNS thread answers every A
/// query with this IP. When None, replies are suppressed entirely (so we
/// don't accidentally hijack STA-side resolution).
pub type RedirectIp = Arc<Mutex<Option<Ipv4Addr>>>;

pub fn spawn(redirect: RedirectIp) {
    thread::Builder::new()
        .name("captive-dns".into())
        .stack_size(6 * 1024)
        .spawn(move || {
            let socket = match UdpSocket::bind("0.0.0.0:53") {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("captive_dns: bind 0.0.0.0:53 failed: {e}");
                    return;
                }
            };
            log::info!("captive_dns: listening on UDP/53");
            let mut buf = [0u8; 512];
            loop {
                let (n, src) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("captive_dns: recv: {e}");
                        continue;
                    }
                };
                let Some(ip) = *redirect.lock().unwrap() else {
                    continue;
                };
                if let Some(resp) = build_a_response(&buf[..n], ip.octets()) {
                    let _ = socket.send_to(&resp, src);
                }
            }
        })
        .ok();
}

/// Build an A-record response for a single-question query, pointing at
/// `ip`. Returns `None` if the request isn't a well-formed single-question
/// IN-A lookup we can answer.
fn build_a_response(req: &[u8], ip: [u8; 4]) -> Option<Vec<u8>> {
    if req.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([req[4], req[5]]);
    if qdcount != 1 {
        return None;
    }
    // Walk the labels of the single question to find QTYPE/QCLASS.
    let mut i = 12usize;
    while i < req.len() {
        let len = req[i] as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // we don't follow pointers in the question section
        }
        i += len + 1;
        if i >= req.len() {
            return None;
        }
    }
    if i + 4 > req.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([req[i], req[i + 1]]);
    let qclass = u16::from_be_bytes([req[i + 2], req[i + 3]]);
    if qtype != 1 || qclass != 1 {
        return None;
    }
    let q_end = i + 4;

    let mut out = Vec::with_capacity(q_end + 16);
    out.extend_from_slice(&req[0..2]); // ID
    out.extend_from_slice(&[0x81, 0x80]); // QR=1, AA=1, RD=1, RA=1, no error
    out.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    out.extend_from_slice(&[0x00, 0x01]); // ANCOUNT = 1
    out.extend_from_slice(&[0x00, 0x00]); // NSCOUNT
    out.extend_from_slice(&[0x00, 0x00]); // ARCOUNT
    out.extend_from_slice(&req[12..q_end]); // echo question
    // Answer: name pointer to offset 12 (start of question name).
    out.extend_from_slice(&[0xC0, 0x0C]);
    out.extend_from_slice(&[0x00, 0x01]); // TYPE A
    out.extend_from_slice(&[0x00, 0x01]); // CLASS IN
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL 60s
    out.extend_from_slice(&[0x00, 0x04]); // RDLENGTH
    out.extend_from_slice(&ip);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dig +short captive.test @<ip>` would generate a query like this.
    fn make_query(name: &[&str]) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&[0x12, 0x34]); // ID
        q.extend_from_slice(&[0x01, 0x00]); // RD=1
        q.extend_from_slice(&[0x00, 0x01]); // QDCOUNT
        q.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // AN/NS/AR
        for label in name {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0); // root label
        q.extend_from_slice(&[0x00, 0x01]); // QTYPE A
        q.extend_from_slice(&[0x00, 0x01]); // QCLASS IN
        q
    }

    #[test]
    fn answers_a_query() {
        let q = make_query(&["captive", "test"]);
        let r = build_a_response(&q, [10, 0, 4, 1]).unwrap();
        // Header: ID echoed, QR=1, ANCOUNT=1.
        assert_eq!(&r[0..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80);
        assert_eq!(&r[6..8], &[0x00, 0x01]);
        // Last 4 bytes of the answer should be the IP.
        assert_eq!(&r[r.len() - 4..], &[10, 0, 4, 1]);
    }

    #[test]
    fn rejects_non_a_query() {
        let mut q = make_query(&["foo"]);
        let len = q.len();
        q[len - 4] = 0;
        q[len - 3] = 0x10; // QTYPE = 16 (TXT)
        assert!(build_a_response(&q, [1, 2, 3, 4]).is_none());
    }
}
