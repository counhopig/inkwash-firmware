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

/// I2C0 is shared between the PCF8563 RTC, the ES8311 audio codec, and the
/// GT23SC6699 NFC tag, so all three hold a clone of the same driver
/// instance rather than each owning their own (only one `I2cDriver` may be
/// installed per port).
///
/// The lock is `Arc<Mutex<..>>` (not `Rc<RefCell<..>>`) because the RTC
/// executor runs on its own task and must take the same bus the main
/// thread's audio codec uses. `parking_lot::Mutex` is used instead of
/// `std::sync::Mutex` to match the rest of the crate (`epd_task.rs`) and
/// because its guards need no `unwrap()` at every call site.
pub type SharedI2c = Arc<Mutex<I2cDriver<'static>>>;

/// eFuse curve-fitting calibration (ESP32-S3 three-point fit) instead of
/// the uncalibrated linear `DirectConverter` fallback: without it the mV
/// reading carries a systematic offset large enough to skew the battery
/// percent by several points. Matches the official ZECTRIX demo's
/// `adc_cali_curve_fitting` setup.
const BATTERY_ADC_CHANNEL_CONFIG: AdcChannelConfig = AdcChannelConfig {
    attenuation: DB_12,
    calibration: Calibration::Curve,
    ..AdcChannelConfig::new()
};

/// Number of ADC samples averaged per `battery_millivolts` call. The
/// official demo reads 10 times; the averaged value feeds both the percent
/// curve and the charger state machine, so smoothing matters.
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
    /// Percent from the last successful `battery_millivolts` read.
    /// `battery_percent` returns this on a failed read instead of `None`
    /// forcing every caller to invent its own fallback - see its doc
    /// comment for why defaulting to "empty" is actively wrong.
    last_battery_percent: Option<u8>,
    pub display: EpdClient,
    /// The shared I2C0 bus (RTC + audio codec + NFC). The PCF8563 driver
    /// lives on the RTC executor task; `main` spawns that executor from a
    /// clone of this bus handle right after `Note4Board::take`, and the
    /// audio/NFC drivers hold their own clones.
    pub i2c_bus: SharedI2c,
    /// One-shot wake interrupts for the idle loop (see `wake.rs`): the
    /// three nav keys resume the main loop's idle wait the moment they
    /// assert, instead of waiting out the 1 s poll. (The RTC alarm line is
    /// not wired here - see `power::WAKE_PINS` for why.)
    pub wake: crate::wake::Waker,
    /// `None` when the ES8311 failed to initialize; the rest of the board
    /// (display/buttons/RTC/Wi-Fi) still works without it. Owned by the
    /// board only until `take_audio` moves it onto the audio task.
    audio: Option<Es8311>,
    /// `None` when the GT23SC6699 failed to initialize; see `audio` above.
    pub nfc: Option<NfcTag>,
}

/// Debounced charger status, as a UI/UI-adjacent view of the charge
/// management IC's two open-drain-ish status lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChargeSnapshot {
    /// External 5 V supply connected: either status line has been active
    /// within the hold window. Everything else is gated on this, so the
    /// battery-only case never reads as "full" from a floating line.
    pub power_present: bool,
    /// Battery is actively charging (CHRG_L low, debounced).
    pub charging: bool,
    /// Battery is full (STDBY_H high, debounced).
    pub full: bool,
    /// Both status lines active at once - the charge IC reporting an
    /// electrical fault. Mutually exclusive with `charging`/`full`/
    /// `no_battery`: this takes priority over all three, matching the
    /// official ZECTRIX demo's `ChargeStatus::State::kFault` (see the
    /// `ChargeStatus` doc comment below - this field used to be computed
    /// and silently discarded instead of reaching the UI).
    pub fault: bool,
    /// Both status lines have been seen active recently but neither has
    /// settled into a stable state - the demo's `State::kNoBattery`,
    /// meaning the board is likely running with no battery installed (the
    /// lines bounce between states with nothing to hold them steady).
    /// Mutually exclusive with `charging`/`full`, lower priority than
    /// `fault`.
    pub no_battery: bool,
}

/// Ported from the official ZECTRIX demo's `ChargeStatus` state machine
/// (`components/zectrix_board/charge_status.cc`), simplified to the two
/// status GPIOs plus debounce - the voltage-based precharge/CC/CV stage
/// split is omitted since the UI only needs charge/full/power-present, but
/// `fault`/`no_battery` (see `ChargeSnapshot`) are kept: the demo treats
/// both as real, user-visible states, not implementation detail.
///
/// `tick` must be called once per `report_power_state` poll (~1 s); one
/// tick is the debounce quantum, "stable" means active for ≥ 2 ticks -
/// coarser than the demo's 400 ms `kStableHighMs`, since this device's
/// e-paper display has no need for sub-second charge-status feedback.
struct ChargeStatus {
    /// Ticks since either status line was last active; `None` before the
    /// first activity. `<= 1` means power is present (matches the demo's
    /// ~1 s `kPowerPresentHoldMs`).
    power_ticks_since_seen: Option<u32>,
    /// Ticks since the CHRG_L line was last seen active; used only for
    /// `no_battery` detection (the demo's `last_detect_seen_ms_`).
    detect_ticks_since_seen: Option<u32>,
    /// Ticks since the STDBY_H line was last seen active; used only for
    /// `no_battery` detection (the demo's `last_full_seen_ms_`).
    full_ticks_since_seen: Option<u32>,
    charge_ticks: u32,
    full_ticks: u32,
    both_ticks: u32,
    /// Ticks since the last "both lines active" (a charger fault) - the
    /// demo's `kFaultHoldMs` equivalent.
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

