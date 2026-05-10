//! HTTP-push OTA, backed by `esp-idf-svc::ota::EspOta`.
//!
//! POST /api/ota with the raw firmware binary in the body. The handler
//! streams the body straight into the inactive OTA partition; on EOF it
//! finishes + activates the new image and reboots. ESP-IDF's bootloader
//! handles A/B partition selection on the next boot.
//!
//! Rollback is enabled in sdkconfig.defaults
//! (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`). After a successful boot the
//! firmware MUST call [`mark_app_valid`] once it's confident in itself
//! (e.g. WiFi up, HTTPD responding) — otherwise the next reboot will
//! revert to the previous slot.

use anyhow::{anyhow, Result};
use esp_idf_svc::ota::EspOta;
use esp_idf_svc::sys::esp_restart;

/// Mark the currently-running app as valid so the bootloader doesn't roll
/// back on the next reboot. Call once the device has proven itself
/// (WiFi up, HTTPD listening). Idempotent — safe to call repeatedly.
pub fn mark_app_valid() {
    let mut ota = match EspOta::new() {
        Ok(o) => o,
        Err(e) => {
            log::warn!("ota: cannot open partition table: {e:?}");
            return;
        }
    };
    if let Err(e) = ota.mark_running_slot_valid() {
        log::warn!("ota: mark_running_slot_valid failed: {e:?}");
    } else {
        log::debug!("ota: running slot marked valid (rollback armed)");
    }
}

/// Apply a firmware image streamed in chunks. The reader is called
/// repeatedly; each non-empty buffer is written into the next partition.
/// Returns Ok once the whole image has been written and committed; the
/// caller is expected to schedule a reboot afterwards.
pub fn apply_image<R: FnMut(&mut [u8]) -> std::io::Result<usize>>(
    mut read: R,
) -> Result<usize> {
    let mut ota = EspOta::new()?;
    let mut update = ota.initiate_update()?;
    let mut total = 0usize;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = read(&mut buf).map_err(|e| anyhow!("ota read: {e}"))?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n])?;
        total += n;
    }
    if total == 0 {
        return Err(anyhow!("empty firmware image"));
    }
    update.complete()?;
    log::info!("ota: applied {total} bytes; rebooting into the new slot");
    Ok(total)
}

/// Trigger a soft reboot. Used after a successful OTA so the bootloader
/// picks the freshly-flashed slot.
pub fn reboot() -> ! {
    unsafe { esp_restart() }
}
