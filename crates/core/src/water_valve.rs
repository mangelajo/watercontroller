//! Composite water-control state machine. Drives the motorized valve
//! (open motor + close motor) and the drain output.
//!
//! Reference behavior (from `ref/watercontroller_esphome.yaml`):
//!
//! Turn ON sequence (16 s total):
//!   t=0      drain off, close off, open off
//!   t=1s     open motor ON
//!   t=15s    open motor OFF
//!   t=16s    publish state=ON
//!
//! Turn OFF sequence (16 s + 5 min drain):
//!   t=0      open off
//!   t=1s     close motor ON
//!   t=15s    close motor OFF
//!   t=16s    drain ON, publish state=OFF
//!   t=316s   drain OFF
//!
//! While a sequence is in progress, further commands are ignored — overlapping
//! sequences would energize both motor directions, which is electrically unsafe.
//! The drain phase is interruptible: a turn-on issued mid-drain cuts over
//! immediately.

/// Fixed settle window applied before and after the motor pulse. Not user-
/// configurable: 1 s is enough to debounce the relay/contactor switching the
/// motor coil and is the value used in the original ESPHome YAML.
const SETTLE_MS: u64 = 1_000;

/// Runtime-configurable timing for the motorized valve. Only the two
/// user-meaningful knobs are exposed: how long the motor needs to fully
/// open/close, and how long the drain output is held afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValveTiming {
    /// Duration the open/close motor is energized (s). The reference valve
    /// takes ~14 s to mechanically cycle.
    pub motor_run_secs: u32,
    /// How long the drain output is held after a close sequence completes (s).
    /// Set to 0 to disable the drain phase entirely.
    pub drain_secs: u32,
}

impl Default for ValveTiming {
    fn default() -> Self {
        Self { motor_run_secs: 14, drain_secs: 300 }
    }
}

impl ValveTiming {
    fn motor_run_ms(&self) -> u64 { self.motor_run_secs as u64 * 1_000 }
    fn drain_hold_ms(&self) -> u64 { self.drain_secs as u64 * 1_000 }
    fn sequence_ms(&self) -> u64 { SETTLE_MS + self.motor_run_ms() + SETTLE_MS }
}