    /// One poll tick. `charging` = CHRG_L line low (active), `charge_done`
    /// = STDBY_H line high (active).
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

        // Both lines have been seen recently but neither has settled -
        // matches the demo's `alt_seen && !detect_stable && !full_stable`.
        let alt_seen = power_present
            && self.detect_ticks_since_seen.is_some_and(|t| t <= 1)
            && self.full_ticks_since_seen.is_some_and(|t| t <= 1);
        let no_battery = alt_seen && !fault && !charge_stable && !full_stable;

        // Priority mirrors the demo's State enum exactly: fault outranks
        // no_battery outranks full outranks charging.
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

        // After a deep-sleep wakeup GPIO17 may still be held high by the
        // RTC slow IO block from the previous session. Releasing the hold
        // before constructing the PinDriver prevents the two from fighting.
        power::release_power_latch_hold()?;

        let mut power_latch = PinDriver::output(pins.gpio17)?;
        power_latch.set_high()?;

        // Starts off (`LED_G` is active-low) until the first
        // `update_charging_led` call in the main loop takes over.
        let mut led = PinDriver::output(pins.gpio3)?;
        led.set_high()?;

        let mut avdd_power = PinDriver::output(pins.gpio42)?;
        avdd_power.set_high()?;

        let key_enter = Button::new(pins.gpio0.into(), Pull::Up, crate::button::ButtonId::Enter)?;
        let key_up = Button::new(pins.gpio39.into(), Pull::Up, crate::button::ButtonId::Up)?;
        let key_down = Button::new(pins.gpio18.into(), Pull::Up, crate::button::ButtonId::Down)?;
        // One-shot wake interrupts for the idle loop: a key press resumes
        // the main loop's 1 s idle wait immediately (see `wake.rs` for why
        // the sleep wake sources alone cannot do that).
        let wake = crate::wake::Waker::new(&crate::power::WAKE_PINS);
        wake.subscribe(crate::power::GPIO_NUM_0)?;
        wake.subscribe(crate::power::GPIO_NUM_39)?;
        wake.subscribe(crate::power::GPIO_NUM_18)?;
        // Neither status line gets an internal pull: the official ZECTRIX
        // demo's `ChargeStatus::Init` explicitly disables both pull-up and
        // pull-down on both GPIOs (its only configuration of these two
        // pins anywhere in that codebase), relying on the charge IC (or an
        // external pull already on the PCB) to drive them - not on the
        // ESP32's own weak internal pulls. `charging` used to add
        // `Pull::Up` here with no comment explaining why; matched to the
        // verified-working reference instead.
        let charging = PinDriver::input(pins.gpio2, Pull::Floating)?;
        let charge_done = PinDriver::input(pins.gpio1, Pull::Floating)?;
        let display = EpdClient::new()?;

        let adc: BoardAdc = AdcDriver::new(peripherals.adc1)?;
        // GPIO4 is reserved as the analog battery pin and intentionally
        // untouched here so the ADC driver can attach it on demand inside
        // `battery_millivolts`. The peripheral is fetched by stealing from
        // the (consumed) Peripherals handle, which is safe exactly once
        // after `Peripherals::take()` in `Note4Board::take`.

        let i2c_config = I2cConfig::new().baudrate(I2C_FREQUENCY);
        let i2c = I2cDriver::new(peripherals.i2c0, pins.gpio47, pins.gpio48, &i2c_config)
            .context("failed to install I2C0 driver on GPIO47/48")?;
        let i2c_bus: SharedI2c = Arc::new(Mutex::new(i2c));
        // The PCF8563 driver and its registers are owned by the dedicated
        // RTC executor task (`rtc_executor.rs`), not by the board: no code
        // outside that single owner touches the RTC I2C registers. `main`
        // spawns the executor from the shared bus right after
        // `Note4Board::take`. The old `rtc.probe()` here is gone - probing
        // now lives on the executor task, whose spawn awaits the probe
        // result so a dead RTC still fails boot loudly.

