//! Composite water-control state machine. Drives the motorized valve (open
//! coil + close coil, both 14s pulses) and the drain valve (5 min hold).
//!
//! Reference behavior (from `ref/watercontroller_esphome.yaml`):
//!
//! Turn ON sequence (16 s total):
//!   t=0      drain off, close off, open off
//!   t=1s     open coil ON
//!   t=15s    open coil OFF (auto-off after 14 s)
//!   t=16s    publish state=ON
//!
//! Turn OFF sequence (16 s + 5 min drain):
//!   t=0      open off
//!   t=1s     close coil ON
//!   t=15s    close coil OFF (auto-off after 14 s)
//!   t=16s    drain ON, publish state=OFF
//!   t=316s   drain OFF (auto-off after 5 min)
//!
//! While a sequence is in progress, further commands are ignored — overlapping
//! sequences would energize both coils, which is electrically unsafe for a
//! motorized valve.

const PRE_DELAY_MS: u64 = 1_000;
const COIL_PULSE_MS: u64 = 14_000;
const POST_DELAY_MS: u64 = 1_000;
const DRAIN_HOLD_MS: u64 = 5 * 60 * 1_000;

/// Total duration of a turn-on or turn-off sequence (excluding drain hold).
pub const SEQUENCE_DURATION_MS: u64 = PRE_DELAY_MS + COIL_PULSE_MS + POST_DELAY_MS;

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
    pub open_coil: bool,
    pub close_coil: bool,
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
    user_state: bool, // last published state
}

impl Default for WaterValve {
    fn default() -> Self {
        Self::new()
    }
}

impl WaterValve {
    pub fn new() -> Self {
        Self { phase: Phase::Idle, user_state: false }
    }

    /// Restore from persisted state at boot. The hardware is not driven —
    /// callers should still send the device through a real sequence to bring
    /// the valve to its restored position.
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

    /// Issue an ON command. Ignored if a sequence is already in progress, or
    /// if already on (idempotent). The drain phase is interruptible — if the
    /// user re-enables water during drain, we cut over immediately.
    pub fn turn_on(&mut self, now_ms: u64) {
        match self.phase {
            Phase::Idle if !self.user_state => {
                self.phase = Phase::TurningOn { start_ms: now_ms };
            }
            Phase::Draining { .. } => {
                // Cancelling the drain to bring water back is fine; the close
                // coil is already off, the open coil is already off — a normal
                // turn-on sequence applies.
                self.phase = Phase::TurningOn { start_ms: now_ms };
            }
            _ => {}
        }
    }

    /// Issue an OFF command. Ignored if a sequence is already in progress.
    pub fn turn_off(&mut self, now_ms: u64) {
        match self.phase {
            Phase::Idle if self.user_state => {
                self.phase = Phase::TurningOff { start_ms: now_ms };
            }
            _ => {}
        }
    }

