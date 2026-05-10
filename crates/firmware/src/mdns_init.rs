//! mDNS responder so the device is reachable as `<hostname>.local` (default
//! `doremorwater.local`) without depending on DHCP or DNS. Services advertised:
//! `_http._tcp` on port 80 and `_telnet._tcp` on port 23.
//!
//! The `EspMdns` handle is returned and must be kept alive for the lifetime
//! of the program — dropping it tears down the responder.

use anyhow::Result;
use esp_idf_svc::mdns::EspMdns;

pub fn start(hostname: &str) -> Result<EspMdns> {
    let mut mdns = EspMdns::take()?;
    mdns.set_hostname(hostname)?;
    mdns.set_instance_name("doremorwater")?;
    mdns.add_service(None, "_http", "_tcp", 80, &[("path", "/")])?;
    mdns.add_service(None, "_telnet", "_tcp", 23, &[])?;
    log::info!("mdns: hostname='{hostname}.local', advertising _http + _telnet");
    Ok(mdns)
}
