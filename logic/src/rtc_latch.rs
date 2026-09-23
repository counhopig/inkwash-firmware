use crate::app::RtcAlarmSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAction {
    ReadFresh,

    UseCached,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RtcSnapshotLatch {
    cached: Option<RtcAlarmSnapshot>,
}

impl RtcSnapshotLatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn action(&self) -> SnapshotAction {
        match &self.cached {
            Some(snapshot) if snapshot.alarm_flag => SnapshotAction::UseCached,

            _ => SnapshotAction::ReadFresh,
        }
    }

    pub fn cached(&self) -> Option<&RtcAlarmSnapshot> {
        self.cached.as_ref()
    }

    pub fn observe_snapshot(&mut self, snapshot: RtcAlarmSnapshot) {
        if snapshot.alarm_flag {
            self.cached = Some(snapshot);
        } else {
            self.cached = None;
        }
    }

    pub fn on_ack_or_disable_success(&mut self) {
        self.cached = None;
    }

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

        latch.observe_alarm_flag(true);

        assert_eq!(latch.action(), SnapshotAction::ReadFresh);
    }

    #[test]
    fn fresh_consistent_read_after_clear_relatches() {
        let mut latch = RtcSnapshotLatch::new();
        latch.observe_snapshot(snapshot(true, true));
        latch.on_ack_or_disable_success();

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
