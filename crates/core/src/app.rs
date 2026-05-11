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
    // Wrapped in `Arc` so `config()` returns a cheap refcount-bump
    // (~16 B stack, no heap traversal) instead of cloning the entire
    // Config — which contains the HTTPS PEM cert + key, MQTT TLS
    // material, schedule rules, etc. and runs ~6–10 KiB. Callers that
    // need to mutate explicitly `(*app.config()).clone()` first.
    config: Mutex<Arc<Config>>,
    valve: Mutex<WaterValve>,
    sprinkler1: Mutex<TimedSwitch>,
    sprinkler2: Mutex<TimedSwitch>,
    nvs: Option<Arc<dyn NvsStore>>,
    last_persisted_valve: Mutex<Option<bool>>,
    /// Timestamp (clock.monotonic_ms) of the most recent flow-alarm
    /// evaluation. Used to compute the elapsed-time delta on each tick
    /// — tick rate isn't fixed (firmware ticks faster during sequencer
    /// transitions, slower at idle), so we count real time rather than
    /// ticks.
    flow_alarm_last_check_ms: Mutex<Option<u64>>,
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
        // Auto-off durations come from runtime config. 0 = no auto-off.
        let auto_off_or_none = |secs: u32| {
            if secs == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(secs as u64))
            }
        };
        let s1 = TimedSwitch::new(auto_off_or_none(config.switches.sprinkler_1_auto_off_secs));
        let s2 = TimedSwitch::new(auto_off_or_none(config.switches.sprinkler_2_auto_off_secs));

        // Restore last-known valve state from NVS, if available.
        let mut valve = WaterValve::with_timing(config.switches.valve_timing);
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
                config: Mutex::new(Arc::new(config)),
                valve: Mutex::new(valve),
                sprinkler1: Mutex::new(s1),
                sprinkler2: Mutex::new(s2),
                nvs,
                last_persisted_valve: Mutex::new(restored_state),
                flow_alarm_last_check_ms: Mutex::new(None),
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

    /// Snapshot of the current config. Returns an `Arc<Config>` — readers
    /// pay only a refcount bump, not a full clone of the (kilobyte-sized)
    /// Config struct. To mutate, do `let mut cfg = (*app.config()).clone();
    /// cfg.x = …; app.replace_config(cfg);` so the clone is explicit at
    /// the callsite that needs it.
    pub fn config(&self) -> Arc<Config> {
        self.inner.config.lock().unwrap().clone()
    }

    /// Replace the in-memory config and push live-tunable values into the
    /// running components. Currently: sprinkler auto-off durations on the
    /// `TimedSwitch`es. Persistence to NVS is the caller's responsibility.
    pub fn replace_config(&self, cfg: Config) {
        let auto_off_or_none = |secs: u32| {
            if secs == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(secs as u64))
            }
        };
        self.inner.sprinkler1.lock().unwrap()
            .set_auto_off(auto_off_or_none(cfg.switches.sprinkler_1_auto_off_secs));
        self.inner.sprinkler2.lock().unwrap()
            .set_auto_off(auto_off_or_none(cfg.switches.sprinkler_2_auto_off_secs));
        self.inner.valve.lock().unwrap().set_timing(cfg.switches.valve_timing);
        *self.inner.config.lock().unwrap() = Arc::new(cfg);
    }

    /// Fire a scheduled sprinkler activation with an optional per-run
    /// duration override (seconds). `None` falls back to the configured
    /// manual auto-off on the switch. Returns `false` if `id` is unknown.
    pub fn fire_schedule_sprinkler(&self, id: &str, duration_secs: Option<u32>) -> bool {
        let now = self.inner.clock.monotonic_ms();
        let lock = match id {
            "sprinkler_1" => &self.inner.sprinkler1,
            "sprinkler_2" => &self.inner.sprinkler2,
            _ => return false,
        };
        let mut s = lock.lock().unwrap();
        match duration_secs {
            Some(d) => s.turn_on_for(now, std::time::Duration::from_secs(d as u64)),
            None => s.turn_on(now),
        }
        match duration_secs {
            Some(d) => log::info!("{id}: ON (schedule, duration {d}s)"),
            None => log::info!("{id}: ON (schedule)"),
        }
        true
    }

    /// Apply a switch command. Returns `Busy` if the water valve is
    /// mid-sequence (motor energized in either direction).
    pub fn switch_command(&self, cmd: SwitchCommand) -> CommandOutcome {
        let now = self.inner.clock.monotonic_ms();
        match cmd {
            SwitchCommand::Sprinkler1 { on } => {
                let mut s = self.inner.sprinkler1.lock().unwrap();
                let was_on = s.is_on();
                if on { s.turn_on(now); } else { s.turn_off(now); }
                if was_on != on {
                    log::info!("sprinkler_1: {} (manual)", if on { "ON" } else { "off" });
                }
                CommandOutcome::Ok
            }
            SwitchCommand::Sprinkler2 { on } => {
                let mut s = self.inner.sprinkler2.lock().unwrap();
                let was_on = s.is_on();
                if on { s.turn_on(now); } else { s.turn_off(now); }
                if was_on != on {
                    log::info!("sprinkler_2: {} (manual)", if on { "ON" } else { "off" });
                }
                CommandOutcome::Ok
            }
            SwitchCommand::WaterControl { on } => {
                let mut v = self.inner.valve.lock().unwrap();
                if v.is_busy() {
                    log::warn!(
                        "water_control: refused {} — valve in mid-sequence",
                        if on { "on" } else { "off" }
                    );
                    return CommandOutcome::Busy {
                        reason: "valve sequence in progress".into(),
                    };
                }
                log::info!(
                    "water_control: {} sequence starting",
                    if on { "open" } else { "close" }
                );
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

        // Drive timers under per-component locks. The TimedSwitch ticks
        // tell us whether *this* tick was the one that fired auto-off, so
        // we can log it without false-positives from manual `turn_off`.
        let s1_auto_off_fired = self.inner.sprinkler1.lock().unwrap().tick(now);
        let s2_auto_off_fired = self.inner.sprinkler2.lock().unwrap().tick(now);
        let valve_outputs = self.inner.valve.lock().unwrap().tick(now);
        if s1_auto_off_fired {
            log::info!("sprinkler_1: off (auto-off after timer expiry)");
        }
        if s2_auto_off_fired {
            log::info!("sprinkler_2: off (auto-off after timer expiry)");
        }

        // Mirror state into the snapshot. Re-acquire the locks briefly to
        // minimise time held simultaneously.
        let s1_on = self.inner.sprinkler1.lock().unwrap().is_on();
        let s2_on = self.inner.sprinkler2.lock().unwrap().is_on();
        let valve_state = self.inner.valve.lock().unwrap().state();

        self.inner.state.update(|s| {
            // Water control transitions are logged on every state change
            // because they're driven by the valve's internal sequencer
            // (open/close coil timing), not by an explicit user op.
            let new_wc = WaterControlState::from(valve_state);
            if s.switches.water_control != new_wc {
                log::info!(
                    "water_control: {:?} → {:?}",
                    s.switches.water_control,
                    new_wc
                );
            }
            s.switches.sprinkler_1 = s1_on;
            s.switches.sprinkler_2 = s2_on;
            s.switches.water_control = new_wc;
        });

        // Flow-rate alarm: evaluate after sprinkler/valve state was
        // mirrored above so `any_sprinkler_on` reflects this tick.
        self.evaluate_flow_alarm(now);

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

impl App {
    /// Evaluate the flow-rate alarm rule. Called from `tick()` after the
    /// sprinkler/valve snapshot is updated.
    ///
    /// Rule (per the user's spec): if `sensors.flow_lph` is ≥
    /// `config.flow_alarm.threshold_lph` for a sustained
    /// `duration_secs`, while no sprinkler is currently on, latch
    /// `alarm.active = true` and force water_control off. Sprinkler
    /// activity resets the elapsed counter — high flow during a known
    /// open zone isn't anomalous.
    ///
    /// Latched: once active, only `clear_flow_alarm()` (POST
    /// /api/alarm/clear or the serial `alarm clear` command) un-sets
    /// it. While latched and the valve has somehow re-opened, the
    /// rule keeps issuing close on every tick — best-effort retry.
    fn evaluate_flow_alarm(&self, now_ms: u64) {
        let cfg = self.config();
        // Update last-check timestamp even when disabled so a later
        // enable doesn't see a giant accumulated delta.
        let delta_s = {
            let mut last = self.inner.flow_alarm_last_check_ms.lock().unwrap();
            let d = match *last {
                Some(t) => ((now_ms.saturating_sub(t)) / 1000) as u32,
                None => 0,
            };
            *last = Some(now_ms);
            d
        };
        if !cfg.flow_alarm.enabled {
            // Reset state when disabled so re-enable starts clean.
            self.update_state(|s| {
                s.alarm.active = false;
                s.alarm.elapsed_secs = 0;
            });
            return;
        }

        let snap = self.snapshot();
        let any_sprinkler_on = snap.switches.sprinkler_1 || snap.switches.sprinkler_2;
        let flow = snap.sensors.flow_lph.unwrap_or(0.0);
        let above = flow >= cfg.flow_alarm.threshold_lph;
        let threshold = cfg.flow_alarm.threshold_lph;
        let duration = cfg.flow_alarm.duration_secs;
        let was_active = snap.alarm.active;

        let mut just_fired = false;
        self.update_state(|s| {
            s.alarm.elapsed_secs = if any_sprinkler_on || !above {
                0
            } else {
                s.alarm.elapsed_secs.saturating_add(delta_s)
            };
            if !was_active && s.alarm.elapsed_secs >= duration {
                s.alarm.active = true;
                just_fired = true;
            }
        });

        if just_fired {
            log::error!(
                "flow alarm FIRED: flow {flow:.1} L/h ≥ {threshold:.1} L/h sustained ≥ {duration}s — closing water_control"
            );
            let _ = self.switch_command(SwitchCommand::WaterControl { on: false });
        } else if was_active
            && matches!(snap.switches.water_control, WaterControlState::On)
        {
            // Best-effort retry while alarm is latched.
            let _ = self.switch_command(SwitchCommand::WaterControl { on: false });
        }
    }

    /// User-initiated clear. Resets latched alarm + elapsed counter.
    pub fn clear_flow_alarm(&self) {
        let was_active = self.inner.state.snapshot().alarm.active;
        self.update_state(|s| {
            s.alarm.active = false;
            s.alarm.elapsed_secs = 0;
        });
        if was_active {
            log::info!("flow alarm cleared");
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
    fn replace_config_updates_sprinkler_auto_off_live() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default()); // 7 min default
        app.switch_command(SwitchCommand::Sprinkler1 { on: true });
        app.tick();
        assert!(app.snapshot().switches.sprinkler_1);

        // Live-tighten auto-off to 2 minutes.
        let mut cfg = (*app.config()).clone();
        cfg.switches.sprinkler_1_auto_off_secs = 120;
        app.replace_config(cfg);

        // 3 min in (past the new 2 min window): tick should auto-off.
        clock.advance(3 * 60_000);
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

    // ---------------- flow alarm ----------------

    /// Helper: configure flow alarm with a chosen threshold/duration and
    /// publish a fake flow_lph reading via `update_state`.
    fn arm_alarm(app: &App, threshold: f32, duration_secs: u32) {
        let mut cfg = (*app.config()).clone();
        cfg.flow_alarm = crate::config::FlowAlarmConfig {
            enabled: true,
            threshold_lph: threshold,
            duration_secs,
        };
        app.replace_config(cfg);
    }

    fn set_flow(app: &App, lph: f32) {
        app.update_state(|s| s.sensors.flow_lph = Some(lph));
    }

    #[test]
    fn flow_alarm_fires_after_sustained_high_flow() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        arm_alarm(&app, 50.0, 10);
        set_flow(&app, 200.0); // well above threshold

        // First tick establishes the baseline (delta = 0).
        app.tick();
        assert!(!app.snapshot().alarm.active);

        // 5 s — not yet over duration.
        clock.advance(5_000);
        app.tick();
        assert!(!app.snapshot().alarm.active);
        assert_eq!(app.snapshot().alarm.elapsed_secs, 5);

        // Another 6 s — total 11 s ≥ 10 s threshold → fire.
        clock.advance(6_000);
        app.tick();
        let s = app.snapshot();
        assert!(s.alarm.active, "alarm should have fired by now");
        assert!(s.alarm.elapsed_secs >= 10);
    }

    #[test]
    fn flow_alarm_ignored_while_sprinkler_on() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        arm_alarm(&app, 50.0, 5);
        set_flow(&app, 200.0);

        // Turn a sprinkler on — its activity should mask any high flow.
        app.switch_command(SwitchCommand::Sprinkler1 { on: true });
        app.tick(); // baseline

        for _ in 0..10 {
            clock.advance(1_000);
            app.tick();
            // Re-assert flow each tick because the snapshot mirror
            // would otherwise reset it via state.sensors not being
            // re-published. (We bypass the real sensor read path.)
            set_flow(&app, 200.0);
        }

        let s = app.snapshot();
        assert!(!s.alarm.active);
        assert_eq!(s.alarm.elapsed_secs, 0);
    }

    #[test]
    fn flow_alarm_resets_elapsed_when_flow_drops() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        arm_alarm(&app, 50.0, 30);
        set_flow(&app, 100.0);

        app.tick();
        clock.advance(10_000);
        app.tick();
        assert_eq!(app.snapshot().alarm.elapsed_secs, 10);

        // Drop below threshold — elapsed must reset.
        set_flow(&app, 5.0);
        clock.advance(1_000);
        app.tick();
        assert_eq!(app.snapshot().alarm.elapsed_secs, 0);
    }

    #[test]
    fn flow_alarm_latches_until_cleared() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        arm_alarm(&app, 50.0, 5);
        set_flow(&app, 100.0);
        app.tick();
        clock.advance(6_000);
        app.tick();
        assert!(app.snapshot().alarm.active);

        // Flow drops to zero — alarm stays latched.
        set_flow(&app, 0.0);
        for _ in 0..10 {
            clock.advance(1_000);
            app.tick();
        }
        assert!(app.snapshot().alarm.active);

        // Explicit clear resets both flags.
        app.clear_flow_alarm();
        let s = app.snapshot();
        assert!(!s.alarm.active);
        assert_eq!(s.alarm.elapsed_secs, 0);
    }

    #[test]
    fn flow_alarm_disabled_resets_state() {
        let clock = Arc::new(TestClock::new());
        let app = App::new(clock.clone(), Config::default());
        arm_alarm(&app, 10.0, 1);
        set_flow(&app, 200.0);
        app.tick();
        clock.advance(2_000);
        app.tick();
        assert!(app.snapshot().alarm.active);

        // Disable — state must clear without needing a manual clear.
        let mut cfg = (*app.config()).clone();
        cfg.flow_alarm.enabled = false;
        app.replace_config(cfg);
        app.tick();
        assert!(!app.snapshot().alarm.active);
        assert_eq!(app.snapshot().alarm.elapsed_secs, 0);
    }
}
