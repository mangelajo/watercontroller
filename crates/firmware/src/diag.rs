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
    /// Breakdown by memory class. Internal DRAM is the scarce one
    /// (~290 KiB total); PSRAM is much larger but can't hold task
    /// stacks (FreeRTOS limitation) or DMA buffers (32-bit access
    /// only). Fragmentation in `internal_largest_free` is what
    /// triggers ENOMEM on `spawn_named` even when total_free shows
    /// 4 MB available.
    pub internal_free_bytes: usize,
    pub internal_largest_free: usize,
    pub internal_min_ever_free: usize,
    pub psram_free_bytes: usize,
    pub psram_largest_free: usize,
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
    let mut total = MaybeUninit::<multi_heap_info_t>::zeroed();
    let mut internal = MaybeUninit::<multi_heap_info_t>::zeroed();
    let mut spiram = MaybeUninit::<multi_heap_info_t>::zeroed();
    unsafe {
        heap_caps_get_info(total.as_mut_ptr(), MALLOC_CAP_8BIT);
        heap_caps_get_info(internal.as_mut_ptr(), MALLOC_CAP_INTERNAL);
        heap_caps_get_info(spiram.as_mut_ptr(), MALLOC_CAP_SPIRAM);
        let total = total.assume_init();
        let internal = internal.assume_init();
        let spiram = spiram.assume_init();
        HeapInfo {
            total_free_bytes: total.total_free_bytes,
            total_allocated_bytes: total.total_allocated_bytes,
            largest_free_block: total.largest_free_block,
            min_ever_free_bytes: total.minimum_free_bytes,
            internal_free_bytes: internal.total_free_bytes,
            internal_largest_free: internal.largest_free_block,
            internal_min_ever_free: internal.minimum_free_bytes,
            psram_free_bytes: spiram.total_free_bytes,
            psram_largest_free: spiram.largest_free_block,
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
