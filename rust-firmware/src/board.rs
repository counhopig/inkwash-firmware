use std::sync::Arc;

use anyhow::{Context, Result};
use esp_idf_svc::hal::adc::attenuation::DB_12;
use esp_idf_svc::hal::adc::oneshot::{
    config::{AdcChannelConfig, Calibration},
    AdcChannelDriver, AdcDriver,
};
use esp_idf_svc::hal::gpio::{Input, Output, PinDriver, Pull};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sTx};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::units::Hertz;
use parking_lot::Mutex;

use crate::audio::{self, Es8311};
use crate::button::Button;
use crate::display::EpdClient;
use crate::nfc::{self, NfcTag};
use crate::power;
pub type BoardAdc = AdcDriver<'static, esp_idf_svc::hal::adc::ADCU1>;

pub type SharedI2c = Arc<Mutex<I2cDriver<'static>>>;

const BATTERY_ADC_CHANNEL_CONFIG: AdcChannelConfig = AdcChannelConfig {
    attenuation: DB_12,
    calibration: Calibration::Curve,
    ..AdcChannelConfig::new()
};

const BATTERY_ADC_SAMPLES: u32 = 10;

const I2C_FREQUENCY: Hertz = Hertz(400_000);

pub struct Note4Board {
    _power_latch: PinDriver<'static, Output>,
    led: PinDriver<'static, Output>,
    _avdd_power: PinDriver<'static, Output>,
    pub key_enter: Button,
    pub key_up: Button,
    pub key_down: Button,
    charging: PinDriver<'static, Input>,
    charge_done: PinDriver<'static, Input>,
    charge_status: ChargeStatus,
    charge_snapshot: ChargeSnapshot,
    adc: BoardAdc,

    last_battery_percent: Option<u8>,
    pub display: EpdClient,

    pub i2c_bus: SharedI2c,

    pub wake: crate::wake::Waker,

    audio: Option<Es8311>,

    pub nfc: Option<NfcTag>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChargeSnapshot {
    pub power_present: bool,

    pub charging: bool,

    pub full: bool,

    pub fault: bool,

    pub no_battery: bool,
}

struct ChargeStatus {
    power_ticks_since_seen: Option<u32>,

    detect_ticks_since_seen: Option<u32>,

    full_ticks_since_seen: Option<u32>,
    charge_ticks: u32,
    full_ticks: u32,
    both_ticks: u32,

    fault_ticks_since_seen: Option<u32>,
}

impl ChargeStatus {
    const fn new() -> Self {
        Self {
            power_ticks_since_seen: None,
            detect_ticks_since_seen: None,
            full_ticks_since_seen: None,
            charge_ticks: 0,
            full_ticks: 0,
            both_ticks: 0,
            fault_ticks_since_seen: None,
        }
    }