/// Total duration of a turn-on or turn-off sequence using ESPHome-default
/// timing (1 + 14 + 1 = 16 s). Kept as a public constant for tests.
pub const SEQUENCE_DURATION_MS: u64 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterState {
    Off,
    TurningOn,
    On,
    TurningOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ValveOutputs {
    pub open_motor: bool,
    pub close_motor: bool,
    pub drain: bool,
}

#[derive(Debug)]
enum Phase {
    Idle,
    TurningOn { start_ms: u64 },
    TurningOff { start_ms: u64 },
    Draining { start_ms: u64 },
}

#[derive(Debug)]
pub struct WaterValve {
    phase: Phase,
    user_state: bool,
    timing: ValveTiming,
}

impl Default for WaterValve {
    fn default() -> Self {
        Self::new()
    }
}

impl WaterValve {
    pub fn new() -> Self {
        Self::with_timing(ValveTiming::default())
    }

    pub fn with_timing(timing: ValveTiming) -> Self {
        Self { phase: Phase::Idle, user_state: false, timing }
    }

    pub fn set_timing(&mut self, timing: ValveTiming) {
        self.timing = timing;
    }

    pub fn timing(&self) -> ValveTiming {
        self.timing
    }

    pub fn restore(&mut self, on: bool) {
        self.user_state = on;
    }

    pub fn user_state(&self) -> bool {
        self.user_state
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Draining { .. })
    }

    pub fn state(&self) -> WaterState {
        match self.phase {
            Phase::TurningOn { .. } => WaterState::TurningOn,
            Phase::TurningOff { .. } => WaterState::TurningOff,
            Phase::Draining { .. } => WaterState::Off,
            Phase::Idle if self.user_state => WaterState::On,
            Phase::Idle => WaterState::Off,
        }
    }

    pub fn turn_on(&mut self, now_ms: u64) {
        match self.phase {
            Phase::Idle if !self.user_state => {
                self.phase = Phase::TurningOn { start_ms: now_ms };
            }
            Phase::Draining { .. } => {
                self.phase = Phase::TurningOn { start_ms: now_ms };
            }
            _ => {}
        }
    }

    pub fn turn_off(&mut self, now_ms: u64) {
        if let Phase::Idle = self.phase {
            if self.user_state {
                self.phase = Phase::TurningOff { start_ms: now_ms };
            }
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> ValveOutputs {
        let motor_run = self.timing.motor_run_ms();
        let sequence = self.timing.sequence_ms();
        let drain_hold = self.timing.drain_hold_ms();
        match self.phase {
            Phase::Idle => ValveOutputs::default(),
            Phase::TurningOn { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= sequence {
                    self.user_state = true;
                    self.phase = Phase::Idle;
                    ValveOutputs::default()
                } else if elapsed < SETTLE_MS {
                    ValveOutputs::default()
                } else if elapsed < SETTLE_MS + motor_run {
                    ValveOutputs { open_motor: true, ..Default::default() }
                } else {
                    ValveOutputs::default()
                }
            }
            Phase::TurningOff { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= sequence {
                    self.user_state = false;
                    if drain_hold == 0 {
                        self.phase = Phase::Idle;
                        ValveOutputs::default()
                    } else {
                        self.phase = Phase::Draining { start_ms: now_ms };
                        ValveOutputs { drain: true, ..Default::default() }
                    }
                } else if elapsed < SETTLE_MS {
                    ValveOutputs::default()
                } else if elapsed < SETTLE_MS + motor_run {
                    ValveOutputs { close_motor: true, ..Default::default() }
                } else {
                    ValveOutputs::default()
                }
            }
            Phase::Draining { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= drain_hold {
                    self.phase = Phase::Idle;
                    ValveOutputs::default()
                } else {
                    ValveOutputs { drain: true, ..Default::default() }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(valve: &mut WaterValve, ts: &[u64]) -> Vec<(u64, ValveOutputs)> {
        ts.iter().map(|&t| (t, valve.tick(t))).collect()
    }

    #[test]
    fn turn_on_sequence_matches_yaml() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        let trace = drive(&mut v, &[0, 999, 1_000, 14_999, 15_000, 15_999, 16_000]);

        assert_eq!(trace[0].1, ValveOutputs::default());
        assert_eq!(trace[1].1, ValveOutputs::default());
        assert_eq!(trace[2].1, ValveOutputs { open_motor: true, ..Default::default() });
        assert_eq!(trace[3].1, ValveOutputs { open_motor: true, ..Default::default() });
        assert_eq!(trace[4].1, ValveOutputs::default());
        assert_eq!(trace[5].1, ValveOutputs::default());
        assert_eq!(trace[6].1, ValveOutputs::default());
        assert!(v.user_state());
        assert_eq!(v.state(), WaterState::On);
    }

    #[test]
    fn turn_off_sequence_drives_close_then_drain() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        v.turn_off(20_000);

        assert_eq!(
            v.tick(21_500),
            ValveOutputs { close_motor: true, ..Default::default() }
        );
        assert_eq!(
            v.tick(34_999),
            ValveOutputs { close_motor: true, ..Default::default() }
        );

        assert_eq!(v.tick(35_500), ValveOutputs::default());

        let drain = v.tick(36_000);
        assert_eq!(drain, ValveOutputs { drain: true, ..Default::default() });
        assert!(!v.user_state());

        let drain_hold_ms = ValveTiming::default().drain_hold_ms();
        assert_eq!(
            v.tick(36_000 + drain_hold_ms - 1),
            ValveOutputs { drain: true, ..Default::default() }
        );
        assert_eq!(
            v.tick(36_000 + drain_hold_ms),
            ValveOutputs::default()
        );
    }

    #[test]
    fn set_timing_changes_subsequent_sequences() {
        let mut v = WaterValve::with_timing(ValveTiming {
            motor_run_secs: 2,
            drain_secs: 3,
        });
        v.turn_on(0);
        // SETTLE_MS=1000, motor=2000 → motor active 1000..3000, sequence ends 4000.
        assert_eq!(
            v.tick(1_500),
            ValveOutputs { open_motor: true, ..Default::default() }
        );
        assert_eq!(v.tick(4_000), ValveOutputs::default());
        assert_eq!(v.state(), WaterState::On);
    }

    #[test]
    fn drain_secs_zero_skips_draining_phase() {
        let mut v = WaterValve::with_timing(ValveTiming {
            motor_run_secs: 1,
            drain_secs: 0,
        });
        v.turn_on(0);
        v.tick(3_000); // 1 + 1 + 1 = 3 s sequence
        assert_eq!(v.state(), WaterState::On);
        v.turn_off(4_000);
        v.tick(7_000);
        assert_eq!(v.state(), WaterState::Off);
        assert_eq!(v.tick(7_001), ValveOutputs::default());
    }

    #[test]
    fn overlapping_turn_on_during_turn_off_ignored() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        assert!(v.user_state());

        v.turn_off(20_000);
        v.turn_on(25_000);
        assert_eq!(
            v.tick(25_000),
            ValveOutputs { close_motor: true, ..Default::default() }
        );
    }

    #[test]
    fn never_drives_both_motor_directions() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        for t in 0..=SEQUENCE_DURATION_MS {
            let o = v.tick(t);
            assert!(
                !(o.open_motor && o.close_motor),
                "open and close motor both driven at t={t}"
            );
        }

        v.tick(SEQUENCE_DURATION_MS);
        v.turn_off(SEQUENCE_DURATION_MS);
        for t in SEQUENCE_DURATION_MS..=2 * SEQUENCE_DURATION_MS {
            let o = v.tick(t);
            assert!(
                !(o.open_motor && o.close_motor),
                "open and close motor both driven at t={t}"
            );
        }
    }

    #[test]
    fn turn_on_during_drain_cancels_drain() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        v.turn_off(20_000);
        v.tick(36_000);
        assert_eq!(
            v.tick(40_000),
            ValveOutputs { drain: true, ..Default::default() }
        );

        v.turn_on(50_000);
        assert_eq!(
            v.tick(51_500),
            ValveOutputs { open_motor: true, ..Default::default() }
        );
    }

    #[test]
    fn turn_on_when_already_on_is_noop() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        assert!(v.user_state());

        v.turn_on(20_000);
        assert!(!v.is_busy());
        assert_eq!(v.tick(20_500), ValveOutputs::default());
    }
}
