use crate::app::InboxState;
use crate::inbox_item::{InboxKind, Priority};

pub fn boundary_index(unix: u64, period_secs: u64) -> u64 {
    unix / period_secs
}

pub fn boundary_changed(unix: u64, period_secs: u64, last_seen: u64) -> bool {
    boundary_index(unix, period_secs) != last_seen
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncScheduler {
    last_urgent_boundary: u64,
    last_full_boundary: u64,
    interval_minutes: u16,
    never_synced: bool,
    urgent_synced: bool,
    urgent_boundary_before_advance: Option<u64>,
    full_boundary_before_advance: Option<u64>,
}

impl SyncScheduler {
    pub fn new(now_unix: u64, sync_interval_minutes: u16, last_sync_epoch: Option<u64>) -> Self {
        let interval = sync_interval_minutes.max(1) as u64;
        Self {
            last_urgent_boundary: boundary_index(now_unix, 30),
            last_full_boundary: boundary_index(now_unix, interval * 60),
            interval_minutes: interval as u16,
            never_synced: last_sync_epoch.is_none(),
            urgent_synced: false,
            urgent_boundary_before_advance: None,
            full_boundary_before_advance: None,
        }
    }

    pub fn already_synced_urgent(&self, inbox: &InboxState) -> bool {
        self.urgent_synced && Self::has_unread_urgent(inbox)
    }

    pub fn has_unread_urgent(inbox: &InboxState) -> bool {
        inbox
            .items
            .iter()
            .any(|it| !it.read && it.priority == Priority::High && it.kind == InboxKind::Alert)
    }

    pub fn full_due(&self, unix: u64, sync_interval_minutes: u16) -> bool {
        let period = sync_interval_minutes.max(1) as u64 * 60;
        boundary_changed(unix, period, self.last_full_boundary)
    }

    pub fn interval_minutes(&self) -> u16 {
        self.interval_minutes
    }

    pub fn set_interval_minutes(&mut self, unix: u64, minutes: u16) {
        let interval = minutes.max(1);
        self.interval_minutes = interval;
        self.last_full_boundary = boundary_index(unix, interval as u64 * 60);
    }

    pub fn urgent_due(&self, unix: u64) -> bool {
        boundary_changed(unix, 30, self.last_urgent_boundary)
    }

    pub fn advance_full_boundary(&mut self, unix: u64, sync_interval_minutes: u16) {
        let period = sync_interval_minutes.max(1) as u64 * 60;
        self.full_boundary_before_advance = Some(self.last_full_boundary);
        self.last_full_boundary = boundary_index(unix, period);
    }

    pub fn rollback_full_boundary(&mut self) {
        if let Some(previous) = self.full_boundary_before_advance.take() {
            self.last_full_boundary = previous;
        }
    }

    pub fn advance_urgent_boundary(&mut self, unix: u64) {
        self.urgent_boundary_before_advance = Some(self.last_urgent_boundary);
        self.last_urgent_boundary = boundary_index(unix, 30);
    }

    pub fn rollback_urgent_boundary(&mut self) {
        if let Some(previous) = self.urgent_boundary_before_advance.take() {
            self.last_urgent_boundary = previous;
        }
    }

    pub fn is_never_synced(&self) -> bool {
        self.never_synced
    }

    pub fn mark_synced(&mut self) {
        self.never_synced = false;
    }

    pub fn mark_urgent_synced(&mut self) {
        self.urgent_synced = true;
    }

    pub fn clear_urgent_synced(&mut self) {
        self.urgent_synced = false;
    }

    pub fn mark_full_sync_completed(&mut self) {
        self.never_synced = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_item::{InboxItem, InboxKind};

    fn urgent_item(id: u64, read: bool) -> InboxItem {
        InboxItem {
            id,
            kind: InboxKind::Alert,
            priority: Priority::High,
            title: "urgent".into(),
            body: String::new(),
            when: None,
            read,
        }
    }

    fn make_inbox(items: Vec<InboxItem>) -> InboxState {
        InboxState { items }
    }

    #[test]
    fn boundary_index_groups_seconds_within_one_period() {
        assert_eq!(boundary_index(0, 30), 0);
        assert_eq!(boundary_index(29, 30), 0);
        assert_eq!(boundary_index(30, 30), 1);
        assert_eq!(boundary_index(59, 30), 1);
        assert_eq!(boundary_index(60, 30), 2);
    }

    #[test]
    fn boundary_changed_is_false_within_the_same_period() {
        let last_seen = boundary_index(100, 30);
        assert!(!boundary_changed(101, 30, last_seen));
        assert!(!boundary_changed(119, 30, last_seen));
    }

    #[test]
    fn boundary_changed_is_true_once_the_index_changes() {
        let last_seen = boundary_index(100, 30);
        assert!(boundary_changed(130, 30, last_seen));
    }

    #[test]
    fn boundary_changed_is_true_when_the_clock_moves_backwards() {
        let last_seen = boundary_index(300, 30);
        assert!(boundary_changed(270, 30, last_seen));
    }

    #[test]
    fn hourly_boundary_aligns_to_wall_clock_not_boot_time() {
        assert_eq!(boundary_index(1, 3600), boundary_index(3599, 3600));
        assert_ne!(boundary_index(3599, 3600), boundary_index(3600, 3600));
    }

    #[test]
    fn a_never_seen_boundary_index_of_zero_is_not_special_cased() {
        assert!(boundary_changed(3600, 3600, 0));
        assert!(!boundary_changed(0, 3600, 0));
    }

    #[test]
    fn already_synced_urgent_skips_when_unread_present() {
        let sched = SyncScheduler::new(100, 60, Some(50));
        let mut s = sched.clone();
        s.mark_urgent_synced();
        let inbox = make_inbox(vec![urgent_item(1, false)]);
        assert!(
            s.already_synced_urgent(&inbox),
            "already synced + unread → skip"
        );
    }

    #[test]
    fn already_synced_urgent_falls_through_when_all_read() {
        let mut s = SyncScheduler::new(100, 60, Some(50));
        s.mark_urgent_synced();
        let inbox = make_inbox(vec![urgent_item(1, true)]);
        assert!(!s.already_synced_urgent(&inbox), "all read → not redundant");
    }

    #[test]
    fn already_synced_urgent_falls_through_before_first_sync() {
        let s = SyncScheduler::new(100, 60, Some(50));

        let inbox = make_inbox(vec![urgent_item(1, false)]);
        assert!(
            !s.already_synced_urgent(&inbox),
            "not yet synced → not redundant"
        );
    }

    #[test]
    fn never_synced_is_true_on_fresh_device() {
        let s = SyncScheduler::new(0, 60, None);
        assert!(s.is_never_synced());
    }

    #[test]
    fn never_synced_is_false_after_epoch_set() {
        let s = SyncScheduler::new(100, 60, Some(100));
        assert!(!s.is_never_synced());
    }

    #[test]
    fn mark_synced_clears_never_synced() {
        let mut s = SyncScheduler::new(0, 60, None);
        assert!(s.is_never_synced());
        s.mark_synced();
        assert!(!s.is_never_synced());
    }

    #[test]
    fn clear_urgent_synced_resets_flag() {
        let mut s = SyncScheduler::new(100, 60, Some(50));
        s.mark_urgent_synced();
        s.clear_urgent_synced();
        let inbox = make_inbox(vec![urgent_item(1, false)]);
        assert!(
            !s.already_synced_urgent(&inbox),
            "after clear → not redundant"
        );
    }

    #[test]
    fn full_due_detects_boundary_advancement() {
        let s = SyncScheduler::new(0, 60, None);
        assert!(s.full_due(3600, 60), "3600 > 0 → full sync due");
        assert!(!s.full_due(3599, 60), "within same hour → not due");
    }

    #[test]
    fn advance_full_boundary_resets_due_state() {
        let mut s = SyncScheduler::new(0, 60, None);
        assert!(s.full_due(3600, 60));
        s.advance_full_boundary(3600, 60);
        assert!(!s.full_due(7199, 60), "same hour as last advance → not due");
        assert!(s.full_due(7200, 60), "next hour → due again");
    }

    #[test]
    fn rollback_restores_advanced_boundaries() {
        let mut s = SyncScheduler::new(0, 60, None);
        s.advance_full_boundary(3600, 60);
        s.advance_urgent_boundary(30);
        assert!(!s.full_due(7199, 60), "same hour as the advance → not due");
        assert!(
            !s.urgent_due(59),
            "same 30s window as the advance → not due"
        );
        s.rollback_full_boundary();
        s.rollback_urgent_boundary();
        assert!(
            s.full_due(7199, 60),
            "the rolled-back hour is due for a retry again"
        );
        assert!(
            s.urgent_due(59),
            "the rolled-back 30s window is due for a retry again"
        );
    }

    #[test]
    fn urgent_due_detects_30s_boundary() {
        let s = SyncScheduler::new(0, 60, None);
        assert!(s.urgent_due(30), "30 > 0 → urgent due");
        assert!(!s.urgent_due(29), "within same 30s window → not due");
    }

    #[test]
    fn clock_rollback_re_aligns_the_urgent_cursor_instead_of_stalling() {
        let mut s = SyncScheduler::new(300, 60, Some(300));

        assert!(s.urgent_due(270));
        assert!(
            !s.full_due(270, 60),
            "a rollback inside the same hour is not a full-sync boundary"
        );
        s.advance_urgent_boundary(270);
        assert!(
            !s.urgent_due(275),
            "same 30s window after the re-align → not due"
        );
        assert!(s.urgent_due(300), "the next real boundary → due again");
    }

    #[test]
    fn clock_rollback_re_arms_a_full_sync_after_a_long_backward_jump() {
        let mut s = SyncScheduler::new(7 * 24 * 3600, 60, Some(600));
        let rolled_back = 6 * 24 * 3600;
        assert!(
            s.full_due(rolled_back, 60),
            "the first sample after the jump re-aligns and syncs"
        );
        s.advance_full_boundary(rolled_back, 60);
        assert!(
            !s.full_due(rolled_back + 3599, 60),
            "re-aligned hour → not due"
        );
        assert!(
            s.full_due(rolled_back + 3600, 60),
            "the next aligned hour → due"
        );
    }

    #[test]
    fn urgent_poll_flow_marks_synced_and_skips_redundant() {
        let mut s = SyncScheduler::new(0, 60, Some(50));

        assert!(s.urgent_due(45));

        s.advance_urgent_boundary(45);
        s.mark_urgent_synced();

        let inbox = make_inbox(vec![urgent_item(1, false)]);
        assert!(
            s.already_synced_urgent(&inbox),
            "same unread urgent → redundant sync skipped"
        );

        let read_inbox = make_inbox(vec![urgent_item(1, true)]);
        assert!(
            !s.already_synced_urgent(&read_inbox),
            "message read → not redundant"
        );
    }
    #[test]
    fn has_unread_urgent_detects_high_priority_alerts() {
        let inbox = make_inbox(vec![
            urgent_item(1, false),
            InboxItem {
                id: 2,
                kind: InboxKind::Alert,
                priority: Priority::Normal,
                title: "normal".into(),
                body: String::new(),
                when: None,
                read: false,
            },
        ]);
        assert!(SyncScheduler::has_unread_urgent(&inbox));
    }

    #[test]
    fn has_unread_urgent_is_empty_for_no_items() {
        assert!(!SyncScheduler::has_unread_urgent(&make_inbox(vec![])));
    }
}
