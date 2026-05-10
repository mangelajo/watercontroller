//! HTTP-push OTA — stubbed until M10. ESP-IDF's `esp_https_ota` /
//! `esp_ota` APIs handle the partition write + boot-validation lifecycle.

pub fn pending_verify_completed() {
    // M10: call esp_ota_mark_app_valid_cancel_rollback() once the device has
    // proven itself (WiFi up + HTTPD up + first MQTT publish ok).
}
