//! Adc trait impl on top of esp-idf-hal's oneshot ADC API. The ADC1
//! peripheral and its channel drivers must outlive the `EspAdc`s we hand
//! to the sensor task — `Box::leak` keeps them alive for the rest of the
//! program (we never tear down the sensor task).

use esp_idf_hal::adc::attenuation::DB_11;
use esp_idf_hal::adc::oneshot::{
    config::{AdcChannelConfig, Calibration},
    AdcChannelDriver, AdcDriver,
};

/// Returns a fixed value; used under `--features qemu` where the ADC
/// peripheral hangs on `read_raw`.
pub struct PlaceholderAdc(pub u16);
impl Adc for PlaceholderAdc {
    fn read_raw(&mut self) -> u16 {
        self.0
    }
}
use esp_idf_hal::adc::ADC1;
use esp_idf_hal::gpio::{ADCPin, Gpio32, Gpio36};
use esp_idf_hal::peripheral::Peripheral;
use watercontroller_core::traits::Adc;

pub struct EspAdcChan<P: ADCPin<Adc = ADC1>> {
    driver: &'static AdcDriver<'static, ADC1>,
    chan: AdcChannelDriver<'static, P, &'static AdcDriver<'static, ADC1>>,
}

impl<P: ADCPin<Adc = ADC1>> EspAdcChan<P> {
    pub fn new(
        driver: &'static AdcDriver<'static, ADC1>,
        pin: impl Peripheral<P = P> + 'static,
    ) -> anyhow::Result<Self> {
        // We rely on the per-channel calibration tables in
        // `core::config::SensorsConfig` rather than ESP-IDF's eFuse
        // calibration — the eFuse vref isn't set in QEMU and depending on
        // it would fail open. Calibration::None is acceptable; the math in
        // core::calibration handles the real-world conversion.
        let cfg = AdcChannelConfig {
            attenuation: DB_11,
            calibration: Calibration::None,
            ..Default::default()
        };
        let chan = AdcChannelDriver::new(driver, pin, &cfg)?;
        Ok(Self { driver, chan })
    }
}

impl<P: ADCPin<Adc = ADC1>> Adc for EspAdcChan<P> {
    fn read_raw(&mut self) -> u16 {
        self.driver.read_raw(&mut self.chan).unwrap_or(0)
    }
}

/// Convenience: owned (battery, pressure) channels for the doremorwater pins.
pub fn build_battery_pressure(
    adc1: ADC1,
    pin36: Gpio36,
    pin32: Gpio32,
) -> anyhow::Result<(EspAdcChan<Gpio36>, EspAdcChan<Gpio32>)> {
    let driver: &'static AdcDriver<'static, ADC1> = Box::leak(Box::new(AdcDriver::new(adc1)?));
    let bat = EspAdcChan::new(driver, pin36)?;
    let pres = EspAdcChan::new(driver, pin32)?;
    Ok((bat, pres))
}
