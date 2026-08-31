//! PCF8563 alarm compare fields, moved out of `rust-firmware/src/rtc.rs`
//! (which re-exports them) so the host-testable application state machine
//! can reference the RTC programming shape without pulling in the I2C
//! driver.

/// PCF8563 alarm compare fields. `None` sets that field's AE bit, which
/// means "ignored in the match" - e.g. `day: None, weekday: None` with
/// `minute`/`hour` set fires every day at that time; `day: Some(d)` fires
/// once on that day-of-month instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlarmRegs {
    pub minute: u8,
    pub hour: u8,
    pub day: Option<u8>,
    pub weekday: Option<u8>,
}
