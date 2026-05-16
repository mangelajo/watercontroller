//! Over-the-air firmware update.
//!
//! `POST /api/ota` with the raw app image as the body. The handler
//! streams the body straight into the *inactive* OTA slot a 4 KiB
//! sector at a time — the ~900 KiB image never lives in the ~50 KiB
//! heap. When the whole image is written, otadata is flipped to the
//! new slot and the device reboots into it.
//!
//! The A/B slot layout (`ota_0` @ 0x20000, `ota_1` @ 0x210000) and the
//! otadata partition come from the same partitions.csv the IDF
//! firmware used; `esp-bootloader-esp-idf` reads/writes that metadata.
//!
//! Rollback: on boot, `confirm_running()` marks the live slot `Valid`.
//! If a freshly-OTA'd image panics before reaching that call, a
//! rollback-enabled bootloader reverts to the previous slot. (The
//! stock bootloader without rollback simply boots whatever otadata
//! points at — a bad image then needs a serial reflash.)

use alloc::{string::String, vec};

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read as _;
use esp_bootloader_esp_idf::{
    ota::OtaImageState,
    ota_updater::OtaUpdater,
    partitions::{
        read_partition_table, AppPartitionSubType, Error as PtError, PartitionType,
        PARTITION_TABLE_MAX_LEN,
    },
};
use esp_storage::FlashStorage;
use picoserve::{
    extract::FromRequest,
    request::{RequestBody, RequestParts},
};

use crate::AppState;

/// Flash sector size — the unit of erase + write.
const SECTOR: usize = 4096;
/// Reject anything smaller than this as obviously-not-a-firmware.
const MIN_IMAGE: usize = 64 * 1024;

/// Raised by the OTA handler once the image is safely written +
/// activated. `reboot_task` waits on it, lets the HTTP response flush,
/// then resets into the new slot.
static OTA_REBOOT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Find the OTA app slot that is *not* currently running — returns its
/// absolute flash offset and length.
fn inactive_slot(flash: &mut FlashStorage<'static>) -> Result<(u32, u32), PtError> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = read_partition_table(flash, &mut buf)?;
    let booted = pt.booted_partition()?.ok_or(PtError::Invalid)?;
    let booted_sub = match booted.partition_type() {
        PartitionType::App(s) => s,
        _ => return Err(PtError::Invalid),
    };
    for part in pt.iter() {
        if let PartitionType::App(s) = part.partition_type() {
            if s != booted_sub && s != AppPartitionSubType::Factory {
                return Ok((part.offset(), part.len()));
            }
        }
    }
    Err(PtError::Invalid)
}

/// Flip otadata to the freshly-written slot and mark it `New` so a
/// rollback-capable bootloader knows it's unconfirmed.
fn activate_inactive_slot(flash: &mut FlashStorage<'static>) -> Result<(), PtError> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(flash, &mut buf)?;
    updater.activate_next_partition()?;
    updater.set_current_ota_state(OtaImageState::New)?;
    Ok(())
}

/// Mark the running slot `Valid`. Best-effort, called once at boot:
/// confirms a just-OTA'd image so a rollback bootloader keeps it, and
/// is a harmless no-op on a slot that's already valid.
pub fn confirm_running(flash: &crate::nvs::FlashKv) {
    flash.with_flash(|f| {
        let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
        match OtaUpdater::new(f, &mut buf) {
            Ok(mut u) => {
                if let Err(e) = u.set_current_ota_state(OtaImageState::Valid) {
                    log::info!("ota: confirm_running failed: {:?}", e);
                }
            }
            Err(e) => log::info!("ota: confirm_running: no OTA metadata: {:?}", e),
        }
    });
}

/// Ask `reboot_task` to reset the device shortly. Used by OTA (after a
/// successful install) and by `POST /api/factory_reset`.
pub fn request_reboot() {
    OTA_REBOOT.signal(());
}

#[embassy_executor::task]
pub async fn reboot_task() {
    OTA_REBOOT.wait().await;
    log::info!("ota: reboot requested");
    // Let the HTTP response flush to the client before we drop the link.
    Timer::after(Duration::from_millis(800)).await;
    esp_hal::system::software_reset();
}

/// Result of an OTA attempt — serialized to JSON by the route handler.
pub struct OtaReport {
    pub ok: bool,
    pub detail: String,
}

impl OtaReport {
    fn err(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
}

impl<'r> FromRequest<'r, AppState> for OtaReport {
    // The extractor never rejects — failures are reported in-band as
    // `{ok:false}` so the caller gets a real HTTP 200 + diagnostic.
    type Rejection = core::convert::Infallible;

    async fn from_request<R: picoserve::io::Read>(
        state: &'r AppState,
        _parts: RequestParts<'r>,
        body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        Ok(write_image(state, body).await)
    }
}

async fn write_image<R: picoserve::io::Read>(
    state: &AppState,
    body: RequestBody<'_, R>,
) -> OtaReport {
    let total = body.content_length();
    if total < MIN_IMAGE {
        return OtaReport::err("image too small / no Content-Length");
    }

    let (offset, slot_len) = match state.flash_kv.with_flash(inactive_slot) {
        Ok(v) => v,
        Err(e) => return OtaReport::err(alloc::format!("no inactive OTA slot: {:?}", e)),
    };
    if total as u32 > slot_len {
        return OtaReport::err("image larger than OTA partition");
    }
    log::info!("ota: receiving {} bytes -> slot @ {:#x}", total, offset);

    let mut reader = body.reader();
    let mut sector = vec![0u8; SECTOR];
    let mut written = 0usize;
    let mut next_log = 256 * 1024;

    while written < total {
        let want = SECTOR.min(total - written);
        let mut filled = 0;
        while filled < want {
            match reader.read(&mut sector[filled..want]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => {
                    log::info!("ota: read error at {} / {} bytes", written + filled, total);
                    return OtaReport::err("read error mid-stream");
                }
            }
        }
        if filled == 0 {
            log::info!("ota: stream ended early at {} / {} bytes", written, total);
            return OtaReport::err("stream ended early");
        }
        let chunk_off = offset + written as u32;
        let write_res = state.flash_kv.with_flash(|f| {
            use embedded_storage::Storage as _;
            f.write(chunk_off, &sector[..filled])
        });
        if write_res.is_err() {
            log::info!("ota: flash write failed at {:#x}", chunk_off);
            return OtaReport::err("flash write failed");
        }
        written += filled;
        if written >= next_log {
            log::info!("ota: {} / {} bytes written", written, total);
            next_log += 256 * 1024;
        }
    }

    if let Err(e) = state.flash_kv.with_flash(activate_inactive_slot) {
        return OtaReport::err(alloc::format!("activate failed: {:?}", e));
    }
    log::info!("ota: {} bytes written + activated", written);
    request_reboot();
    OtaReport { ok: true, detail: alloc::format!("{} bytes, rebooting", written) }
}
