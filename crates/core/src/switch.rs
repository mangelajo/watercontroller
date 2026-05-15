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
    /// One-shot override applied to the current activation only.
    /// `Some(d)` replaces `auto_off` until the switch is turned off.
    override_auto_off: Option<Duration>,
}

impl TimedSwitch {
    pub fn new(auto_off: Option<Duration>) -> Self {
        Self { on: false, on_since: None, auto_off, override_auto_off: None }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn set_auto_off(&mut self, auto_off: Option<Duration>) {
        self.auto_off = auto_off;
    }

    pub fn turn_on(&mut self, now_ms: u64) {
        if !self.on {
            self.on = true;
            self.on_since = Some(OnSince(now_ms));
            self.override_auto_off = None;
        }
    }

    /// Turn on with an explicit auto-off duration that overrides the
    /// configured `auto_off` for this activation only. The override is
    /// cleared on `turn_off` (manual or auto). Re-issuing `turn_on_for`
    /// while already on is a no-op (matches `turn_on` semantics).
    pub fn turn_on_for(&mut self, now_ms: u64, duration: Duration) {
        if !self.on {
            self.on = true;
            self.on_since = Some(OnSince(now_ms));
            self.override_auto_off = Some(duration);
        }
    }

    pub fn turn_off(&mut self, _now_ms: u64) {
        self.on = false;
        self.on_since = None;
        self.override_auto_off = None;
    }

    /// Advance time. If an auto-off duration is configured (or overridden
    /// on this activation) and it has elapsed since the last `turn_on`,
    /// the switch flips to off. Returns `true` only when this tick was
    /// the one that fired the auto-off.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        if !self.on {
            return false;
        }
        let d = self.override_auto_off.or(self.auto_off);
        let Some(d) = d else { return false };
        let Some(OnSince(t0)) = self.on_since else { return false };
        if now_ms.saturating_sub(t0) >= d.as_millis() as u64 {
            self.turn_off(now_ms);
            return true;
        }
        false
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

    #[test]
    fn set_auto_off_shortens_live_window() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(60)));
        s.turn_on(0);
        // 4s in, shorten to 3s — next tick should auto-off.
        s.tick(4_000);
        assert!(s.is_on());
        s.set_auto_off(Some(Duration::from_secs(3)));
        s.tick(4_001);
        assert!(!s.is_on());
    }

    #[test]
    fn turn_on_for_overrides_configured_auto_off() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(600))); // 10 min
        s.turn_on_for(0, Duration::from_secs(5));
        assert!(s.is_on());
        s.tick(4_999);
        assert!(s.is_on());
        s.tick(5_000);
        assert!(!s.is_on());
    }

    #[test]
    fn override_cleared_on_subsequent_manual_turn_on() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(10)));
        s.turn_on_for(0, Duration::from_secs(2));
        s.tick(2_000); // auto-off via override
        assert!(!s.is_on());
        // Manual turn-on now: configured 10 s auto-off applies, not the prior override.
        s.turn_on(3_000);
        s.tick(5_000); // 2 s in, still on
        assert!(s.is_on());
        s.tick(13_000);
        assert!(!s.is_on());
    }

    #[test]
    fn set_auto_off_to_none_keeps_switch_on() {
        let mut s = TimedSwitch::new(Some(Duration::from_secs(5)));
        s.turn_on(0);
        s.set_auto_off(None);
        s.tick(60_000);
        assert!(s.is_on());
    }
}
