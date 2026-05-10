//! Adc implementation — placeholder until milestone 5 wires the ESP-IDF
//! oneshot ADC. The trait shape is what matters here so the rest of the
//! firmware (sensor task, calibration, MQTT publisher) can compile and link.

use watercontroller_core::traits::Adc;

/// Stub ADC that always returns the same raw value. Replaced in M5.
pub struct PlaceholderAdc(pub u16);

impl Adc for PlaceholderAdc {
    fn read_raw(&mut self) -> u16 {
        self.0
    }
}
