//! In-memory log fan-out for the SPA's Logs tab and the telnet port.
//!
//! A `log::Log` implementation tees every record two ways: to the UART
//! (via `esp-println`, so the serial console is unchanged) and into a
//! `PubSubChannel` that every live log consumer drains independently.
//! The firmware's own status messages go through `log::info!`, so they
//! show up in all three places (UART, `/ws/logs`, telnet).
//!
//! A `PubSubChannel` (rather than a plain `Channel`) is what lets the
//! WebSocket streamer and the telnet server each receive *every* line:
//! a plain channel is competing-consumer, so each line would reach only
//! one of them. When a subscriber lags, the oldest lines are dropped for
//! that subscriber only — logs only need to be live while someone watches.

use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    pubsub::{PubSubChannel, Subscriber},
};
use heapless::String;

/// One formatted log line. 160 chars covers the firmware's messages;
/// anything longer is truncated by the `write!` into a bounded String.
pub type LogLine = String<160>;

/// Per-subscriber queue depth. A burst this long survives until a slow
/// consumer catches up; past that the consumer sees a `Lagged` skip.
const DEPTH: usize = 32;
/// Max simultaneous consumers: the 4-deep web-task pool (each may be
/// serving `/ws/logs`) + the telnet server + one of margin.
const SUBS: usize = 6;
/// Publishers via `publisher()`; we only ever use `immediate_publisher`
/// (the logger is sync), which doesn't consume a slot — keep this at 1.
const PUBS: usize = 1;

static CHANNEL: PubSubChannel<CriticalSectionRawMutex, LogLine, DEPTH, SUBS, PUBS> =
    PubSubChannel::new();

/// A log-line subscriber. Each consumer gets its own copy of every line.
pub type LogSub = Subscriber<'static, CriticalSectionRawMutex, LogLine, DEPTH, SUBS, PUBS>;

/// Subscribe to the log stream. Returns `None` if all `SUBS` slots are
/// taken; the slot frees when the returned subscriber is dropped.
pub fn subscriber() -> Option<LogSub> {
    CHANNEL.subscriber().ok()
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
        // Overwrites the oldest line if a subscriber is behind; lagging
        // subscribers see a skip count rather than blocking the logger.
        CHANNEL.immediate_publisher().publish_immediate(line);
    }

    fn flush(&self) {}
}

static LOGGER: WcLogger = WcLogger;

/// Install the firmware logger. Replaces `esp_println::logger`.
pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}
