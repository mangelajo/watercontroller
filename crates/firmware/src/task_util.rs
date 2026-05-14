//! Helper that names a pthread before `std::thread::spawn` creates it.
//!
//! ESP-IDF's pthread implementation maps to FreeRTOS tasks. The OS-level
//! task name (visible via `pcTaskGetName` / TaskStatus_t.pcTaskName) is
//! taken from the pthread cfg active at `pthread_create` time, NOT from
//! `std::thread::Builder::name`. Without this, every Rust thread shows up
//! as "pthread" in /api/diag, making per-task stack tuning a guessing
//! game.

use esp_idf_svc::sys::*;
use std::ffi::CStr;
use std::sync::Mutex;

/// Apply `name` + `stack_size` to the next pthread created on this CPU,
/// then run `f`. Names get truncated to FreeRTOS's 16-char `pcTaskName`.
///
/// Serialises against itself: `esp_pthread_set_cfg` is a global on the
/// current thread and must not race with another `spawn_named`.
pub fn spawn_named<F>(name: &'static CStr, stack_size: usize, f: F) -> Option<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap();
    unsafe {
        let mut cfg = esp_pthread_get_default_config();
        cfg.thread_name = name.as_ptr();
        cfg.stack_size = stack_size;
        cfg.inherit_cfg = false;
        if esp_pthread_set_cfg(&cfg) != ESP_OK {
            log::warn!("esp_pthread_set_cfg failed for {name:?}");
        }
    }
    let display_name = name.to_string_lossy().into_owned();
    let probe_name = display_name.clone();
    let result = std::thread::Builder::new()
        .name(display_name)
        // CRUCIAL: pass stack_size through std::thread::Builder too,
        // not just esp_pthread_set_cfg. Rust's std calls
        // pthread_attr_setstacksize internally with its own default
        // (~10 KiB on this target) AFTER our esp_pthread_set_cfg,
        // silently clobbering the size we wanted.
        .stack_size(stack_size)
        .spawn(move || {
            // One-shot stack-size probe at task entry. On ESP-IDF
            // (v5.3, xtensa-esp32) `uxTaskGetStackHighWaterMark`
            // returns BYTES, not StackType_t words — confirmed
            // empirically: at task entry, free ≈ stack_size for
            // every configured size. (`/api/diag`'s
            // stack_min_free_bytes field is therefore correct as-is.)
            let initial_free_bytes = unsafe {
                esp_idf_svc::sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut())
            } as usize;
            log::info!(
                "task {probe_name}: configured={stack_size}B, initial HWM free={initial_free_bytes}B"
            );
            f()
        });
    let h = match result {
        Ok(h) => Some(h),
        Err(e) => {
            log::error!("spawn_named {name:?} (stack={stack_size}) failed: {e}");
            None
        }
    };
    // Restore default cfg so subsequent pthread_creates from this thread
    // (e.g. IDF subsystems lazily spinning up worker pthreads) don't inherit
    // our custom name+stack — that's how we ended up with three tasks
    // called "schedule" all using an 8 KiB stack.
    unsafe {
        let dflt = esp_pthread_get_default_config();
        let _ = esp_pthread_set_cfg(&dflt);
    }
    h
}
