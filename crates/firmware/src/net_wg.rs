//! Wireguard — stubbed until M11. Implementation will go through the
//! `esp_wireguard` ESP-IDF component via FFI. Pin a known-good version of the
//! component in `idf_component.yml` before wiring this in.

pub fn enabled() -> bool {
    false
}
