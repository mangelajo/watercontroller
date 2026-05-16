//! In-memory log fan-out for the SPA's Logs tab.
//!
//! A `log::Log` implementation tees every record two ways: to the UART
//! (via `esp-println`, so the serial console is unchanged) and into a
//! bounded channel that the `/ws/logs` WebSocket drains. The firmware's
//! own status messages go through `log::info!`, so they show up in both
//! places.
//!
//! When no WebSocket client is connected the channel fills and new
//! lines are dropped — logs only need to be live while someone watches.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use heapless::String;

/// One formatted log line. 160 chars covers the firmware's messages;
/// anything longer is truncated by the `write!` into a bounded String.
pub type LogLine = String<160>;

/// Capacity sized so a brief burst survives until the WebSocket task
/// drains it; ~7.7 KiB of static RAM.
const DEPTH: usize = 48;

static CHANNEL: Channel<CriticalSectionRawMutex, LogLine, DEPTH> = Channel::new();

/// The shared log channel — the WebSocket streamer receives from it.
pub fn channel() -> &'static Channel<CriticalSectionRawMutex, LogLine, DEPTH> {
    &CHANNEL
}

struct WcLogger;

impl log::Log for WcLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        use core::fmt::Write as _;
        let mut line: LogLine = String::new();
        // Truncation on overflow is fine — `write!` keeps what fit.
        let _ = write!(line, "{} {}", record.level(), record.args());
        esp_println::println!("{}", line);
        // Drop when the channel is full (no client draining).
        let _ = CHANNEL.try_send(line);
    }

    fn flush(&self) {}
}

static LOGGER: WcLogger = WcLogger;

/// Install the firmware logger. Replaces `esp_println::logger`.
pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}
