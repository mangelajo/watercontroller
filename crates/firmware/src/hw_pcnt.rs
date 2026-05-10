//! PulseCounter trait impl on top of esp-idf-hal's PCNT v5 API.
//!
//! ESP32 PCNT counts in i16; we extend to u64 in software by polling on
//! every `count()` call and folding the (likely small) delta into a
//! Mutex<i64> accumulator. Xtensa lacks native AtomicI64, so a Mutex is
//! the simplest correct option — pulse rates are far below contention
//! thresholds.

use esp_idf_hal::gpio::AnyInputPin;
use esp_idf_hal::pcnt::{
    Pcnt, PcntChannel, PcntChannelConfig, PcntControlMode, PcntCountMode, PcntDriver, PinIndex,
};
use esp_idf_hal::peripheral::Peripheral;
use std::sync::Mutex;
use watercontroller_core::traits::PulseCounter;

/// Always-zero placeholder used under `--features qemu`, where the legacy
/// PCNT driver dereferences a null pointer during init.
#[derive(Default)]
pub struct PlaceholderPcnt;
impl PulseCounter for PlaceholderPcnt {
    fn count(&self) -> u64 {
        0
    }
}

pub struct EspPulseCounter {
    inner: Mutex<Inner>,
}

struct Inner {
    pcnt: PcntDriver<'static>,
    last_raw: i16,
    total: i64,
}

impl EspPulseCounter {
    pub fn new<U>(unit: U, pin: AnyInputPin) -> anyhow::Result<Self>
    where
        U: Peripheral + 'static,
        U::P: Pcnt,
    {
        let mut pcnt = PcntDriver::new(
            unit,
            Some(pin),
            None::<AnyInputPin>,
            None::<AnyInputPin>,
            None::<AnyInputPin>,
        )?;
        pcnt.channel_config(
            PcntChannel::Channel0,
            PinIndex::Pin0,
            PinIndex::Pin1,
            &PcntChannelConfig {
                lctrl_mode: PcntControlMode::Keep,
                hctrl_mode: PcntControlMode::Keep,
                pos_mode: PcntCountMode::Increment,
                neg_mode: PcntCountMode::Hold,
                counter_h_lim: i16::MAX,
                counter_l_lim: i16::MIN,
            },
        )?;
        pcnt.counter_pause()?;
        pcnt.counter_clear()?;
        pcnt.counter_resume()?;
        Ok(Self {
            inner: Mutex::new(Inner {
                pcnt,
                last_raw: 0,
                total: 0,
            }),
        })
    }
}

impl PulseCounter for EspPulseCounter {
    fn count(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let raw = inner.pcnt.get_counter_value().unwrap_or(0);
        // Wraparound-tolerant delta. Counter increments only (neg_mode = Hold),
        // so the delta should be ≥ 0 except when the hardware wraps.
        let delta: i32 = if raw >= inner.last_raw {
            (raw as i32) - (inner.last_raw as i32)
        } else {
            // Wrap: counter went from large positive to small (or negative).
            (i16::MAX as i32) - (inner.last_raw as i32) + (raw as i32) + 1
        };
        if delta > 0 {
            inner.total = inner.total.saturating_add(delta as i64);
        }
        inner.last_raw = raw;
        inner.total.max(0) as u64
    }
}
