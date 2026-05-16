//! Cron-schedule evaluator task.
//!
//! Wires `watercontroller_core`'s schedule engine into the firmware:
//! once every 30 s it converts the SNTP wall clock to local time, asks
//! `Schedule::evaluate_range` which rules fell due since the last check,
//! and fires each one. `evaluate_range` over a `(last, now]` window
//! recovers missed minutes (e.g. an SNTP step) without double-firing.
//!
//! Mirrors the IDF firmware's `spawn_schedule_task`.

use core::sync::atomic::Ordering;

use embassy_time::{Duration, Timer};
use watercontroller_core::{api::SwitchCommand, app::App, schedule::Action};

#[embassy_executor::task]
pub async fn schedule_task(app: App) {
    // Evaluating against the compile-time epoch baseline would fire (or
    // miss) rules at the wrong wall-clock time — wait for the first
    // SNTP sync before starting the window.
    while crate::sntp::EPOCH_AT_BOOT.load(Ordering::Relaxed) == 0 {
        Timer::after(Duration::from_secs(5)).await;
    }

    let now_utc = app.clock().now();
    let mut last_local =
        watercontroller_core::schedule::to_local(now_utc, &app.config().timezone);
    {
        let cfg = app.config();
        let active = cfg.schedule.rules.iter().filter(|r| r.enabled).count();
        log::info!("schedule: evaluator started, {} active rule(s)", active);
    }

    loop {
        Timer::after(Duration::from_secs(30)).await;
        let cfg = app.config();
        let now_local =
            watercontroller_core::schedule::to_local(app.clock().now(), &cfg.timezone);
        let hits = cfg.schedule.evaluate_range(last_local, now_local);
        for rule in hits {
            log::info!("schedule: fire id='{}'", rule.id);
            match &rule.action {
                Action::Switch { id } => {
                    if !app.fire_schedule_sprinkler(id, rule.duration_secs) {
                        log::warn!(
                            "schedule: rule '{}' references unknown switch '{}'",
                            rule.id,
                            id,
                        );
                    }
                }
                Action::WaterControl { on } => {
                    let _ = app.switch_command(SwitchCommand::WaterControl { on: *on });
                }
            }
        }
        last_local = now_local;
    }
}
