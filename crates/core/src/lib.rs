//! Platform-independent logic for the doremorwater water controller.
//!
//! This crate must compile on any host (x86_64-unknown-linux-gnu, ESP32 in
//! either esp-idf-svc or esp-hal/embassy worlds). It does NOT depend on
//! `esp-idf-*`. Hardware access goes through traits in [`traits`]; firmware
//! and host implementations satisfy them differently.
//!
//! No_std compatibility: with `default-features = false`, this crate
//! builds against `core` + `alloc` only. The `std`-only `log_buffer`
//! module (mpsc-based log fanout) is gated out under no_std — the no_std
//! firmware uses its own embassy-based mechanism instead.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{format, string::String};

pub mod api;
pub mod app;
pub mod calibration;
pub mod config;
pub mod ha_discovery;
#[cfg(feature = "std")]
pub mod log_buffer;
pub mod mqtt_dispatch;
pub mod schedule;
pub mod state;
pub mod switch;
pub mod traits;
pub mod water_valve;
pub mod webhook;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn greeting() -> String {
    format!("watercontroller-core v{}", version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_includes_version() {
        let g = greeting();
        assert!(g.contains(version()));
        assert!(g.starts_with("watercontroller-core"));
    }
}
