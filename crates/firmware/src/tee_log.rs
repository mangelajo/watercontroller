//! Tee logger that mirrors `log` records to UART (via `eprintln!` which lands
//! on ESP-IDF's default UART) **and** into the in-memory ring buffer.
//!
//! Replaces the default `EspLogger` install so we get both outputs from a
//! single global logger.

use log::{Level, LevelFilter, Log, Metadata, Record};
use watercontroller_core::log_buffer;

pub struct TeeLogger;

impl Log for TeeLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let lvl_short = match record.level() {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
            Level::Trace => "T",
        };
        // UART line — kept compact since boot logs are noisy.
        eprintln!(
            "{} {} {}: {}",
            now_ms(),
            lvl_short,
            record.target(),
            record.args()
        );
        if let Some(buf) = log_buffer::global() {
            buf.push(log_buffer::LogRecord {
                monotonic_ms: now_ms(),
                level: match record.level() {
                    Level::Error => 1,
                    Level::Warn => 2,
                    Level::Info => 3,
                    Level::Debug => 4,
                    Level::Trace => 5,
                },
                target: record.target().to_string(),
                message: record.args().to_string(),
            });
        }
    }

    fn flush(&self) {}
}

fn now_ms() -> u64 {
    unsafe { esp_idf_svc::sys::esp_timer_get_time() as u64 / 1000 }
}

pub fn install(level: LevelFilter) {
    static LOGGER: TeeLogger = TeeLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}
