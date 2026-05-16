//! Analog + pulse sensor sampling — battery, water pressure, flow.
//!
//! Mirrors the IDF firmware's sensor task: ADC1 reads on GPIO36
//! (battery divider) and GPIO32 (pressure transducer), and a PCNT unit
//! counting flow-meter pulses on GPIO33. Raw values run through the
//! `core` calibration chains (`Config::sensors`) and land in the
//! device-state snapshot, so `/api/status` and the SPA dashboard show
//! real readings.
//!
//! Cadence matches the IDF build: battery every 10 min (15-sample
//! moving average), pressure + flow every 1 min. The PCNT counter is a
//! 16-bit hardware register, so it's polled every second and the
//! wrapping delta accumulated into a 64-bit software total.

use esp_hal::{
    analog::adc::{Adc, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC1, GPIO32, GPIO33, GPIO36, PCNT},
    pcnt::{
        channel::EdgeMode,
        Pcnt,
    },
    Blocking,
};
use embassy_time::{Duration, Timer};
use watercontroller_core::app::App;

/// Owned ADC1 driver + the two enabled channels.
pub struct Analog {
    pub adc: Adc<'static, ADC1<'static>, Blocking>,
    pub battery: AdcPin<GPIO36<'static>, ADC1<'static>>,
    pub pressure: AdcPin<GPIO32<'static>, ADC1<'static>>,
}

impl Analog {
    /// Build the ADC1 driver with GPIO36 (battery) + GPIO32 (pressure)
    /// enabled at 11 dB attenuation (full ~0-3.1 V input range).
    pub fn new(adc1: ADC1<'static>, gpio36: GPIO36<'static>, gpio32: GPIO32<'static>) -> Self {
        let mut cfg = AdcConfig::new();
        let battery = cfg.enable_pin(gpio36, Attenuation::_11dB);
        let pressure = cfg.enable_pin(gpio32, Attenuation::_11dB);
        let adc = Adc::new(adc1, cfg);
        Self { adc, battery, pressure }
    }
}

fn read_battery(a: &mut Analog) -> u16 {
    loop {
        match a.adc.read_oneshot(&mut a.battery) {
            Ok(v) => return v,
            Err(nb::Error::WouldBlock) => {}
            Err(nb::Error::Other(_)) => return 0,
        }
    }
}

fn read_pressure(a: &mut Analog) -> u16 {
    loop {
        match a.adc.read_oneshot(&mut a.pressure) {
            Ok(v) => return v,
            Err(nb::Error::WouldBlock) => {}
            Err(nb::Error::Other(_)) => return 0,
        }
    }
}

#[embassy_executor::task]
pub async fn sensor_task(app: App, mut analog: Analog, pcnt_periph: PCNT<'static>, flow_gpio: GPIO33<'static>) {
    // PCNT unit 0: count rising edges of the flow-meter signal on
    // GPIO33; the control signal is unused (level kept).
    let pcnt = Pcnt::new(pcnt_periph);
    let unit = &pcnt.unit0;
    unit.channel0.set_edge_signal(flow_gpio);
    unit.channel0.set_input_mode(EdgeMode::Hold, EdgeMode::Increment);
    unit.clear();
    unit.resume();

    // 16-bit hardware counter → 64-bit software accumulator.
    let mut last_raw: i16 = unit.counter.get();
    let mut pulse_total: u64 = 0;

    // Battery 15-sample moving average (one sample per 10 min tick).
    let mut bat_window = [0f32; 15];
    let mut bat_len = 0usize;
    let mut bat_pos = 0usize;

    let mut secs: u64 = 0;
    let mut pulses_at_last_flow: u64 = 0;
    loop {
        Timer::after(Duration::from_secs(1)).await;
        secs += 1;

        // Accumulate the wrapping counter delta every second.
        let raw = unit.counter.get();
        let delta = raw.wrapping_sub(last_raw);
        last_raw = raw;
        if delta > 0 {
            pulse_total += delta as u64;
        }

        let cfg = app.config();

        // Battery — every 10 min, plus an early first read at 10 s so
        // the dashboard isn't blank for the first 10 minutes.
        if secs == 10 || secs % 600 == 0 {
            let v = cfg.sensors.battery.apply(read_battery(&mut analog) as f32);
            bat_window[bat_pos] = v;
            bat_pos = (bat_pos + 1) % bat_window.len();
            if bat_len < bat_window.len() {
                bat_len += 1;
            }
            let avg = bat_window[..bat_len].iter().sum::<f32>() / bat_len as f32;
            app.update_state(|s| s.sensors.battery_v = Some(avg));
        }

        // Pressure — every 1 min, two-stage calibration chain.
        if secs % 60 == 0 {
            let raw = read_pressure(&mut analog) as f32;
            let stage1 = cfg.sensors.pressure_stage1.apply(raw);
            let bar = cfg.sensors.pressure_stage2.apply(stage1);
            app.update_state(|s| s.sensors.pressure_bar = Some(bar));
        }

        // Flow + total water — every 1 min from the pulse delta.
        if secs % 60 == 0 {
            let delta_pulses = pulse_total.saturating_sub(pulses_at_last_flow) as f32;
            let pps = delta_pulses / 60.0;
            let lph = pps * cfg.sensors.flow_lph_per_pps;
            let total = pulse_total as f32 * cfg.sensors.flow_l_per_pulse;
            app.update_state(|s| {
                s.sensors.flow_lph = Some(lph);
                s.sensors.total_l = Some(total);
            });
            pulses_at_last_flow = pulse_total;
        }
    }
}
