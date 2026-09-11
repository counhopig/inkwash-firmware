use std::collections::VecDeque;

use crate::app::Event;

pub const HIGH_CAPACITY: usize = 16;

pub const NORMAL_CAPACITY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,

    Mergeable,
}

pub fn priority(event: &Event) -> Priority {
    match event {
        Event::Tick(_) => Priority::Mergeable,

        Event::Boot(_)
        | Event::Button(_)
        | Event::RtcAlarmSnapshotReady(_)
        | Event::UsbCommand(_)
        | Event::BleCommand(_)
        | Event::BlePairingStarted
        | Event::BlePairingSucceeded(_)
        | Event::BlePairingFailed(_)
        | Event::ReminderDue(_)
        | Event::ReminderFacts(_)
        | Event::BleDisconnected
        | Event::SyncCompleted(_)
        | Event::UrgentPollCompleted { .. }
        | Event::UrgentPollFailed
        | Event::SyncBoundaryDue
        | Event::SyncSchedulerConfigured(_)
        | Event::SetWifiVerified(_)
        | Event::WifiConfigApplied(_)
        | Event::SleepPrepared { .. }
        | Event::SleepCommitted(_)
        | Event::SleepCancelled(_)
        | Event::PowerPoll(_)
        | Event::EffectCompleted(_)
        | Event::EffectFailed(_) => Priority::High,
    }
}

