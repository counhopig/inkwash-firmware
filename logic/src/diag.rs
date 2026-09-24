//! Counters for conditions that are handled and therefore invisible.
//!
//! Queue saturation, BLE reply retries and EPD partial-refresh fallbacks are
//! all by design: the producer retries, the display repaints fully, the command
//! still lands. A device that does that constantly looks identical to a healthy
//! one except for being slower, and there is no error to log. These counters
//! make the pattern countable.
//!
//! The set is closed and bounded — a fixed list of `u32` slots, incremented by
//! the firmware with saturating atomics and never written to NVS. Only the
//! names are shared with the log, so a diagnostic line can never carry a
//! credential or a payload.

/// Conditions worth counting at runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DiagCounter {
    /// A USB reply found the producer mailbox full and had to be retained.
    UsbReplyQueueFull,

    /// A BLE reply found the delivery mailbox (and its latch) full.
    BleReplyQueueFull,

    /// The BLE worker's own command or pending-reply table was full.
    BleWorkerQueueFull,

    /// An effect batch found the persistence worker's queue full.
    EffectQueueFull,

    /// The EPD worker's completion mailbox was full.
    EpdQueueFull,

    /// The application event queue saturated and had to drop or merge work.
    RuntimeQueueFull,

    /// A BLE notification failed and was queued for another attempt.
    BleReplyRetry,

    /// A BLE reply was given up on after its retries were spent.
    BleReplyDrop,

    /// A partial EPD refresh failed and the same frame was pushed as a full
    /// refresh.
    EpdFullFallback,

    /// An I2C transaction on the shared RTC/codec/NFC bus timed out: the bus
    /// may be held (SDA stuck low) rather than a device merely NACKing.
    I2cTimeout,

    /// An I2C transaction failed for any other reason (NACK, invalid state).
    I2cError,
}

impl DiagCounter {
    pub const ALL: [DiagCounter; 11] = [
        DiagCounter::UsbReplyQueueFull,
        DiagCounter::BleReplyQueueFull,
        DiagCounter::BleWorkerQueueFull,
        DiagCounter::EffectQueueFull,
        DiagCounter::EpdQueueFull,
        DiagCounter::RuntimeQueueFull,
        DiagCounter::BleReplyRetry,
        DiagCounter::BleReplyDrop,
        DiagCounter::EpdFullFallback,
        DiagCounter::I2cTimeout,
        DiagCounter::I2cError,
    ];

    /// Slot in the firmware's counter array.
    pub const fn as_index(self) -> usize {
        self as usize
    }

    /// Name used in the diagnostic log; no spaces, so the rendered line stays a
    /// comma-separated list that is easy to read and to parse.
    pub const fn name(self) -> &'static str {
        match self {
            DiagCounter::UsbReplyQueueFull => "usb_reply_queue_full",
            DiagCounter::BleReplyQueueFull => "ble_reply_queue_full",
            DiagCounter::BleWorkerQueueFull => "ble_worker_queue_full",
            DiagCounter::EffectQueueFull => "effect_queue_full",
            DiagCounter::EpdQueueFull => "epd_queue_full",
            DiagCounter::RuntimeQueueFull => "runtime_queue_full",
            DiagCounter::BleReplyRetry => "ble_reply_retry",
            DiagCounter::BleReplyDrop => "ble_reply_drop",
            DiagCounter::EpdFullFallback => "epd_full_fallback",
            DiagCounter::I2cTimeout => "bus_timeout",
            DiagCounter::I2cError => "bus_error",
        }
    }
}

/// Number of slots a counter snapshot must have.
pub const DIAG_COUNTER_COUNT: usize = DiagCounter::ALL.len();

/// Renders the counters that moved since `previous`, or `None` when nothing
/// did — so a quiet device produces no diagnostic output at all.
///
/// Snapshots of the wrong size are rejected instead of indexing out of bounds:
/// a mismatch means the caller paired two different generations of counters,
/// and a silently truncated line would be worse than none.
pub fn render_delta(previous: &[u32], current: &[u32]) -> Option<String> {
    if previous.len() != DIAG_COUNTER_COUNT || current.len() != DIAG_COUNTER_COUNT {
        return None;
    }
    let mut line = String::new();
    for counter in DiagCounter::ALL {
        let index = counter.as_index();
        let delta = current[index].saturating_sub(previous[index]);
        if delta == 0 {
            continue;
        }
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(counter.name());
        line.push_str(" +");
        line.push_str(&delta.to_string());
        line.push_str(" (total ");
        line.push_str(&current[index].to_string());
        line.push(')');
    }
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(fill: u32) -> Vec<u32> {
        vec![fill; DIAG_COUNTER_COUNT]
    }

    fn with(counter: DiagCounter, value: u32) -> Vec<u32> {
        let mut snapshot = snapshot(0);
        snapshot[counter.as_index()] = value;
        snapshot
    }

    #[test]
    fn a_quiet_device_renders_nothing() {
        assert_eq!(render_delta(&snapshot(0), &snapshot(0)), None);
        assert_eq!(render_delta(&snapshot(7), &snapshot(7)), None);
    }

    #[test]
    fn only_the_counters_that_moved_are_named() {
        let line = render_delta(&snapshot(0), &with(DiagCounter::EpdFullFallback, 1))
            .expect("a moved counter must be reported");
        assert!(line.contains(DiagCounter::EpdFullFallback.name()));
        for counter in DiagCounter::ALL {
            if counter != DiagCounter::EpdFullFallback {
                assert!(
                    !line.contains(counter.name()),
                    "`{line}` must not mention an unchanged counter"
                );
            }
        }
    }

    #[test]
    fn the_line_reports_the_increment_and_the_running_total() {
        let previous = with(DiagCounter::BleReplyRetry, 4);
        let current = with(DiagCounter::BleReplyRetry, 7);
        assert_eq!(
            render_delta(&previous, &current).as_deref(),
            Some("ble_reply_retry +3 (total 7)")
        );
    }

    #[test]
    fn a_counter_that_did_not_grow_is_not_reported_as_a_huge_number() {
        let previous = with(DiagCounter::RuntimeQueueFull, 9);
        let current = with(DiagCounter::RuntimeQueueFull, 2);
        assert_eq!(render_delta(&previous, &current), None);
    }

    #[test]
    fn snapshots_of_the_wrong_size_are_rejected() {
        assert_eq!(render_delta(&[], &snapshot(0)), None);
        assert_eq!(render_delta(&snapshot(0), &snapshot(0)[..2]), None);
        let mut oversized = snapshot(0);
        oversized.push(1);
        assert_eq!(render_delta(&snapshot(0), &oversized), None);
    }

    #[test]
    fn every_counter_has_its_own_slot_and_a_log_safe_name() {
        let mut indexes: Vec<usize> = DiagCounter::ALL.iter().map(|c| c.as_index()).collect();
        let mut names: Vec<&str> = DiagCounter::ALL.iter().map(|c| c.name()).collect();
        for counter in DiagCounter::ALL {
            assert!(counter.as_index() < DIAG_COUNTER_COUNT);
            let name = counter.name();
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "`{name}` must stay a single bare token in the log line"
            );
        }
        indexes.sort_unstable();
        indexes.dedup();
        names.sort_unstable();
        names.dedup();
        assert_eq!(indexes.len(), DIAG_COUNTER_COUNT);
        assert_eq!(names.len(), DIAG_COUNTER_COUNT);
    }
}
