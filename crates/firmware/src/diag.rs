//! Runtime RAM/task diagnostics, surfaced over HTTP via `/api/diag`.
//!
//! Two pieces of information that aren't in the regular status snapshot:
//!   * heap fragmentation (largest free block ≠ total free)
//!   * per-task stack high-water marks — the minimum free stack each task has
//!     ever had since boot. Tasks whose peak usage is far below their reserved
//!     size are candidates for shrinking; that RAM goes back to the heap.
//!
//! Both come from ESP-IDF / FreeRTOS APIs (`heap_caps_get_info`,
//! `uxTaskGetSystemState`). The latter requires
//! `CONFIG_FREERTOS_USE_TRACE_FACILITY=y` (set in sdkconfig.defaults).

use esp_idf_svc::sys::*;
use serde::Serialize;
use std::ffi::CStr;
use std::mem::MaybeUninit;

#[derive(Serialize)]
pub struct DiagSnapshot {
    pub heap: HeapInfo,
    pub tasks: Vec<TaskInfo>,
}

#[derive(Serialize)]
pub struct HeapInfo {
    pub total_free_bytes: usize,
    pub total_allocated_bytes: usize,
    pub largest_free_block: usize,
    pub min_ever_free_bytes: usize,
}

#[derive(Serialize)]
pub struct TaskInfo {
    pub name: String,
    pub state: &'static str,
    pub priority: u32,
    /// Lowest amount of free stack ever, in *bytes*. ESP-IDF's
    /// `uxTaskGetSystemState` populates `usStackHighWaterMark` in bytes
    /// already (verified empirically: values match configured stack
    /// sizes). The closer to 0 the closer the task came to overflowing.
    pub stack_min_free_bytes: u32,
    pub run_time: u32,
}

pub fn snapshot() -> DiagSnapshot {
    DiagSnapshot {
        heap: heap_info(),
        tasks: task_list(),
    }
}

fn heap_info() -> HeapInfo {
    let mut info = MaybeUninit::<multi_heap_info_t>::zeroed();
    unsafe {
        heap_caps_get_info(info.as_mut_ptr(), MALLOC_CAP_8BIT);
        let info = info.assume_init();
        HeapInfo {
            total_free_bytes: info.total_free_bytes,
            total_allocated_bytes: info.total_allocated_bytes,
            largest_free_block: info.largest_free_block,
            min_ever_free_bytes: info.minimum_free_bytes,
        }
    }
}

fn task_list() -> Vec<TaskInfo> {
    unsafe {
        let n = uxTaskGetNumberOfTasks() as usize;
        // Allocate slack — tasks can spawn between sample and call.
        let cap = n + 4;
        let mut buf: Vec<TaskStatus_t> = Vec::with_capacity(cap);
        let mut total_run_time: u32 = 0;
        let written =
            uxTaskGetSystemState(buf.as_mut_ptr(), cap as u32, &mut total_run_time) as usize;
        buf.set_len(written);
        buf.into_iter()
            .map(|t| TaskInfo {
                name: if t.pcTaskName.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(t.pcTaskName).to_string_lossy().into_owned()
                },
                state: state_name(t.eCurrentState),
                priority: t.uxCurrentPriority,
                stack_min_free_bytes: t.usStackHighWaterMark,
                run_time: t.ulRunTimeCounter,
            })
            .collect()
    }
}

fn state_name(s: eTaskState) -> &'static str {
    match s {
        eTaskState_eRunning => "running",
        eTaskState_eReady => "ready",
        eTaskState_eBlocked => "blocked",
        eTaskState_eSuspended => "suspended",
        eTaskState_eDeleted => "deleted",
        _ => "?",
    }
}
