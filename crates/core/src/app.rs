//! Application-level wiring shared between firmware and host. The
//! `App` struct owns the device state, the water valve, and the timed
//! switches, plus a handle to the host-provided clock. It exposes high-level
//! methods used by HTTP API handlers and the MQTT command router.
//!
//! Hardware effects (driving GPIOs) happen one level up — `tick` returns the
//! desired pin states and the caller applies them to the platform-specific
//! `GpioOut` impls.

use crate::api::{CommandOutcome, SwitchCommand};
use crate::config::Config;
use crate::state::{DeviceState, DeviceSnapshot, WaterControlState};
use crate::switch::TimedSwitch;
use crate::traits::{Clock, NvsStore};
use crate::water_valve::{ValveOutputs, WaterValve};
use std::sync::{Arc, Mutex};

/// NVS key holding the user-visible desired state of the composite water
/// control switch ("on" / "off"). Persisted on every transition so a crash
/// or power loss while the valve is mid-sequence survives reboot — at next
/// boot the App reads this and the firmware drives a recovery sequence.
const NVS_VALVE_STATE: &str = "wc.valve";

#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

struct AppInner {
    clock: Arc<dyn Clock>,
    state: DeviceState,
    config: Mutex<Config>,
    valve: Mutex<WaterValve>,
    sprinkler1: Mutex<TimedSwitch>,
    sprinkler2: Mutex<TimedSwitch>,
    nvs: Option<Arc<dyn NvsStore>>,
    last_persisted_valve: Mutex<Option<bool>>,
}

impl App {
    pub fn new(clock: Arc<dyn Clock>, config: Config) -> Self {
        Self::with_nvs(clock, config, None)
    }

    /// Variant that registers an `NvsStore` for valve state persistence. The
    /// composite water control's user-visible state (on/off) is saved on
    /// every successful transition so the firmware can restore it after a
    /// power loss / crash.
    pub fn with_nvs(
        clock: Arc<dyn Clock>,
        config: Config,
        nvs: Option<Arc<dyn NvsStore>>,
    ) -> Self {
        // Auto-off durations from the YAML: sprinkler_1 7 min, sprinkler_2 5 min.
        let s1 = TimedSwitch::new(Some(std::time::Duration::from_secs(7 * 60)));
        let s2 = TimedSwitch::new(Some(std::time::Duration::from_secs(5 * 60)));

        // Restore last-known valve state from NVS, if available.
        let mut valve = WaterValve::new();
        let restored_state = nvs
            .as_ref()
            .and_then(|n| n.get(NVS_VALVE_STATE))
            .and_then(|b| b.first().copied().map(|x| x != 0));
        if let Some(on) = restored_state {
            valve.restore(on);
        }

        Self {
            inner: Arc::new(AppInner {
                clock,
                state: DeviceState::new(),
                config: Mutex::new(config),
                valve: Mutex::new(valve),
                sprinkler1: Mutex::new(s1),
                sprinkler2: Mutex::new(s2),
                nvs,
                last_persisted_valve: Mutex::new(restored_state),
            }),
        }
    }

    /// Returns the user-visible state restored from NVS at boot, if any.
    /// `None` means there was no persisted state (fresh device or post
    /// factory_reset).
    pub fn restored_valve_state(&self) -> Option<bool> {
        *self.inner.last_persisted_valve.lock().unwrap()
    }

    pub fn clock(&self) -> &dyn Clock {
        &*self.inner.clock
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        self.inner.state.snapshot()
    }

    pub fn update_state<F: FnOnce(&mut DeviceSnapshot)>(&self, f: F) {
        self.inner.state.update(f);
    }

    pub fn config(&self) -> Config {
        self.inner.config.lock().unwrap().clone()
    }

    pub fn replace_config(&self, cfg: Config) {
        *self.inner.config.lock().unwrap() = cfg;
    }

