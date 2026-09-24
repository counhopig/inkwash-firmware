//! Runtime counters for conditions that are handled and therefore quiet.
//!
//! The taxonomy (which counters exist, how they render) lives in
//! `inkwash_logic::diag`; this module is the storage. Relaxed atomic slots make
//! recording safe from the main task, the EPD worker and the BLE worker without
//! a lock, and nothing here touches NVS: a queue that saturates ten thousand
//! times costs ten thousand atomic increments and no flash wear.

use std::sync::atomic::{AtomicU32, Ordering};

use inkwash_logic::diag::{DiagCounter, DIAG_COUNTER_COUNT};

static COUNTERS: [AtomicU32; DIAG_COUNTER_COUNT] =
    [const { AtomicU32::new(0) }; DIAG_COUNTER_COUNT];

/// Counts one occurrence, saturating at `u32::MAX` so a wedged producer cannot
/// wrap a counter back to zero and hide itself.
pub fn record(counter: DiagCounter) {
    let _ =
        COUNTERS[counter.as_index()].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            (value < u32::MAX).then_some(value + 1)
        });
}

/// Classifies a failed I2C transaction so a bus that is held (timeouts) can
/// be told apart from a device that is absent or NACKing.
pub fn record_i2c(err: &esp_idf_svc::sys::EspError) {
    if err.code() == esp_idf_svc::sys::ESP_ERR_TIMEOUT {
        record(DiagCounter::I2cTimeout);
    } else {
        record(DiagCounter::I2cError);
    }
}

pub fn snapshot() -> [u32; DIAG_COUNTER_COUNT] {
    let mut counts = [0u32; DIAG_COUNTER_COUNT];
    for (index, slot) in COUNTERS.iter().enumerate() {
        counts[index] = slot.load(Ordering::Relaxed);
    }
    counts
}
