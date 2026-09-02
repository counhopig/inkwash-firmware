//! ES8311 codec bring-up: I2C control-register init + I2S TX playback,
//! fixed to this board's exact wiring (16 kHz mono, MCLK = 256x = 4.096 MHz,
//! I2C addr 0x18 - confirmed against the official ZECTRIX demo's
//! `zectrix_board_config.h` and `PrepareAudio()`).
//!
//! The register/coefficient sequence is ported from the well-known ES8311
//! driver in zRedShift/es8311-rs (MIT/Apache-2.0), specialized to our one
//! fixed hardware configuration instead of the general multi-rate driver -
//! this board never runs the codec at any other MCLK/sample-rate pair.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::hal::gpio::{Output, PinDriver};
use esp_idf_svc::hal::i2s::config::{DataBitWidth, StdConfig};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sTx};
use esp_idf_svc::sys::TickType_t;
use parking_lot::Mutex;

pub const ES8311_ADDR: u8 = 0x18;
pub const SAMPLE_RATE_HZ: u32 = 16_000;
const I2C_TIMEOUT_TICKS: TickType_t = 100;
/// Generous timeout for queuing a short test-tone buffer into I2S DMA.
const I2S_WRITE_TIMEOUT_TICKS: TickType_t = 5_000;

mod reg {
    pub const RESET: u8 = 0x00;
    pub const CLK_MANAGER_01: u8 = 0x01;
    pub const CLK_MANAGER_02: u8 = 0x02;
    pub const CLK_MANAGER_03: u8 = 0x03;
    pub const CLK_MANAGER_04: u8 = 0x04;
    pub const CLK_MANAGER_05: u8 = 0x05;
    pub const CLK_MANAGER_06: u8 = 0x06;
    pub const CLK_MANAGER_07: u8 = 0x07;
    pub const CLK_MANAGER_08: u8 = 0x08;
    pub const SDP_IN_09: u8 = 0x09;
    pub const SDP_OUT_0A: u8 = 0x0A;
    pub const SYSTEM_0D: u8 = 0x0D;
    pub const SYSTEM_0E: u8 = 0x0E;
    pub const SYSTEM_12: u8 = 0x12;
    pub const SYSTEM_13: u8 = 0x13;
    pub const ADC_1C: u8 = 0x1C;
    pub const DAC_31: u8 = 0x31;
    pub const DAC_32: u8 = 0x32;
    pub const DAC_37: u8 = 0x37;
}

/// PLL coefficients for MCLK=4.096MHz (256x) at 16kHz - the single
/// combination `StdConfig::philips(SAMPLE_RATE_HZ, ..)` produces. Values
/// are the upstream driver's lookup-table entry for (Freq4096KHz, Freq16KHz).
mod coeff {
    pub const PRE_DIV: u8 = 0x01;
    pub const PRE_MULTI: u8 = 0x00;
    pub const ADC_DIV: u8 = 0x01;
    pub const DAC_DIV: u8 = 0x01;
    pub const FS_MODE: u8 = 0x00;
    pub const LRCK_H: u8 = 0x00;
    pub const LRCK_L: u8 = 0xFF;
    pub const BCLK_DIV: u8 = 0x04;
    pub const ADC_OSR: u8 = 0x10;
    pub const DAC_OSR: u8 = 0x10;
}

/// 16-bit resolution's SDP format code (`Resolution16` in the upstream
/// driver: value 3, packed into bits 4:2 of the SDP register).
const RESOLUTION_16BIT_SDP: u8 = 3 << 2;

pub struct Es8311 {
    i2c: Arc<Mutex<esp_idf_svc::hal::i2c::I2cDriver<'static>>>,
    addr: u8,
    i2s: I2sDriver<'static, I2sTx>,
    pa_enable: PinDriver<'static, Output>,
}

