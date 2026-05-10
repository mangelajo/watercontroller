//! Generic GPIO output with optional auto-off timer.
//!
//! Mirrors ESPHome's `on_turn_on -> delay -> turn_off` pattern. Used for
//! sprinkler outputs (auto-off after 5/7 min) and for the open/close coils of
//! the motorized valve (auto-off after 14s).

use core::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OnSince(u64);

#[derive(Debug)]
pub struct TimedSwitch {
    on: bool,
    on_since: Option<OnSince>,
    auto_off: Option<Duration>,
}

impl TimedSwitch {
    pub fn new(auto_off: Option<Duration>) -> Self {
        Self { on: false, on_since: None, auto_off }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn turn_on(&mut self, now_ms: u64) {
        if !self.on {
            self.on = true;
            self.on_since = Some(OnSince(now_ms));
        }
    }

    pub fn turn_off(&mut self, _now_ms: u64) {
        self.on = false;
        self.on_since = None;
    }

    /// Advance time. If an auto-off duration is configured and it has elapsed
    /// since the last `turn_on`, the switch flips to off.
    pub fn tick(&mut self, now_ms: u64) {
        if !self.on {
            return;
        }
        let Some(d) = self.auto_off else { return };
        let Some(OnSince(t0)) = self.on_since else { return };
        if now_ms.saturating_sub(t0) >= d.as_millis() as u64 {
            self.turn_off(now_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_off_fires_after_duration() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(5)));
        s.turn_on(0);
        assert!(s.is_on());
        s.tick(4_999);
        assert!(s.is_on());
        s.tick(5_000);
        assert!(!s.is_on());
    }

    #[test]
    fn no_auto_off_means_stays_on() {
        let mut s = TimedSwitch::new(None);
        s.turn_on(0);
        s.tick(60_000);
        assert!(s.is_on());
    }

    #[test]
    fn turn_on_resets_clock_only_when_off() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(5)));
        s.turn_on(0);
        s.turn_on(2_000); // already on — should NOT reset the clock
        s.tick(5_000);
        assert!(!s.is_on());
    }

    #[test]
    fn manual_off_before_auto_off() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(5)));
        s.turn_on(0);
        s.turn_off(1_000);
        assert!(!s.is_on());
    }
}