#[derive(Debug)]
pub struct EventQueue {
    high: VecDeque<Event>,
    mergeable: VecDeque<Event>,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            high: VecDeque::with_capacity(HIGH_CAPACITY),
            mergeable: VecDeque::with_capacity(NORMAL_CAPACITY),
        }
    }

    #[allow(clippy::result_large_err)]
    pub fn try_push(&mut self, event: Event) -> Result<(), Event> {
        match priority(&event) {
            Priority::High => {
                if self.high.len() >= HIGH_CAPACITY {
                    return Err(event);
                }
                self.high.push_back(event);
                Ok(())
            }
            Priority::Mergeable => {
                self.push_mergeable(event);
                Ok(())
            }
        }
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn try_push_front(&mut self, event: Event) -> Result<(), Event> {
        match priority(&event) {
            Priority::High => {
                if self.high.len() >= HIGH_CAPACITY {
                    return Err(event);
                }
                self.high.push_front(event);
                Ok(())
            }
            Priority::Mergeable => {
                self.mergeable
                    .retain(|queued| !matches!(queued, Event::Tick(_)));
                self.mergeable.push_front(event);
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub fn push(&mut self, event: Event) {
        self.try_push(event).expect("test queue fixture must fit");
    }

    fn push_mergeable(&mut self, event: Event) {
        match &event {
            Event::Tick(_) => {
                self.mergeable.retain(|e| !matches!(e, Event::Tick(_)));
                self.mergeable.push_back(event);
            }
            _ => {
                while self.mergeable.len() >= NORMAL_CAPACITY {
                    self.mergeable.pop_front();
                }
                self.mergeable.push_back(event);
            }
        }
    }

    pub fn pop(&mut self) -> Option<Event> {
        self.high.pop_front().or_else(|| self.mergeable.pop_front())
    }

    pub fn is_empty(&self) -> bool {
        self.high.is_empty() && self.mergeable.is_empty()
    }

    pub fn len(&self) -> usize {
        self.high.len() + self.mergeable.len()
    }

    pub fn high_len(&self) -> usize {
        self.high.len()
    }

    pub fn mergeable_len(&self) -> usize {
        self.mergeable.len()
    }

    pub fn has_pending_tick(&self) -> bool {
        self.mergeable.iter().any(|e| matches!(e, Event::Tick(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        EffectBatchId, EffectCompletion, EffectError, EffectFailure, EffectId, EffectOutput,
        OperationId,
    };
    use crate::button_event::{ButtonEvent, ButtonId};
    use crate::datetime::DateTime;
    use crate::protocol::Command;
    use crate::wake_cause::WakeCause;

    fn dt(hour: u8, minute: u8) -> DateTime {
        DateTime {
            year: 2026,
            month: 8,
            day: 31,
            weekday: 1,
            hour,
            minute,
            second: 0,
            voltage_low: false,
        }
    }

    fn boot() -> Event {
        Event::Boot(crate::app::BootSnapshot {
            wake_cause: WakeCause::Other,
            now: Some(dt(8, 0)),
            rtc_alarm_flag: false,
            rtc_alarm_interrupt_enabled: false,
            alarms: vec![],
            todos: vec![],
            inbox: vec![],
            config: crate::device_config::DeviceConfig {
                server_url: String::new(),
                auth_token: String::new(),
            },
            status: crate::app::DeviceStatus::default(),
        })
    }

    fn completion() -> Event {
        Event::EffectCompleted(EffectCompletion {
            batch_id: EffectBatchId(1),
            effect_id: EffectId(1),
            operation_id: OperationId(1),
            render_generation: None,
            output: EffectOutput::AckDone,
        })
    }

    fn failure() -> Event {
        Event::EffectFailed(EffectFailure {
            batch_id: EffectBatchId(1),
            effect_id: EffectId(1),
            operation_id: OperationId(1),
            render_generation: None,
            error: EffectError::Ack("i2c".into()),
        })
    }

    fn critical_at(i: usize) -> Event {
        match i % 6 {
            0 => Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            1 => Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
            2 => Event::Button(ButtonEvent::Released(ButtonId::Down)),
            3 => Event::UsbCommand(Command::GetStatus),
            4 => Event::BleCommand(Command::ClearAlarms),
            _ => completion(),
        }
    }

    #[test]
    fn consecutive_ticks_coalesce_to_latest() {
        let mut q = EventQueue::new();
        q.push(Event::Tick(dt(9, 0)));
        q.push(Event::Tick(dt(9, 1)));
        q.push(Event::Tick(dt(9, 2)));
        assert_eq!(q.len(), 1, "ticks collapse to one slot");
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 2))));
        assert!(q.is_empty());
    }

    #[test]
    fn high_priority_events_are_not_displaced_by_a_tick_flood() {
        let mut q = EventQueue::new();
        for minute in 0..64u8 {
            q.push(Event::Tick(dt(9, minute)));
            if minute == 8 {
                q.push(Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
            }
            if minute == 16 {
                q.push(Event::RtcAlarmSnapshotReady(crate::app::RtcAlarmSnapshot {
                    now: dt(9, 16),
                    alarm_flag: true,
                    alarm_interrupt_enabled: true,
                }));
            }
            if minute == 32 {
                q.push(Event::UsbCommand(Command::GetStatus));
            }
            if minute == 48 {
                q.push(completion());
            }
        }

        assert_eq!(q.high_len(), 4);
        assert_eq!(q.mergeable_len(), 1);
        assert_eq!(
            q.pop(),
            Some(Event::Button(ButtonEvent::Pressed(ButtonId::Enter))),
            "critical events pop in FIFO order, ahead of the merged tick"
        );
        assert_eq!(
            q.pop(),
            Some(Event::RtcAlarmSnapshotReady(crate::app::RtcAlarmSnapshot {
                now: dt(9, 16),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }))
        );
        assert_eq!(q.pop(), Some(Event::UsbCommand(Command::GetStatus)));
        assert_eq!(q.pop(), Some(completion()));
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 63))));
        assert!(q.is_empty());
    }

    #[test]
    fn boot_and_completions_are_high_priority() {
        let mut q = EventQueue::new();
        q.push(Event::Tick(dt(9, 0)));
        q.push(boot());
        q.push(failure());
        q.push(Event::Tick(dt(9, 1)));
        assert_eq!(q.high_len(), 2, "boot and effect failure are critical");
        assert_eq!(q.pop(), Some(boot()));
        assert_eq!(q.pop(), Some(failure()));
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 1))));
    }

    #[test]
    fn sync_completion_is_high_priority() {
        let mut q = EventQueue::new();
        q.push(Event::SyncCompleted(crate::app::SyncResult::Failed(
            "timeout".into(),
        )));
        assert_eq!(q.high_len(), 1);
        assert_eq!(
            q.pop(),
            Some(Event::SyncCompleted(crate::app::SyncResult::Failed(
                "timeout".into()
            )))
        );
    }

    #[test]
    fn drained_queue_pops_none() {
        let mut q = EventQueue::new();
        assert_eq!(q.pop(), None);
        q.push(Event::Tick(dt(9, 0)));
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 0))));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn has_pending_tick_distinguishes_stale_tick_from_real_work() {
        let mut q = EventQueue::new();
        q.push(Event::Tick(dt(9, 0)));
        assert!(q.has_pending_tick());
        q.push(Event::Button(ButtonEvent::Released(ButtonId::Enter)));
        assert!(q.has_pending_tick());

        assert!(matches!(q.pop(), Some(Event::Button(_))));
        assert!(q.has_pending_tick());
        assert!(matches!(q.pop(), Some(Event::Tick(_))));
        assert!(!q.has_pending_tick());
    }

    #[test]
    fn protocol_commands_on_both_transports_are_high_priority() {
        let mut q = EventQueue::new();
        q.push(Event::UsbCommand(Command::SyncNow));
        q.push(Event::BleCommand(Command::ClearAlarms));
        q.push(Event::Tick(dt(9, 0)));
        assert_eq!(q.high_len(), 2);
        assert!(matches!(q.pop(), Some(Event::UsbCommand(_))));
        assert!(matches!(q.pop(), Some(Event::BleCommand(_))));
        assert!(matches!(q.pop(), Some(Event::Tick(_))));
    }

    #[test]
    fn reply_payload_roundtrip_through_queue_is_lossless() {
        let mut q = EventQueue::new();
        let reply = Event::EffectCompleted(EffectCompletion {
            batch_id: EffectBatchId(7),
            effect_id: EffectId(3),
            operation_id: OperationId(9),
            render_generation: None,
            output: EffectOutput::Persisted(crate::app::PersistTarget::Alarms),
        });
        q.push(reply.clone());
        q.push(Event::Tick(dt(9, 5)));
        assert_eq!(q.pop(), Some(reply));
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 5))));
    }

    #[test]
    fn try_push_returns_backpressure_when_high_tier_is_full() {
        let mut q = EventQueue::new();

        for i in 0..HIGH_CAPACITY {
            assert!(q.try_push(critical_at(i)).is_ok(), "push {i} must fit");
        }
        assert_eq!(q.high_len(), HIGH_CAPACITY);

        let ev = Event::UsbCommand(Command::GetStatus);
        assert_eq!(
            q.try_push(ev.clone()),
            Err(ev.clone()),
            "critical event must be returned, not dropped"
        );
        assert_eq!(
            q.high_len(),
            HIGH_CAPACITY,
            "queue must not grow past capacity"
        );

        assert_eq!(ev, Event::UsbCommand(Command::GetStatus));
    }

    #[test]
    fn try_push_never_rejects_mergeable_ticks() {
        let mut q = EventQueue::new();
        for minute in 0..64u8 {
            q.try_push(Event::Tick(dt(9, minute))).unwrap();
        }

        assert_eq!(q.mergeable_len(), 1);
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 63))));
        assert!(q.is_empty());
    }

    #[test]
    fn critical_events_preserved_under_high_tier_saturation() {
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY - 1 {
            q.push(critical_at(i));
        }

        q.push(completion());
        assert_eq!(q.high_len(), HIGH_CAPACITY);

        let critical = Event::UsbCommand(Command::GetStatus);
        assert_eq!(q.try_push(critical.clone()), Err(critical));

        for i in 0..HIGH_CAPACITY - 1 {
            assert_eq!(q.pop(), Some(critical_at(i)));
        }
        assert_eq!(q.pop(), Some(completion()));
        assert!(q.is_empty());
    }

    #[test]
    fn tick_flood_coalesces_to_latest_under_capacity() {
        let mut q = EventQueue::new();
        for minute in 0..1000u16 {
            q.try_push(Event::Tick(dt((minute % 24) as u8, (minute % 60) as u8)))
                .unwrap();
        }
        assert_eq!(q.mergeable_len(), 1);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn keypress_release_not_lost_after_full_high_tier() {
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY {
            q.push(critical_at(i));
        }
        for _ in 0..HIGH_CAPACITY {
            q.pop().unwrap();
        }

        q.push(Event::Button(ButtonEvent::Released(ButtonId::Enter)));
        assert_eq!(q.high_len(), 1);
        assert_eq!(
            q.pop(),
            Some(Event::Button(ButtonEvent::Released(ButtonId::Enter)))
        );
    }

    #[test]
    fn push_reports_saturated_critical_tier_without_dropping() {
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY {
            assert!(q.try_push(critical_at(i)).is_ok());
        }
        let event = Event::BleDisconnected;
        assert_eq!(q.try_push(event.clone()), Err(event));
        assert_eq!(q.high_len(), HIGH_CAPACITY);
    }

    #[test]
    fn consecutive_critical_overflow_events_remain_owned_until_retry() {
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY {
            assert!(q.try_push(critical_at(i)).is_ok());
        }

        let mut retained = Vec::new();
        for event in [
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            Event::BleDisconnected,
            completion(),
        ] {
            retained.push(q.try_push(event).expect_err("full queue returns ownership"));
        }
        assert_eq!(retained.len(), 3);
        assert_eq!(q.high_len(), HIGH_CAPACITY);

        for event in retained {
            let event = q.try_push(event).expect_err("queue is still full");
            q.pop().expect("draining one slot makes retry possible");
            q.try_push(event).expect("owned event retries without loss");
        }
        assert_eq!(q.high_len(), HIGH_CAPACITY);
    }
}
