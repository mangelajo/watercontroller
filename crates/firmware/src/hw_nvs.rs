//! NvsStore implementation backed by ESP-IDF's default NVS partition.
//!
//! Keys are stored as blobs. The NVS API is wrapped in a Mutex because
//! `EspNvs` is not Sync; this is fine because writes are infrequent.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};
use std::sync::Mutex;
use watercontroller_core::traits::{NvsError, NvsStore};

pub struct EspNvsStore {
    inner: Mutex<EspNvs<NvsDefault>>,
}

impl EspNvsStore {
    pub fn new(nvs: EspNvs<NvsDefault>) -> Self {
        Self { inner: Mutex::new(nvs) }
    }
}

impl NvsStore for EspNvsStore {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        let n = self.inner.lock().ok()?;
        let len = n.blob_len(key).ok()??;
        let mut buf = vec![0u8; len];
        n.get_blob(key, &mut buf).ok()?;
        Some(buf)
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), NvsError> {
        self.inner
            .lock()
            .map_err(|e| NvsError::Io(format!("nvs lock poisoned: {e}")))?
            .set_blob(key, value)
            .map_err(|e| NvsError::Io(format!("set_blob: {e}")))
    }
    fn remove(&self, key: &str) -> Result<(), NvsError> {
        self.inner
            .lock()
            .map_err(|e| NvsError::Io(format!("nvs lock poisoned: {e}")))?
            .remove(key)
            .map(|_| ())
            .map_err(|e| NvsError::Io(format!("remove: {e}")))
    }
}