        // ES8311 audio codec: I2S0 TX on GPIO14/15/38/45 (MCLK/BCLK/WS/DOUT),
        // speaker PA enabled on GPIO46, control registers over the I2C0 bus
        // shared with the RTC above. Soft-fails (logs and leaves `audio` as
        // `None`) rather than aborting board bring-up, since this hardware
        // path is unverified and the rest of the device should stay usable
        // even if the codec doesn't come up.
        let pa_enable = PinDriver::output(pins.gpio46)?;
        let audio = match I2sDriver::<I2sTx>::new_std_tx(
            peripherals.i2s0,
            &audio::i2s_std_config(),
            pins.gpio15,       // BCLK
            pins.gpio45,       // DOUT
            Some(pins.gpio14), // MCLK
            pins.gpio38,       // WS/LRCK
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

        // GT23SC6699 NFC tag: power on GPIO21, field-detect on GPIO7,
        // control over the same shared I2C0 bus. Soft-fails like `audio`
        // above.
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

    /// Moves the ES8311 codec out of the board and onto the audio task.
    /// Returns `None` if the codec failed to initialise at boot. The board
    /// keeps no audio handle afterwards; every tone goes through
    /// `audio_task::AudioTask`.
    pub fn take_audio(&mut self) -> Option<Es8311> {
        self.audio.take()
    }

    /// Debounced charger status. Reads both charge-management IC lines and
    /// advances the `ChargeStatus` state machine; call once per poll cycle
    /// (~1 s, i.e. from `main.rs`'s `report_power_state`) so the debounce
    /// tick rate stays constant. Everything else should read
    /// [`Note4Board::charge_snapshot`].
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

    /// Last tick's charger snapshot, for render paths that must not
    /// advance the state machine (they run at their own cadence).
    pub fn charge_snapshot(&self) -> ChargeSnapshot {
        self.charge_snapshot
    }

    /// Drives the status LED (GPIO3) as a charging indicator: lit only
    /// while a charger is connected **and** the battery is genuinely
    /// charging, dark once full, unplugged, or in a fault/no-battery state
    /// (those aren't "charging" either - a lit indicator would be actively
    /// misleading). `LED_G` is active-low (low = on, matching the official
    /// demo's `SetPowerLed(on)` -> level 0); the official firmware names
    /// this pin `ZECTRIX_POWER_LED` and keeps it off except during
    /// self-test. A plugged-in-but-full state doesn't keep a bright
    /// indicator burning all day - the battery icon on the Home screen
    /// already shows the full state.
    pub fn update_charging_led(&mut self, charge: ChargeSnapshot) -> Result<()> {
        if charge.power_present && !charge.full && !charge.fault && !charge.no_battery {
            self.led.set_low()?;
        } else {
            self.led.set_high()?;
        }
        Ok(())
    }

    /// Reads battery voltage via GPIO4 (ADC1 channel 3, on-board 1:2 divider)
    /// in mV. Averages `BATTERY_ADC_SAMPLES` raw readings converted with
    /// eFuse curve-fitting calibration (see `BATTERY_ADC_CHANNEL_CONFIG`),
    /// then doubles the pin mV since the divider halves VBAT before the
    /// pin sees it. The DB_12 attenuation tops out around ~3.1 V on
    /// ESP32-S3, so the doubled result is clamped to `u16::MAX`.
    ///
    /// The channel driver is (re)built from `&self.adc` on every call
    /// rather than stored as a `Note4Board` field: `AdcChannelDriver<'d>`
    /// borrows the `AdcDriver` it's built from, and a field can't borrow a
    /// sibling field of the same struct without unsafe self-referential
    /// tricks. The official demo sidesteps this in C++ by owning the ADC
    /// handle as a raw pointer instead; reconstructing per call is the
    /// straightforward safe-Rust equivalent, not an oversight - see
    /// `Note4Board::battery_percent` for where the actual per-call cost
    /// (one more `esp_idf_svc` config call plus `BATTERY_ADC_SAMPLES`
    /// reads) is bounded to callers that need a fresh value.
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

    /// Battery percent for display, tolerant of a transient ADC read
    /// failure: on success, computes and remembers the percent; on
    /// failure, returns whatever was last remembered instead of `None`.
    /// Every render call site used to do `battery_millivolts().ok().map
    /// (battery_percent_from_mv)` directly, which turns a single transient
    /// I2C/ADC hiccup into the *emptiest* possible battery icon
    /// (`unwrap_or(0)` at the render layer) regardless of the real charge
    /// level - the worst available default for a false reading. `None` is
    /// only possible here before the first successful read ever completes
    /// (e.g. right at boot).
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

/// Battery percent from the official ZECTRIX demo's quadratic fit of the
/// single-cell LiPo discharge curve (`zectrix_board.cc::ReadBattery`):
/// `(-mv^2 + 9016*mv - 19189000) / 10000`, clamped to 0..100. 0% sits at
/// ~3444 mV and 100% at ~4200 mV; the curve is steeper in the upper band
/// than the old linear 3300-4200 mapping, which over-reported percent
/// around the 4.0 V plateau where a discharging cell spends most of its
/// life.
pub fn battery_percent_from_mv(mv: u16) -> u8 {
    let mv = mv as i32;
    let calculated = (-mv * mv + 9016 * mv - 19189000) / 10000;
    calculated.clamp(0, 100) as u8
}
