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

    #[allow(dead_code)]
    pub fn field_present(&self) -> bool {
        self.field_detect.is_low()
    }

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

    #[allow(dead_code)]
    pub fn read_uid(&mut self) -> Result<[u8; 7]> {
        let mut block = [0u8; BLOCK_SIZE];
        self.read_block(UID_BLOCK, &mut block)?;
        let mut uid = [0u8; 7];
        uid.copy_from_slice(&block[..7]);
        Ok(uid)
    }
}
