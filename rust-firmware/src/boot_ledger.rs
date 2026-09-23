//! Retained boot ledger.
//!
//! The decision logic lives in `inkwash_logic::boot_guard`; this module is only
//! the storage for it. The record sits in RTC "noinit" memory: the section is
//! marked `NOLOAD`, so it is never initialized from the flash image, and the
//! RTC domain keeps its contents across a reset or a deep-sleep wake while a
//! real power cycle clears it. That is exactly the lifetime a boot-failure
//! counter needs — a power cycle is always a fresh start, a panic loop is not.

use std::ptr::{addr_of, addr_of_mut};

use esp_idf_svc::sys::{
    esp_reset_reason_t, esp_reset_reason_t_ESP_RST_BROWNOUT as ESP_RST_BROWNOUT,
    esp_reset_reason_t_ESP_RST_CPU_LOCKUP as ESP_RST_CPU_LOCKUP,
    esp_reset_reason_t_ESP_RST_DEEPSLEEP as ESP_RST_DEEPSLEEP,
    esp_reset_reason_t_ESP_RST_EXT as ESP_RST_EXT,
    esp_reset_reason_t_ESP_RST_INT_WDT as ESP_RST_INT_WDT,
    esp_reset_reason_t_ESP_RST_JTAG as ESP_RST_JTAG,
    esp_reset_reason_t_ESP_RST_PANIC as ESP_RST_PANIC,
    esp_reset_reason_t_ESP_RST_POWERON as ESP_RST_POWERON,
    esp_reset_reason_t_ESP_RST_PWR_GLITCH as ESP_RST_PWR_GLITCH,
    esp_reset_reason_t_ESP_RST_SW as ESP_RST_SW,
    esp_reset_reason_t_ESP_RST_TASK_WDT as ESP_RST_TASK_WDT,
    esp_reset_reason_t_ESP_RST_USB as ESP_RST_USB, esp_reset_reason_t_ESP_RST_WDT as ESP_RST_WDT,
};

use inkwash_logic::boot_guard::{BootLedger, ResetKind};

#[repr(C)]
#[derive(Clone, Copy)]
struct LedgerWords {
    magic: u32,
    failures: u32,
}

#[link_section = ".rtc_noinit"]
static mut LEDGER: LedgerWords = LedgerWords {
    magic: 0,
    failures: 0,
};

fn load() -> BootLedger {
    let words = unsafe { std::ptr::read_volatile(addr_of!(LEDGER)) };
    BootLedger::from_raw(words.magic, words.failures)
}

fn store(ledger: BootLedger) {
    let words = LedgerWords {
        magic: ledger.magic(),
        failures: ledger.failures(),
    };
    unsafe { std::ptr::write_volatile(addr_of_mut!(LEDGER), words) };
}

/// Records the attempt starting now and returns the updated ledger.
pub fn note_attempt(reset: ResetKind) -> BootLedger {
    let ledger = load().note_attempt(reset);
    store(ledger);
    ledger
}

/// Ends the run after the boot path finished initializing. The marker stays so
/// a later abnormal reset is still recognizable as part of this power session.
pub fn clear() {
    store(load().cleared());
}

pub fn reset_kind(reason: esp_reset_reason_t) -> ResetKind {
    match reason {
        ESP_RST_POWERON => ResetKind::PowerOn,
        ESP_RST_EXT => ResetKind::ExternalReset,
        ESP_RST_SW => ResetKind::SoftwareReset,
        ESP_RST_DEEPSLEEP => ResetKind::DeepSleep,
        ESP_RST_USB => ResetKind::Usb,
        ESP_RST_JTAG => ResetKind::Jtag,
        ESP_RST_PANIC => ResetKind::Panic,
        ESP_RST_TASK_WDT => ResetKind::TaskWatchdog,
        ESP_RST_INT_WDT => ResetKind::InterruptWatchdog,
        ESP_RST_WDT => ResetKind::Watchdog,
        ESP_RST_BROWNOUT => ResetKind::Brownout,
        ESP_RST_PWR_GLITCH => ResetKind::PowerGlitch,
        ESP_RST_CPU_LOCKUP => ResetKind::CpuLockup,
        _ => ResetKind::Other,
    }
}
