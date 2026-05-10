//! mDNS responder — deferred. ESP-IDF's mdns component lives in the IDF
//! Component Registry (managed_components) rather than in esp-idf-svc by
//! default; pulling it in via `idf_component.yml` is the right path but
//! requires a working ETH driver before it's worth wiring.
//!
//! This stub keeps the call site stable so adding mDNS later is a single
//! file change.

use anyhow::Result;

pub fn start(hostname: &str) -> Result<()> {
    log::info!("mdns init: stub (would advertise {hostname}.local)");
    Ok(())
}
