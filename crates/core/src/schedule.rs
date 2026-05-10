//! Cron-like schedule for switch / water-control automation.
//!
//! Each rule fires at a set of `(hour, minute)` combinations on the chosen
//! days of the week. Times are interpreted as **local time** — the caller is
//! responsible for converting UTC → local using the configured timezone
//! before evaluation.
//!
//! `evaluate_range` reports any rule whose firing minute falls in the
//! interval `(last, now]`. This recovers cleanly from missed minutes
//! (e.g. an SNTP sync that jumps the clock forward), and avoids double-firing
//! across normal minute ticks because the interval is exclusive on the left.

use chrono::{Datelike, NaiveDateTime, Weekday};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Action {
    /// Turn on a named switch (e.g. "sprinkler_1"). The switch's own auto-off
    /// timer handles duration.
    Switch { id: String },
    /// Drive the composite water control to ON or OFF.
    WaterControl { on: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    pub action: Action,
    /// Hours of day, 0..=23.
    pub hours: Vec<u8>,
    /// Minutes of the hour, 0..=59. Defaults to a single 0 (top of the hour).
    #[serde(default = "default_minutes")]
    pub minutes: Vec<u8>,
    /// Days of week the rule applies to. Empty = every day.
    /// 0=Mon ... 6=Sun (matching chrono's Weekday::num_days_from_monday).
    #[serde(default)]
    pub days_of_week: Vec<u8>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_minutes() -> Vec<u8> {
    vec![0]
}
fn default_enabled() -> bool {
    true
}

impl Rule {
    fn matches_dow(&self, w: Weekday) -> bool {
        if self.days_of_week.is_empty() {
            return true;
        }
        let idx = w.num_days_from_monday() as u8;
        self.days_of_week.contains(&idx)
    }

    fn matches_minute(&self, dt: NaiveDateTime) -> bool {
        if !self.enabled {
            return false;
        }
        let h = dt.time().hour() as u8;
        let m = dt.time().minute() as u8;
        if !self.hours.contains(&h) {
            return false;
        }
        if !self.minutes.contains(&m) {
            return false;
        }
        self.matches_dow(dt.weekday())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Schedule {
    pub rules: Vec<Rule>,
}

impl Schedule {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// Returns the rules that should fire in the interval `(last, now]`.
    /// Iterates minute-by-minute over that interval, so callers shouldn't let
    /// `now - last` get unbounded — call this once per minute under normal
    /// operation. A jump of up to ~24 h is handled gracefully (~1440 iters).
    pub fn evaluate_range(&self, last: NaiveDateTime, now: NaiveDateTime) -> Vec<&Rule> {
        if now <= last {
            return Vec::new();
        }
        // Truncate to minute boundaries for both ends.
        let mut t = floor_to_minute(last) + chrono::Duration::minutes(1);
        let end = floor_to_minute(now);
        let mut hits: Vec<&Rule> = Vec::new();
        // Cap iteration at 25 h to defend against extreme jumps.
        let max_iters = 25 * 60;
        let mut iters = 0;
        while t <= end && iters < max_iters {
            for r in &self.rules {
                if r.matches_minute(t) {
                    hits.push(r);
                }
            }
            t += chrono::Duration::minutes(1);
            iters += 1;
        }
        hits
    }

    /// Convenience: rules that fire at `now`. Equivalent to evaluating the
    /// 1-minute window ending at `now`.
    pub fn evaluate_minute(&self, now: NaiveDateTime) -> Vec<&Rule> {
        let last = floor_to_minute(now) - chrono::Duration::seconds(1);
        self.evaluate_range(last, now)
    }
}

fn floor_to_minute(dt: NaiveDateTime) -> NaiveDateTime {
    dt.with_second(0).unwrap().with_nanosecond(0).unwrap()
}

use chrono::Timelike;

/// The default schedule restored from the YAML's commented-out `on_time` block:
/// at 10:00 and 22:00 turn on sprinkler_1; at 10:30 and 22:30 turn on sprinkler_2.
pub fn default_schedule() -> Schedule {
    Schedule::new(vec![
        Rule {
            id: "riego_exterior".into(),
            action: Action::Switch { id: "sprinkler_1".into() },
            hours: vec![10, 22],
            minutes: vec![0],
            days_of_week: vec![],
            enabled: true,
        },
        Rule {
            id: "riego_interior".into(),
            action: Action::Switch { id: "sprinkler_2".into() },
            hours: vec![10, 22],
            minutes: vec![30],
            days_of_week: vec![],
            enabled: true,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, min, 0).unwrap())
    }

    #[test]
    fn yaml_default_fires_at_10_and_22() {
        let s = default_schedule();
        let last = dt(2026, 5, 10, 9, 59);
        let now = dt(2026, 5, 10, 10, 0);
        let hits = s.evaluate_range(last, now);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "riego_exterior");
    }

    #[test]
    fn missed_minutes_are_recovered() {
        let s = default_schedule();
        // Clock jumped from 09:55 to 10:31 (e.g. SNTP sync).
        let hits = s.evaluate_range(dt(2026, 5, 10, 9, 55), dt(2026, 5, 10, 10, 31));
        let ids: Vec<_> = hits.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["riego_exterior", "riego_interior"]);
    }

    #[test]
    fn no_double_fire_at_minute_boundary() {
        let s = default_schedule();
        // First call covers up through 10:00.
        let h1 = s.evaluate_range(dt(2026, 5, 10, 9, 59), dt(2026, 5, 10, 10, 0));
        // Second call uses 10:00 as `last` — must NOT fire 10:00 again.
        let h2 = s.evaluate_range(dt(2026, 5, 10, 10, 0), dt(2026, 5, 10, 10, 1));
        assert_eq!(h1.len(), 1);
        assert_eq!(h2.len(), 0);
    }

    #[test]
    fn day_of_week_filter() {
        let mut s = default_schedule();
        s.rules[0].days_of_week = vec![0, 1, 2, 3, 4]; // Mon-Fri only
        let weekday = dt(2026, 5, 11, 10, 0); // 2026-05-11 is a Monday
        let weekend = dt(2026, 5, 9, 10, 0); // Saturday
        assert_eq!(s.evaluate_minute(weekday).len(), 1);
        assert_eq!(s.evaluate_minute(weekend).len(), 0);
    }

    #[test]
    fn disabled_rules_dont_fire() {
        let mut s = default_schedule();
        s.rules[0].enabled = false;
        let hits = s.evaluate_minute(dt(2026, 5, 10, 10, 0));
        assert!(hits.is_empty());
    }

    #[test]
    fn extreme_jump_is_capped_safely() {
        // 5-day jump should not iterate forever; cap at 25h means we still
        // catch some firings but not all 5 days' worth.
        let s = default_schedule();
        let hits = s.evaluate_range(dt(2026, 5, 10, 0, 0), dt(2026, 5, 15, 23, 59));
        // We'd hit each of riego_exterior (10:00, 22:00) and riego_interior
        // (10:30, 22:30) up to ~25h after the start.
        assert!(hits.len() <= 5);
    }
}
