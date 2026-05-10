//! mDNS responder backed by the espressif/mdns managed component.
//!
//! Advertises:
//!   * `<hostname>.local`               — A record on the WiFi STA address
//!   * `_http._tcp.local` :80           — for the SPA over plain HTTP
//!   * `_https._tcp.local` :443         — when the HTTPS server is up
//!
//! Direct FFI because esp-idf-svc doesn't wrap mdns. Skipped under qemu
//! because the component's init null-derefs there (call site gates with
//! `#[cfg(not(feature = "qemu"))]`).

use anyhow::{anyhow, Result};
use esp_idf_svc::sys::*;
use std::ffi::{c_char, c_void, CString};

// The espressif/mdns managed component is linked into the binary via the
// extra_components entry in Cargo.toml, but esp-idf-sys's bindgen doesn't
// scan component-only headers (only ESP-IDF core ones). Declare the
// handful of functions we use directly. Signatures are stable across mdns
// 1.x — the component is API-frozen in this major.
unsafe extern "C" {
    fn mdns_init() -> esp_err_t;
    fn mdns_hostname_set(hostname: *const c_char) -> esp_err_t;
    fn mdns_instance_name_set(instance_name: *const c_char) -> esp_err_t;
    fn mdns_service_add(
        instance_name: *const c_char,
        service_type: *const c_char,
        proto: *const c_char,
        port: u16,
        txt: *mut c_void, // mdns_txt_item_t* — nullable; we don't use TXT
        num_items: usize,
    ) -> esp_err_t;
}

/// Initialise the responder + register a host name and the HTTP/HTTPS
/// service records. Idempotent — calling again replaces the registration.
pub fn start(hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        return Err(anyhow!("mdns hostname must not be empty"));
    }
    unsafe {
        check(mdns_init(), "mdns_init")?;

        let c_host = CString::new(hostname).map_err(|_| anyhow!("hostname has nul byte"))?;
        check(mdns_hostname_set(c_host.as_ptr()), "mdns_hostname_set")?;

        // Pretty name shown by clients that display instance names
        // (Avahi browsers, etc.). Mirrors the hostname for consistency.
        check(
            mdns_instance_name_set(c_host.as_ptr()),
            "mdns_instance_name_set",
        )?;

        let proto_tcp = c"_tcp";
        let svc_http = c"_http";
        let svc_https = c"_https";

        // Component versions before 1.x had `mdns_service_add` taking 6
        // args; current take instance_name as nullable first arg. Pass
        // null so the service inherits the global instance name set above.
        let rc = mdns_service_add(
            std::ptr::null(),     // instance_name → use global
            svc_http.as_ptr(),
            proto_tcp.as_ptr(),
            80,
            std::ptr::null_mut(), // no TXT records
            0,
        );
        if rc != ESP_OK {
            log::warn!("mdns_service_add(_http._tcp:80) failed: {rc:#x}");
        }

        let rc = mdns_service_add(
            std::ptr::null(),
            svc_https.as_ptr(),
            proto_tcp.as_ptr(),
            443,
            std::ptr::null_mut(),
            0,
        );
        if rc != ESP_OK {
            log::warn!("mdns_service_add(_https._tcp:443) failed: {rc:#x}");
        }
    }
    log::info!("mdns: advertising {hostname}.local + _http._tcp:80 + _https._tcp:443");
    Ok(())
}

fn check(rc: esp_err_t, what: &str) -> Result<()> {
    if rc == ESP_OK {
        Ok(())
    } else {
        Err(anyhow!("{what}: esp_err {rc:#x}"))
    }
}
