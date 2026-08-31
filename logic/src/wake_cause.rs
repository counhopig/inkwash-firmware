//! Wake-cause vocabulary, moved out of `rust-firmware/src/power.rs` (which
//! re-exports it) so the host-testable application state machine can
//! reference how this boot began without the ESP-IDF sleep driver.

/// Who woke the chip up, disambiguated from `esp_sleep_get_wakeup_cause`'s
/// EXT1 case via `esp_sleep_get_ext1_wakeup_status`'s per-pin bitmask :
/// ENTER, DOWN, and the RTC alarm all share the same ext1 wake source, so
/// the cause alone doesn't tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeCause {
    Enter,
    RtcAlarm,
    Down,
    Other,
}
