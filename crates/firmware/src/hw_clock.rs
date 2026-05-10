//! Clock implementation for ESP-IDF: monotonic via `esp_timer_get_time`,
//! wall-clock via `SystemTime` (which is set by SNTP once that lands).

use chrono::{DateTime, Utc};
use watercontroller_core::traits::Clock;

pub struct EspClock;

impl Clock for EspClock {
    fn now(&self) -> DateTime<Utc> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        DateTime::from_timestamp(secs, 0).unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
    }
    fn monotonic_ms(&self) -> u64 {
        // esp_timer_get_time returns microseconds since boot.
        unsafe { esp_idf_svc::sys::esp_timer_get_time() as u64 / 1000 }
    }
}
