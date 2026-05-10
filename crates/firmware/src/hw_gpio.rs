//! GpioOut implementation wrapping a single ESP-IDF output pin.

use esp_idf_hal::gpio::{AnyOutputPin, Output, PinDriver};
use watercontroller_core::traits::GpioOut;

pub struct EspGpioOut {
    pin: PinDriver<'static, AnyOutputPin, Output>,
}

impl EspGpioOut {
    pub fn new(pin: PinDriver<'static, AnyOutputPin, Output>) -> Self {
        Self { pin }
    }
}

impl GpioOut for EspGpioOut {
    fn set(&mut self, high: bool) {
        if high {
            let _ = self.pin.set_high();
        } else {
            let _ = self.pin.set_low();
        }
    }
}
