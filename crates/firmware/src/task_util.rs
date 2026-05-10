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
    let h = std::thread::Builder::new()
        .name(name.to_string_lossy().into_owned())
        .spawn(f)
        .ok();
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