    /// Advance the state machine. Returns the GPIO output values for the
    /// current instant — caller should apply them to real pins.
    pub fn tick(&mut self, now_ms: u64) -> ValveOutputs {
        match self.phase {
            Phase::Idle => ValveOutputs::default(),
            Phase::TurningOn { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= SEQUENCE_DURATION_MS {
                    self.user_state = true;
                    self.phase = Phase::Idle;
                    ValveOutputs::default()
                } else if elapsed < PRE_DELAY_MS {
                    ValveOutputs::default()
                } else if elapsed < PRE_DELAY_MS + COIL_PULSE_MS {
                    ValveOutputs { open_coil: true, ..Default::default() }
                } else {
                    ValveOutputs::default() // POST_DELAY window
                }
            }
            Phase::TurningOff { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= SEQUENCE_DURATION_MS {
                    self.user_state = false;
                    self.phase = Phase::Draining { start_ms: now_ms };
                    ValveOutputs { drain: true, ..Default::default() }
                } else if elapsed < PRE_DELAY_MS {
                    ValveOutputs::default()
                } else if elapsed < PRE_DELAY_MS + COIL_PULSE_MS {
                    ValveOutputs { close_coil: true, ..Default::default() }
                } else {
                    ValveOutputs::default() // POST_DELAY window
                }
            }
            Phase::Draining { start_ms } => {
                let elapsed = now_ms.saturating_sub(start_ms);
                if elapsed >= DRAIN_HOLD_MS {
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

        // t=0..999: pre-delay, all off
        assert_eq!(trace[0].1, ValveOutputs::default());
        assert_eq!(trace[1].1, ValveOutputs::default());

        // t=1000..14999: open coil energized
        assert_eq!(trace[2].1, ValveOutputs { open_coil: true, ..Default::default() });
        assert_eq!(trace[3].1, ValveOutputs { open_coil: true, ..Default::default() });

        // t=15000..15999: post-delay, all off
        assert_eq!(trace[4].1, ValveOutputs::default());
        assert_eq!(trace[5].1, ValveOutputs::default());

        // t=16000: sequence done, user_state flipped
        assert_eq!(trace[6].1, ValveOutputs::default());
        assert!(v.user_state());
        assert_eq!(v.state(), WaterState::On);
    }

    #[test]
    fn turn_off_sequence_drives_close_then_drain() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS); // complete turn-on
        v.turn_off(20_000);

        // close coil active 21000..35000
        assert_eq!(
            v.tick(21_500),
            ValveOutputs { close_coil: true, ..Default::default() }
        );
        assert_eq!(
            v.tick(34_999),
            ValveOutputs { close_coil: true, ..Default::default() }
        );

        // post-delay 35000..36000: all off
        assert_eq!(v.tick(35_500), ValveOutputs::default());

        // t=36000 enters drain phase
        let drain = v.tick(36_000);
        assert_eq!(drain, ValveOutputs { drain: true, ..Default::default() });
        assert!(!v.user_state());

        // drain holds for 5 min from drain start (36000)
        assert_eq!(
            v.tick(36_000 + DRAIN_HOLD_MS - 1),
            ValveOutputs { drain: true, ..Default::default() }
        );
        assert_eq!(
            v.tick(36_000 + DRAIN_HOLD_MS),
            ValveOutputs::default()
        );
    }

    #[test]
    fn overlapping_turn_on_during_turn_off_ignored() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        assert!(v.user_state());

        v.turn_off(20_000);
        // mid turn-off: try to turn on → must be ignored (close coil engaged)
        v.turn_on(25_000);
        assert_eq!(
            v.tick(25_000),
            ValveOutputs { close_coil: true, ..Default::default() }
        );
    }

    #[test]
    fn never_energizes_both_coils() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        for t in 0..=SEQUENCE_DURATION_MS {
            let o = v.tick(t);
            assert!(
                !(o.open_coil && o.close_coil),
                "open and close coils both energized at t={t}"
            );
        }

        v.tick(SEQUENCE_DURATION_MS);
        v.turn_off(SEQUENCE_DURATION_MS);
        for t in SEQUENCE_DURATION_MS..=2 * SEQUENCE_DURATION_MS {
            let o = v.tick(t);
            assert!(
                !(o.open_coil && o.close_coil),
                "open and close coils both energized at t={t}"
            );
        }
    }

    #[test]
    fn turn_on_during_drain_cancels_drain() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        v.turn_off(20_000);
        v.tick(36_000); // entering drain
        assert_eq!(
            v.tick(40_000),
            ValveOutputs { drain: true, ..Default::default() }
        );

        // user changes mind — re-enable water mid-drain
        v.turn_on(50_000);
        // should now be in turn-on sequence (drain canceled)
        assert_eq!(
            v.tick(51_500),
            ValveOutputs { open_coil: true, ..Default::default() }
        );
    }

    #[test]
    fn turn_on_when_already_on_is_noop() {
        let mut v = WaterValve::new();
        v.turn_on(0);
        v.tick(SEQUENCE_DURATION_MS);
        assert!(v.user_state());

        v.turn_on(20_000); // already on — should not start a new sequence
        assert!(!v.is_busy());
        assert_eq!(v.tick(20_500), ValveOutputs::default());
    }
}
