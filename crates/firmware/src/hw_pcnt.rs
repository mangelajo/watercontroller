//! PulseCounter placeholder — milestone 5 wires the real ESP-IDF PCNT
//! peripheral. Uses a plain Mutex<u64> because Xtensa lacks native 64-bit
//! atomics (AtomicU64 is unavailable on this target).

use std::sync::Mutex;
use watercontroller_core::traits::PulseCounter;

#[derive(Default)]
pub struct PlaceholderPcnt {
    pub count: Mutex<u64>,
}

impl PulseCounter for PlaceholderPcnt {
    fn count(&self) -> u64 {
        *self.count.lock().unwrap()
    }
}