    /// Apply a switch command. Returns `Busy` if the water valve is
    /// mid-sequence (open or close coil energized).
    pub fn switch_command(&self, cmd: SwitchCommand) -> CommandOutcome {
        let now = self.inner.clock.monotonic_ms();
        match cmd {
            SwitchCommand::Sprinkler1 { on } => {
                let mut s = self.inner.sprinkler1.lock().unwrap();
                if on { s.turn_on(now); } else { s.turn_off(now); }
                CommandOutcome::Ok
            }
            SwitchCommand::Sprinkler2 { on } => {
                let mut s = self.inner.sprinkler2.lock().unwrap();
                if on { s.turn_on(now); } else { s.turn_off(now); }
                CommandOutcome::Ok
            }
            SwitchCommand::WaterControl { on } => {
                let mut v = self.inner.valve.lock().unwrap();
                if v.is_busy() {
                    return CommandOutcome::Busy {
                        reason: "valve sequence in progress".into(),
                    };
                }
                if on { v.turn_on(now); } else { v.turn_off(now); }
                CommandOutcome::Ok
            }
        }
    }

    /// Tick all timed components and refresh the device snapshot. Returns the
    /// outputs that should be applied to physical pins. Caller is responsible
    /// for actually driving them.
    pub fn tick(&self) -> TickOutputs {
        let now = self.inner.clock.monotonic_ms();

        // Drive timers under per-component locks. Order is irrelevant.
        self.inner.sprinkler1.lock().unwrap().tick(now);
        self.inner.sprinkler2.lock().unwrap().tick(now);
        let valve_outputs = self.inner.valve.lock().unwrap().tick(now);

        // Mirror state into the snapshot. Re-acquire the locks briefly to
        // minimise time held simultaneously.
        let s1_on = self.inner.sprinkler1.lock().unwrap().is_on();
        let s2_on = self.inner.sprinkler2.lock().unwrap().is_on();
        let valve_state = self.inner.valve.lock().unwrap().state();

        self.inner.state.update(|s| {
            s.switches.sprinkler_1 = s1_on;
            s.switches.sprinkler_2 = s2_on;
            s.switches.water_control = WaterControlState::from(valve_state);
        });

        // Persist user-visible valve state to NVS on transition. We only
        // write when the value changes — avoids hot-looping NVS writes
        // during the no-op tick path.
        if let Some(nvs) = &self.inner.nvs {
            let user_on = self.inner.valve.lock().unwrap().user_state();
            let mut last = self.inner.last_persisted_valve.lock().unwrap();
            if *last != Some(user_on) {
                let byte = [u8::from(user_on)];
                if let Err(e) = nvs.set(NVS_VALVE_STATE, &byte) {
                    log::warn!("nvs persist valve state failed: {e}");
                } else {
                    *last = Some(user_on);
                }
            }
        }

        TickOutputs {
            sprinkler_1: s1_on,
            sprinkler_2: s2_on,
            valve: valve_outputs,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TickOutputs {
    pub sprinkler_1: bool,
    pub sprinkler_2: bool,
    pub valve: ValveOutputs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use chrono::{DateTime, Utc};

    struct TestClock {
        ms: AtomicU64,
    }
    impl TestClock {
        fn new() -> Self { Self { ms: AtomicU64::new(0) } }
        fn advance(&self, by_ms: u64) { self.ms.fetch_add(by_ms, Ordering::SeqCst); }
    }
    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(0, 0).unwrap()
        }
        fn monotonic_ms(&self) -> u64 { self.ms.load(Ordering::SeqCst) }
    }

    #[test]
    fn switch_command_sprinkler_round_trip() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        app.switch_command(SwitchCommand::Sprinkler1 { on: true });
        app.tick();
        assert!(app.snapshot().switches.sprinkler_1);

        // 8 minutes pass — auto-off (7 min) should fire.
        clock.advance(8 * 60_000);
        app.tick();
        assert!(!app.snapshot().switches.sprinkler_1);
    }

    #[test]
    fn water_control_cannot_be_re_issued_during_sequence() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());

        let r1 = app.switch_command(SwitchCommand::WaterControl { on: true });
        assert!(matches!(r1, CommandOutcome::Ok));
        // 5 s into the 16 s sequence — busy
        clock.advance(5_000);
        app.tick();
        let r2 = app.switch_command(SwitchCommand::WaterControl { on: false });
        assert!(matches!(r2, CommandOutcome::Busy { .. }));
    }
}
