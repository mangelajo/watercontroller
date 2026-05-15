//! Flash-backed key-value store implementing `core::traits::NvsStore`.
//!
//! The IDF firmware used ESP-IDF's NVS component. The no_std firmware
//! has no such thing, so this is a deliberately simple replacement:
//! an in-RAM `BTreeMap` that is the source of truth for reads, mirrored
//! to a flash region as one TLV blob rewritten on every `set`/`remove`.
//!
//! Why the whole-blob rewrite instead of sequential-storage's proper
//! log-structured KV: our total payload is tiny (config ~2 KiB + valve
//! byte + alarm history ~1 KiB) and writes are rare (a config change,
//! a valve transition). Erasing + rewriting two 4 KiB sectors per
//! change is well within flash endurance and a fraction of the code.
//!
//! Region: 8 KiB at flash offset 0x9000 — the `nvs` partition from
//! partitions.csv (0x9000, len 0x6000). We use the first 2 sectors.
//!
//! On-flash layout (little-endian, every field 4-byte aligned):
//!   magic  "WCKV"            (4 bytes)
//!   version 1u32             (4 bytes)
//!   count   Nu32             (4 bytes)
//!   N × entry:
//!     klen u32, vlen u32, key bytes (pad→4), val bytes (pad→4)

use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use spin::Mutex;
use watercontroller_core::traits::{NvsError, NvsStore};

const REGION_OFFSET: u32 = 0x9000;
const REGION_LEN: u32 = 8 * 1024;
const MAGIC: &[u8; 4] = b"WCKV";
const VERSION: u32 = 1;

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

pub struct FlashKv {
    flash: Mutex<FlashStorage<'static>>,
    cache: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FlashKv {
    /// Construct from the flash peripheral, loading any existing blob.
    pub fn new(flash: FlashStorage<'static>) -> Self {
        let mut flash = flash;
        let cache = Self::load(&mut flash).unwrap_or_default();
        Self {
            flash: Mutex::new(flash),
            cache: Mutex::new(cache),
        }
    }

    /// Run `f` with exclusive access to the raw flash. The OTA path
    /// needs to write the inactive app partition + flip otadata; flash
    /// is a hardware singleton, so the NVS store owns it and lends it
    /// out here. The lock is held only for the closure — the OTA
    /// streamer takes it once per 4 KiB sector, never across an await,
    /// so a concurrent config write just waits a few ms.
    pub fn with_flash<R>(&self, f: impl FnOnce(&mut FlashStorage<'static>) -> R) -> R {
        f(&mut self.flash.lock())
    }

    /// Read + parse the flash region. Returns None on bad magic /
    /// version / corruption (treated as "empty store").
    fn load(flash: &mut FlashStorage<'static>) -> Option<BTreeMap<String, Vec<u8>>> {
        let mut buf = vec![0u8; REGION_LEN as usize];
        flash.read(REGION_OFFSET, &mut buf).ok()?;

        if &buf[0..4] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().ok()?);
        if version != VERSION {
            return None;
        }
        let count = u32::from_le_bytes(buf[8..12].try_into().ok()?) as usize;

        let mut map = BTreeMap::new();
        let mut pos = 12usize;
        for _ in 0..count {
            if pos + 8 > buf.len() {
                return None;
            }
            let klen = u32::from_le_bytes(buf[pos..pos + 4].try_into().ok()?) as usize;
            let vlen = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().ok()?) as usize;
            pos += 8;
            let kend = pos + klen;
            if kend > buf.len() {
                return None;
            }
            let key = String::from_utf8(buf[pos..kend].to_vec()).ok()?;
            pos = pad4(kend);
            let vend = pos + vlen;
            if vend > buf.len() {
                return None;
            }
            let val = buf[pos..vend].to_vec();
            pos = pad4(vend);
            map.insert(key, val);
        }
        Some(map)
    }

    /// Serialize the cache and rewrite the flash region.
    fn flush(&self, cache: &BTreeMap<String, Vec<u8>>) -> Result<(), NvsError> {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(cache.len() as u32).to_le_bytes());
        for (k, v) in cache {
            buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            buf.extend_from_slice(k.as_bytes());
            buf.resize(pad4(buf.len()), 0);
            buf.extend_from_slice(v);
            buf.resize(pad4(buf.len()), 0);
        }
        if buf.len() as u32 > REGION_LEN {
            return Err(NvsError::Full);
        }
        // NorFlash writes must be a multiple of WRITE_SIZE (4); the
        // pad4 calls above already keep `buf` 4-aligned.
        let mut flash = self.flash.lock();
        flash
            .erase(REGION_OFFSET, REGION_OFFSET + REGION_LEN)
            .map_err(|_| NvsError::Io(String::from("erase")))?;
        flash
            .write(REGION_OFFSET, &buf)
            .map_err(|_| NvsError::Io(String::from("write")))?;
        Ok(())
    }
}

impl NvsStore for FlashKv {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.cache.lock().get(key).cloned()
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), NvsError> {
        let mut cache = self.cache.lock();
        cache.insert(String::from(key), value.to_vec());
        self.flush(&cache)
    }

    fn remove(&self, key: &str) -> Result<(), NvsError> {
        let mut cache = self.cache.lock();
        cache.remove(key);
        self.flush(&cache)
    }
}
