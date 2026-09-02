//! Duplicate-snapshot latch for the RTC executor (migration stage 2).
//!
//! The architecture requires that while one AF (alarm flag) assertion is
//! being handled, repeated RTC snapshot requests are merged into a single
//! consistent read rather than hammering the I2C bus:
//!
//! ```text
//! 同一次 RTC AF 持续期间产生的重复 RTC snapshot 请求合并为一条，
//! 直到 ACK 或重新读取到 AF 已清除。
//! ```
//!
//! The state machine (`app.rs`) already ignores duplicate snapshots while
//! firing / waiting-to-rearm / residue-ACK-pending; this latch is the
//! *collection-side* dedupe so those ignored snapshots never become bus
//! traffic in the first place. It is pure and host-testable: the firmware's
//! `rtc_executor` task consults it around each real `Pcf8563` operation.
//!
//! Rules:
//!
//! - A consistent snapshot that observed AF set is latched.
//! - While latched, a snapshot request is answered from the cached read
//!   (no fresh I2C transaction).
//! - A successful ACK or disable (AF cleared by the executor) releases the
//!   latch.
//! - A separate read that observes AF clear also releases it.
//! - A failed ACK/disable keeps the latch (the edge may still be asserted;
//!   the retry ACK is the release point).
//! - Programming the next alarm does not release the latch: an asserted AF
//!   from the alarm that just fired is only released by its ACK.

use crate::app::RtcAlarmSnapshot;

/// What the executor should do for a snapshot request under the current
/// latch state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAction {
    /// No cached asserted-AF snapshot: read time/AF/AIE from the RTC.
    ReadFresh,
    /// A previous consistent read with AF set is still latched: answer
    /// from the cache without touching the bus.
    UseCached,
}

/// The collection-side duplicate-snapshot latch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RtcSnapshotLatch {
    cached: Option<RtcAlarmSnapshot>,
}

impl RtcSnapshotLatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// The action for the next snapshot request.
    pub fn action(&self) -> SnapshotAction {
        match &self.cached {
            Some(snapshot) if snapshot.alarm_flag => SnapshotAction::UseCached,
            // A cached snapshot with AF clear should never exist (cleared on
            // store), but treat it as no-latch defensively.
            _ => SnapshotAction::ReadFresh,
        }
    }

    /// The latched snapshot, when the action is [`SnapshotAction::UseCached`].
    pub fn cached(&self) -> Option<&RtcAlarmSnapshot> {
        self.cached.as_ref()
    }

    /// Record a fresh consistent read. Latches it when AF is set, clears
    /// the latch when AF is clear (a read observed the flag released).
    pub fn observe_snapshot(&mut self, snapshot: RtcAlarmSnapshot) {
        if snapshot.alarm_flag {
            self.cached = Some(snapshot);
        } else {
            self.cached = None;
        }
    }

    /// Release the latch after a successful ACK or disable. A failed one
    /// must not call this - the edge may still be asserted.
    pub fn on_ack_or_disable_success(&mut self) {
        self.cached = None;
    }

    /// A non-snapshot read observed the AF state; if clear, release any
    /// held latch so the next asserted edge is a fresh snapshot.
    pub fn observe_alarm_flag(&mut self, alarm_flag: bool) {
        if !alarm_flag {
            self.cached = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::DateTime;

    fn dt(hour: u8, minute: u8) -> DateTime {
        DateTime {
            year: 2026,
            month: 9,
            day: 2,
            weekday: 1,
            hour,
            minute,
            second: 0,
            voltage_low: false,
        }
    }

    fn snapshot(af: bool, aie: bool) -> RtcAlarmSnapshot {
        RtcAlarmSnapshot {
            now: dt(8, 0),
            alarm_flag: af,
            alarm_interrupt_enabled: aie,
        }
    }

    #[test]
    fn fresh_request_reads_when_no_latch() {
        let latch = RtcSnapshotLatch::new();
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn af_snapshot_latches_and_reuses_until_released() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        assert_eq!(latch.action(), SnapshotAction::UseCached);
        assert_eq!(latch.cached(), Some(&snapshot(true, true)));
    }

    #[test]
    fn af_clear_snapshot_does_not_latch() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(false, true));
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
        assert_eq!(latch.cached(), None);
    }

    #[test]
    fn ack_success_releases_the_latch() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.on_ack_or_disable_success();
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn disable_success_releases_the_latch() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.on_ack_or_disable_success();
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn read_observing_af_clear_releases_the_latch() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.observe_alarm_flag(false);
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn read_observing_af_still_set_keeps_the_latch() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.observe_alarm_flag(true);
        assert_eq!(latch.action(), SnapshotAction::UseCached);
    }

    #[test]
    fn a_new_asserted_edge_after_release_reads_fresh() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.on_ack_or_disable_success();
        // A later independent AF assertion must be a fresh read (it could
        // be a different alarm / a re-arm).
        latch.observe_alarm_flag(true);
        // The latch was cleared by the ACK; observing AF set again through
        // a non-snapshot read does not re-latch an old snapshot.
        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn fresh_consistent_read_after_clear_relatches() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.on_ack_or_disable_success();
        // The alarm was re-armed and fired again: a fresh consistent read
        // with AF set latches the new snapshot.
        let second = RtcAlarmSnapshot {
            now: dt(8, 5),
            alarm_flag: true,
            alarm_interrupt_enabled: true,
        };
        latch.observe_snapshot(second.clone());
        assert_eq!(latch.action(), SnapshotAction::UseCached);
        assert_eq!(latch.cached(), Some(&second));
    }

    #[test]
    fn default_is_empty() {
        assert_eq!(RtcSnapshotLatch::default(), RtcSnapshotLatch::new());
    }
}