impl Es8311 {
    pub fn new(
        i2c: Arc<Mutex<esp_idf_svc::hal::i2c::I2cDriver<'static>>>,
        addr: u8,
        i2s: I2sDriver<'static, I2sTx>,
        pa_enable: PinDriver<'static, Output>,
    ) -> Result<Self> {
        let mut codec = Self {
            i2c,
            addr,
            i2s,
            pa_enable,
        };
        codec.init()?;
        Ok(codec)
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8> {
        let mut value = [0u8; 1];
        self.i2c
            .lock()
            .write_read(self.addr, &[reg], &mut value, I2C_TIMEOUT_TICKS)
            .map_err(|e| anyhow!("ES8311 read reg 0x{reg:02x} failed: {e}"))?;
        Ok(value[0])
    }

    fn write_reg(&mut self, reg: u8, value: u8) -> Result<()> {
        self.i2c
            .lock()
            .write(self.addr, &[reg, value], I2C_TIMEOUT_TICKS)
            .map_err(|e| anyhow!("ES8311 write reg 0x{reg:02x} failed: {e}"))
    }

    /// Reset + clock + format + power-up sequence, ported from the upstream
    /// driver's `init()` / `clock_config()` / `format_config()`.
    fn init(&mut self) -> Result<()> {
        self.write_reg(reg::RESET, 0x1F)?;
        thread::sleep(Duration::from_micros(20));
        self.write_reg(reg::RESET, 0x00)?;
        self.write_reg(reg::RESET, 0x80)?;

        // External MCLK enabled (reg01 bit7), MCLK not inverted.
        self.write_reg(reg::CLK_MANAGER_01, 0x3F | (1 << 7))?;
        let reg06 = self.read_reg(reg::CLK_MANAGER_06)?;
        self.write_reg(reg::CLK_MANAGER_06, reg06 & !(1 << 5))?; // sclk not inverted

        let reg02 = self.read_reg(reg::CLK_MANAGER_02)?;
        let reg02 = (reg02 & 0x07) | ((coeff::PRE_DIV - 1) << 5) | (coeff::PRE_MULTI << 3);
        self.write_reg(reg::CLK_MANAGER_02, reg02)?;
        self.write_reg(reg::CLK_MANAGER_03, (coeff::FS_MODE << 6) | coeff::ADC_OSR)?;
        self.write_reg(reg::CLK_MANAGER_04, coeff::DAC_OSR)?;
        self.write_reg(
            reg::CLK_MANAGER_05,
            ((coeff::ADC_DIV - 1) << 4) | (coeff::DAC_DIV - 1),
        )?;
        let reg06 = self.read_reg(reg::CLK_MANAGER_06)?;
        let bclk_div = coeff::BCLK_DIV - u8::from(coeff::BCLK_DIV < 19);
        self.write_reg(reg::CLK_MANAGER_06, (reg06 & 0xE0) | bclk_div)?;
        let reg07 = self.read_reg(reg::CLK_MANAGER_07)?;
        self.write_reg(reg::CLK_MANAGER_07, (reg07 & 0xC0) | coeff::LRCK_H)?;
        self.write_reg(reg::CLK_MANAGER_08, coeff::LRCK_L)?;

        // 16-bit in/out over the I2S serial data ports.
        let reg00 = self.read_reg(reg::RESET)?;
        self.write_reg(reg::RESET, reg00 & 0xBF)?;
        self.write_reg(reg::SDP_IN_09, RESOLUTION_16BIT_SDP)?;
        self.write_reg(reg::SDP_OUT_0A, RESOLUTION_16BIT_SDP)?;

        self.write_reg(reg::SYSTEM_0D, 0x01)?;
        self.write_reg(reg::SYSTEM_0E, 0x02)?;
        self.write_reg(reg::SYSTEM_12, 0x00)?;
        self.write_reg(reg::SYSTEM_13, 0x10)?;
        self.write_reg(reg::ADC_1C, 0x6A)?;
        self.write_reg(reg::DAC_37, 0x08)?;

        self.set_volume(200)?;
        self.set_mute(false)?;
        Ok(())
    }

    /// Digital DAC volume, 0 (silent) to 255 (max digital gain).
    pub fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.write_reg(reg::DAC_32, volume)
    }

    pub fn set_mute(&mut self, mute: bool) -> Result<()> {
        let mut reg31 = self.read_reg(reg::DAC_31)?;
        const MUTE_BITS: u8 = (1 << 6) | (1 << 5);
        if mute {
            reg31 |= MUTE_BITS;
        } else {
            reg31 &= !MUTE_BITS;
        }
        self.write_reg(reg::DAC_31, reg31)
    }

    /// Streams a sine tone straight to the I2S TX channel in small
    /// stack-buffered chunks, so a multi-second tone never needs a single
    /// large heap allocation. Used as the alarm-ring tone in `alarms.rs`.
    pub fn play_sine_stereo(
        &mut self,
        freq_hz: f32,
        duration_secs: f32,
        amplitude: i16,
    ) -> Result<()> {
        self.pa_enable.set_high()?;
        thread::sleep(Duration::from_millis(10));
        self.i2s.tx_enable()?;

        const CHUNK_FRAMES: usize = 256;
        let mut chunk = [0u8; CHUNK_FRAMES * 4]; // 4 bytes/frame: L+R x 16-bit
        let sample_rate = SAMPLE_RATE_HZ as f32;
        let total_frames = (sample_rate * duration_secs) as usize;

        let mut result = Ok(());
        let mut frame = 0usize;
        while frame < total_frames {
            let n = CHUNK_FRAMES.min(total_frames - frame);
            for i in 0..n {
                let t = (frame + i) as f32 / sample_rate;
                let s =
                    (amplitude as f32 * (2.0 * std::f32::consts::PI * freq_hz * t).sin()) as i16;
                let bytes = s.to_le_bytes();
                chunk[i * 4..i * 4 + 2].copy_from_slice(&bytes);
                chunk[i * 4 + 2..i * 4 + 4].copy_from_slice(&bytes);
            }
            if let Err(err) = self.i2s.write_all(&chunk[..n * 4], I2S_WRITE_TIMEOUT_TICKS) {
                result = Err(anyhow!("I2S write failed: {err}"));
                break;
            }
            frame += n;
        }

        self.drain_and_disable();
        result
    }

    /// `write_all` returns once the last chunk is accepted into the DMA
    /// buffers, not once it's actually been clocked out - the DMA queue
    /// here (6 x 240 frames ~= 120ms at 16kHz) is far smaller than a typical
    /// tone, so disabling tx immediately would clip the tail.
    fn drain_and_disable(&mut self) {
        thread::sleep(Duration::from_millis(150));
        let _ = self.i2s.tx_disable();
        let _ = self.pa_enable.set_low();
    }
}

/// Ready-made 16kHz/16-bit Philips I2S config matching [`SAMPLE_RATE_HZ`].
pub fn i2s_std_config() -> StdConfig {
    StdConfig::philips(SAMPLE_RATE_HZ, DataBitWidth::Bits16)
}
