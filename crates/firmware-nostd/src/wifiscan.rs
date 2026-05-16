//! WiFi access-point scan plumbing.
//!
//! `scan_async` needs `&mut WifiController`, which lives in
//! `connection_task`. So a scan can't run from the HTTP handler
//! directly: the handler signals `SCAN_REQ`, the connection task does
//! the scan between disconnect waits (`wait_for_disconnect_async` only
//! borrows `&self`, so it composes with the request signal), and posts
//! the converted results back through `SCAN_RESULT`.

use alloc::{string::String, vec::Vec};

use embassy_futures::select::{select, Either};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use watercontroller_core::api::{WifiScanResponse, WifiScanResult};

/// Set by an HTTP handler to ask the connection task for a scan.
pub static SCAN_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Set by the connection task with the converted scan results.
pub static SCAN_RESULT: Signal<CriticalSectionRawMutex, Vec<WifiScanResult>> = Signal::new();

/// Trigger a scan and wait up to 12 s for the result. Returns the
/// `WifiScanResponse` JSON the SPA's WiFi tab expects.
pub async fn request_scan() -> String {
    SCAN_RESULT.reset();
    SCAN_REQ.signal(());
    let networks = match select(
        SCAN_RESULT.wait(),
        Timer::after(Duration::from_secs(12)),
    )
    .await
    {
        Either::First(v) => v,
        Either::Second(()) => Vec::new(),
    };
    serde_json::to_string(&WifiScanResponse { networks }).unwrap_or_default()
}
