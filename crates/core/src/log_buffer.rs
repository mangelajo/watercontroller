//! Bounded ring buffer of recent log records, plus a `log::Log` impl that
//! pushes into it. The web UI's WebSocket log viewer and the telnet log
//! server both subscribe to the same buffer via `subscribe()`.
//!
//! Subscribers receive new records via a bounded MPSC channel. If a subscriber
//! falls behind the channel will fill up; further sends are dropped (lossy by
//! design — we never block the logger).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogRecord {
    pub monotonic_ms: u64,
    pub level: u8, // 0=Off, 1=Error, 2=Warn, 3=Info, 4=Debug, 5=Trace
    pub target: String,
    pub message: String,
}

impl LogRecord {
    pub fn formatted(&self) -> String {
        let lvl = match self.level {
            1 => "ERROR",
            2 => "WARN ",
            3 => "INFO ",
            4 => "DEBUG",
            5 => "TRACE",
            _ => "?    ",
        };
        format!(
            "[{:>10}ms] {} {}: {}",
            self.monotonic_ms, lvl, self.target, self.message
        )
    }
}

type Sender = std::sync::mpsc::SyncSender<LogRecord>;

pub struct LogBuffer {
    capacity: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    records: VecDeque<LogRecord>,
    next_subscriber_id: u64,
    subscribers: Vec<(u64, Sender)>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner {
                records: VecDeque::with_capacity(capacity),
                next_subscriber_id: 0,
                subscribers: Vec::new(),
            }),
        }
    }

    pub fn push(&self, rec: LogRecord) {
        let mut inner = self.inner.lock().unwrap();
        if inner.records.len() == self.capacity {
            inner.records.pop_front();
        }
        inner.records.push_back(rec.clone());
        // Best-effort fan-out. Drop the record for any subscriber whose
        // channel is full — we never block the logger.
        inner.subscribers.retain(|(_, tx)| match tx.try_send(rec.clone()) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => true, // keep alive
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    /// Returns up to `n` most recent records.
    pub fn snapshot(&self, n: usize) -> Vec<LogRecord> {
        let inner = self.inner.lock().unwrap();
        let len = inner.records.len();
        let start = len.saturating_sub(n);
        inner.records.range(start..).cloned().collect()
    }

    /// Subscribe to live records. Returns a Receiver and an opaque id; pass
    /// the id to `unsubscribe` when done. Channel capacity should be sized to
    /// the subscriber's worst-case lag (e.g. 256).
    pub fn subscribe(
        &self,
        channel_capacity: usize,
    ) -> (u64, std::sync::mpsc::Receiver<LogRecord>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(channel_capacity);
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_subscriber_id;
        inner.next_subscriber_id += 1;
        inner.subscribers.push((id, tx));
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.retain(|(i, _)| *i != id);
    }
}

/// Process-wide log buffer. Initialized once via `init_global`.
static GLOBAL: OnceLock<Arc<LogBuffer>> = OnceLock::new();

pub fn init_global(capacity: usize) -> Arc<LogBuffer> {
    GLOBAL
        .get_or_init(|| Arc::new(LogBuffer::new(capacity)))
        .clone()
}

pub fn global() -> Option<Arc<LogBuffer>> {
    GLOBAL.get().cloned()
}

/// `log::Log` implementation that records into the global buffer in addition
/// to whatever underlying logger is configured. Install it via `log::set_boxed_logger`.
pub struct BufferingLogger {
    monotonic_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl BufferingLogger {
    pub fn new<F>(monotonic_ms: F) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self { monotonic_ms: Box::new(monotonic_ms) }
    }
}

impl log::Log for BufferingLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let Some(buf) = global() else { return };
        let level = match record.level() {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 5,
        };
        buf.push(LogRecord {
            monotonic_ms: (self.monotonic_ms)(),
            level,
            target: record.target().to_string(),
            message: record.args().to_string(),
        });
    }
    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ms: u64, msg: &str) -> LogRecord {
        LogRecord {
            monotonic_ms: ms,
            level: 3,
            target: "test".into(),
            message: msg.into(),
        }
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push(rec(i, &format!("m{i}")));
        }
        let snap = buf.snapshot(10);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "m2");
        assert_eq!(snap[2].message, "m4");
    }

    #[test]
    fn subscriber_receives_new_records() {
        let buf = LogBuffer::new(10);
        let (_id, rx) = buf.subscribe(8);
        buf.push(rec(1, "hello"));
        let r = rx.recv().unwrap();
        assert_eq!(r.message, "hello");
    }

    #[test]
    fn slow_subscriber_does_not_block_pushes() {
        let buf = LogBuffer::new(10);
        let (_id, _rx) = buf.subscribe(2); // small channel
        // push more than the channel can hold; pushes must succeed regardless
        for i in 0..100 {
            buf.push(rec(i, "spam"));
        }
        assert_eq!(buf.snapshot(100).len(), 10); // ring buffer still consistent
    }

    #[test]
    fn unsubscribe_drops_sender() {
        let buf = LogBuffer::new(10);
        let (id, rx) = buf.subscribe(4);
        buf.unsubscribe(id);
        drop(rx);
        // Pushing now is fine; no panics, no leftover senders.
        buf.push(rec(1, "after-unsub"));
        assert_eq!(buf.snapshot(10).len(), 1);
    }
}
