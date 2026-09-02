//! GT23SC6699 NFC tag bring-up: I2C block read (UID) + field-detect GPIO.
//! Pin/address/protocol values confirmed against the official ZECTRIX demo
//! (`zectrix_board_config.h`, `zectrix_nfc.cc`): I2C addr 0x55 on the same
//! I2C0 bus as the RTC/codec, power on GPIO21 (active-high), field-detect
//! on GPIO7 (active-LOW, i.e. a low level means a reader/phone is present).
//!
//! Scoped to hardware bring-up (probe + read UID + field state) rather than
//! the demo's full NDEF read/write support - that's content-layer work.

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::hal::gpio::{Input, Output, PinDriver};
use esp_idf_svc::sys::TickType_t;

use crate::board::SharedI2c;

pub const GT23SC6699_ADDR: u8 = 0x55;
pub const UID_BLOCK: u8 = 0x00;
pub const BLOCK_SIZE: usize = 16;
const I2C_TIMEOUT_TICKS: TickType_t = 100;
/// Delay between writing the block address and reading its data: the chip
/// needs time to prepare the block internally (matches the reference
/// driver's `kReadDelayUs`).
const READ_DELAY: Duration = Duration::from_millis(10);

pub struct NfcTag {
    i2c: SharedI2c,
    addr: u8,
    _power: PinDriver<'static, Output>,
    field_detect: PinDriver<'static, Input>,
}

impl NfcTag {
    pub fn new(
        i2c: SharedI2c,
        addr: u8,
        mut power: PinDriver<'static, Output>,
        field_detect: PinDriver<'static, Input>,
    ) -> Result<Self> {
        power
            .set_high()
            .context("failed to power on NFC (GPIO21)")?;
        thread::sleep(Duration::from_millis(1));

        let mut tag = Self {
            i2c,
            addr,
            _power: power,
            field_detect,
        };
        let mut probe_buf = [0u8; BLOCK_SIZE];
        tag.read_block(UID_BLOCK, &mut probe_buf)
            .context("GT23SC6699 not responding on I2C bus (addr 0x55)")?;
        Ok(tag)
    }

    /// True when an external NFC field (e.g. a phone) is currently in range.
    /// Not called anywhere yet - kept for whatever content-layer feature
    /// reacts to a tap.
    #[allow(dead_code)]
    pub fn field_present(&self) -> bool {
        self.field_detect.is_low()
    }

    /// Reads one 16-byte block. Two separate I2C transactions (write the
    /// block address, wait, then read) rather than a combined write-then-
    /// read - the chip needs `READ_DELAY` to prepare the block internally.
    pub fn read_block(&mut self, block_addr: u8, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        self.i2c
            .lock()
            .write(self.addr, &[block_addr], I2C_TIMEOUT_TICKS)
            .map_err(|e| anyhow!("NFC write block addr 0x{block_addr:02x} failed: {e}"))?;
        thread::sleep(READ_DELAY);
        self.i2c
            .lock()
            .read(self.addr, out, I2C_TIMEOUT_TICKS)
            .map_err(|e| anyhow!("NFC read block 0x{block_addr:02x} failed: {e}"))
    }

    /// Reads the first 7 bytes of the UID block (block 0). Not called
    /// anywhere yet - kept for whatever content-layer feature identifies
    /// tags.
    #[allow(dead_code)]
    pub fn read_uid(&mut self) -> Result<[u8; 7]> {
        let mut block = [0u8; BLOCK_SIZE];
        self.read_block(UID_BLOCK, &mut block)?;
        let mut uid = [0u8; 7];
        uid.copy_from_slice(&block[..7]);
        Ok(uid)
    }
}