    fn tick(&mut self, charging: bool, charge_done: bool) -> ChargeSnapshot {
        self.charge_ticks = if charging { self.charge_ticks + 1 } else { 0 };
        self.full_ticks = if charge_done { self.full_ticks + 1 } else { 0 };
        let both_active = charging && charge_done;
        self.both_ticks = if both_active { self.both_ticks + 1 } else { 0 };

        self.power_ticks_since_seen = if charging || charge_done {
            Some(0)
        } else {
            self.power_ticks_since_seen.map(|t| t + 1)
        };
        let power_present = self.power_ticks_since_seen.is_some_and(|t| t <= 1);

        self.detect_ticks_since_seen = if charging {
            Some(0)
        } else {
            self.detect_ticks_since_seen.map(|t| t + 1)
        };
        self.full_ticks_since_seen = if charge_done {
            Some(0)
        } else {
            self.full_ticks_since_seen.map(|t| t + 1)
        };

        let charge_stable = self.charge_ticks >= 2;
        let full_stable = self.full_ticks >= 2;
        let both_stable = self.both_ticks >= 2;

        self.fault_ticks_since_seen = if both_stable {
            Some(0)
        } else {
            self.fault_ticks_since_seen.map(|t| t + 1)
        };
        let fault = power_present && self.fault_ticks_since_seen.is_some_and(|t| t <= 1);

        let alt_seen = power_present
            && self.detect_ticks_since_seen.is_some_and(|t| t <= 1)
            && self.full_ticks_since_seen.is_some_and(|t| t <= 1);
        let no_battery = alt_seen && !fault && !charge_stable && !full_stable;

        let full = power_present && !fault && !no_battery && full_stable;
        let charging = power_present && !fault && !no_battery && !full && charge_stable;

        ChargeSnapshot {
            power_present,
            charging,
            full,
            fault,
            no_battery,
        }
    }
}

impl Note4Board {
    pub fn take() -> Result<Self> {
        let peripherals = Peripherals::take()?;
        let pins = peripherals.pins;

        power::release_power_latch_hold()?;

        let mut power_latch = PinDriver::output(pins.gpio17)?;
        power_latch.set_high()?;

        let mut led = PinDriver::output(pins.gpio3)?;
        led.set_high()?;

        let mut avdd_power = PinDriver::output(pins.gpio42)?;
        avdd_power.set_high()?;

        let key_enter = Button::new(pins.gpio0.into(), Pull::Up, crate::button::ButtonId::Enter)?;
        let key_up = Button::new(pins.gpio39.into(), Pull::Up, crate::button::ButtonId::Up)?;
        let key_down = Button::new(pins.gpio18.into(), Pull::Up, crate::button::ButtonId::Down)?;

        let wake = crate::wake::Waker::new(&crate::power::WAKE_PINS);
        wake.subscribe(crate::power::GPIO_NUM_0)?;
        wake.subscribe(crate::power::GPIO_NUM_39)?;
        wake.subscribe(crate::power::GPIO_NUM_18)?;

        let charging = PinDriver::input(pins.gpio2, Pull::Floating)?;
        let charge_done = PinDriver::input(pins.gpio1, Pull::Floating)?;
        let display = EpdClient::new()?;

        let adc: BoardAdc = AdcDriver::new(peripherals.adc1)?;

        let i2c_config = I2cConfig::new().baudrate(I2C_FREQUENCY);
        let i2c = I2cDriver::new(peripherals.i2c0, pins.gpio47, pins.gpio48, &i2c_config)
            .context("failed to install I2C0 driver on GPIO47/48")?;
        let i2c_bus: SharedI2c = Arc::new(Mutex::new(i2c));

        let pa_enable = PinDriver::output(pins.gpio46)?;
        let audio = match I2sDriver::<I2sTx>::new_std_tx(
            peripherals.i2s0,
            &audio::i2s_std_config(),
            pins.gpio15,
            pins.gpio45,
            Some(pins.gpio14),
            pins.gpio38,
        ) {
            Ok(i2s) => match Es8311::new(i2c_bus.clone(), audio::ES8311_ADDR, i2s, pa_enable) {
                Ok(codec) => Some(codec),
                Err(err) => {
                    log::warn!("ES8311 init failed: {err}");
                    None
                }
            },
            Err(err) => {
                log::warn!("I2S0 TX channel setup failed: {err}");
                None
            }
        };

        let nfc_power = PinDriver::output(pins.gpio21)?;
        let nfc_fd = PinDriver::input(pins.gpio7, Pull::Up)?;
        let nfc = match NfcTag::new(i2c_bus.clone(), nfc::GT23SC6699_ADDR, nfc_power, nfc_fd) {
            Ok(tag) => Some(tag),
            Err(err) => {
                log::warn!("NFC init failed: {err}");
                None
            }
        };

        Ok(Self {
            _power_latch: power_latch,
            led,
            _avdd_power: avdd_power,
            key_enter,
            key_up,
            key_down,
            charging,
            charge_done,
            charge_status: ChargeStatus::new(),
            charge_snapshot: ChargeSnapshot::default(),
            adc,
            last_battery_percent: None,
            display,
            i2c_bus: i2c_bus.clone(),
            wake,
            audio,
            nfc,
        })
    }

    pub fn take_audio(&mut self) -> Option<Es8311> {
        self.audio.take()
    }

    pub fn charging_state(&mut self) -> ChargeSnapshot {
        let snapshot = self
            .charge_status
            .tick(self.charging.is_low(), self.charge_done.is_high());
        if snapshot.full {
            log::debug!("charger: full (STDBY_H high, debounced)");
        }
        if snapshot.fault {
            log::warn!("charger: fault (CHRG_L and STDBY_H both active, debounced)");
        }
        if snapshot.no_battery {
            log::warn!("charger: no battery detected (status lines alternating, debounced)");
        }
        self.charge_snapshot = snapshot;
        snapshot
    }

    pub fn charge_snapshot(&self) -> ChargeSnapshot {
        self.charge_snapshot
    }

    pub fn update_charging_led(&mut self, charge: ChargeSnapshot) -> Result<()> {
        if charge.power_present && !charge.full && !charge.fault && !charge.no_battery {
            self.led.set_low()?;
        } else {
            self.led.set_high()?;
        }
        Ok(())
    }

    pub fn battery_millivolts(&mut self) -> Result<u16> {
        let peripherals = unsafe { Peripherals::steal() };
        let mut channel = AdcChannelDriver::new(
            &self.adc,
            peripherals.pins.gpio4,
            &BATTERY_ADC_CHANNEL_CONFIG,
        )?;
        let mut sum: u32 = 0;
        for _ in 0..BATTERY_ADC_SAMPLES {
            sum += self.adc.read(&mut channel)? as u32;
        }
        let avg_mv = (sum / BATTERY_ADC_SAMPLES) as u16;
        let vbat_mv = (avg_mv as u32) * 2;
        Ok(vbat_mv.min(u16::MAX as u32) as u16)
    }

    pub fn battery_percent(&mut self) -> Option<u8> {
        match self.battery_millivolts() {
            Ok(mv) => {
                let percent = battery_percent_from_mv(mv);
                self.last_battery_percent = Some(percent);
                Some(percent)
            }
            Err(err) => {
                log::warn!(
                    "Battery ADC read failed, reusing last known percent ({:?}): {err}",
                    self.last_battery_percent
                );
                self.last_battery_percent
            }
        }
    }
}

pub fn battery_percent_from_mv(mv: u16) -> u8 {
    let mv = mv as i32;
    let calculated = (-mv * mv + 9016 * mv - 19189000) / 10000;
    calculated.clamp(0, 100) as u8
}
