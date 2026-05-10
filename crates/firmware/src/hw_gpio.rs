//! GpioOut trait impl wrapping a single ESP-IDF output pin.

use esp_idf_hal::gpio::{AnyOutputPin, Output, PinDriver};
use esp_idf_hal::peripheral::Peripheral;
use watercontroller_core::traits::GpioOut;

pub struct EspGpioOut(PinDriver<'static, AnyOutputPin, Output>);

impl EspGpioOut {
    pub fn new<P>(pin: P) -> anyhow::Result<Self>
    where
        P: Peripheral<P = AnyOutputPin> + 'static,
    {
        let mut driver = PinDriver::output(pin)?;
        driver.set_low()?;
        Ok(Self(driver))
    }
}

impl GpioOut for EspGpioOut {
    fn set(&mut self, high: bool) {
        if high {
            let _ = self.0.set_high();
        } else {
            let _ = self.0.set_low();
        }
    }
}
