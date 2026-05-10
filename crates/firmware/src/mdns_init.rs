//! mDNS responder backed by the espressif/mdns managed component.
//!
//! The component dep is currently commented out in
//! `crates/firmware/Cargo.toml` because pulling it in null-derefs early
//! in ESP-IDF startup under QEMU. To enable on real hardware:
//!   1. uncomment the `extra_components` block in
//!      `[package.metadata.esp-idf-sys]`,
//!   2. swap this stub for the EspMdns wrapper in the file's git history
//!      (the call sites already exist).

use anyhow::Result;

pub fn start(hostname: &str) -> Result<()> {
    log::info!("mdns: stub (would advertise {hostname}.local)");
    Ok(())
}
