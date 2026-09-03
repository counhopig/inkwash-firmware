//! Tiered input queue for the unified application event loop (migration
//! step 2 of `docs/firmware-architecture.md`).
//!
//! The architecture splits incoming events into two classes:
//!
//! - **High-priority, never mergeable, never droppable**: RTC alarm
//!   snapshots, buttons, protocol requests, and effect completions or
//!   failures. Producers of these events apply backpressure when the tier
//!   is full (a GPIO ISR only latches the edge; the collector retries), so
//!   this tier is unbounded here and the firmware producer owns the
//!   blocking write.
//! - **Mergeable ordinary events**: facts that a newer event of the same
//!   kind fully replaces. Today that is exactly `Tick` (only the newest
//!   wall-clock time matters) - the class grows in later steps (duplicate
//!   RTC snapshot requests latch to one until the AF clears, stale render
//!   completions drop by generation).
//!
//! Keeping the tiers in one struct is what makes "critical events are not
//! squeezed out by ordinary events" structural instead of convention: a
//! flood of ticks collapses to a single slot and can never displace a
//! queued button or completion.
//!
//! Consumption order: the whole high-priority tier first (FIFO), then the
//! ordinary tier (FIFO, with the collapsed latest tick last). An ordinary
//! event is therefore allowed to be reordered *after* critical events that
//! arrived later; that is the documented trade of the two-tier model - a
//! late Tick carries no information a newer Tick does not.

use std::collections::VecDeque;

use crate::app::Event;

/// Which queue tier an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    /// Mergeable; a newer event of the same kind replaces the queued one.
    Mergeable,
}

/// Classification of every `Event` variant. One source of truth, so the
/// firmware's collection layer and the host tests agree on tiering.
pub fn priority(event: &Event) -> Priority {
    match event {
        // Mergeable facts: a newer one fully replaces the older.
        Event::Tick(_) => Priority::Mergeable,
        // Everything else is critical: alarm facts, input, protocol,
        // effect completions/failures, boot.
        Event::Boot(_)
        | Event::Button(_)
        | Event::RtcAlarmSnapshotReady(_)
        | Event::UsbCommand(_)
        | Event::BleCommand(_)
        | Event::BlePairingStarted
        | Event::BlePairingSucceeded(_)
        | Event::BlePairingFailed(_)
        | Event::BleDisconnected
        | Event::SyncCompleted(_)
        | Event::SyncBoundaryDue
        | Event::SetWifiCompleted(_)
        | Event::DisplayCompleted(_)
        | Event::EffectCompleted(_)
        | Event::EffectFailed(_)
        | Event::IdleDeadlineReached => Priority::High,
    }
}

/// The single application event queue: a high-priority FIFO plus a
/// mergeable FIFO with per-kind coalescing.
#[derive(Debug, Default)]
pub struct EventQueue {
    high: VecDeque<Event>,
    mergeable: VecDeque<Event>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classifies and enqueues one event. This is the only entry producers
    /// should use; the tier split (and its merge rules) stays internal.
    pub fn push(&mut self, event: Event) {
        match priority(&event) {
            Priority::High => self.high.push_back(event),
            Priority::Mergeable => self.push_mergeable(event),
        }
    }

    /// Mergeable tier: `Tick` coalesces to the latest received time (any
    /// older queued tick is dropped). Other mergeable kinds append.
    fn push_mergeable(&mut self, event: Event) {
        match &event {
            Event::Tick(_) => {
                self.mergeable.retain(|e| !matches!(e, Event::Tick(_)));
                self.mergeable.push_back(event);
            }
            _ => self.mergeable.push_back(event),
        }
    }

    /// Pops the next event to consume: everything high-priority first, then
    /// the mergeable tier. Returns `None` when the queue is drained.
    pub fn pop(&mut self) -> Option<Event> {
        self.high.pop_front().or_else(|| self.mergeable.pop_front())
    }

    pub fn is_empty(&self) -> bool {
        self.high.is_empty() && self.mergeable.is_empty()
    }

    /// Total queued events (merged ticks count once).
    pub fn len(&self) -> usize {
        self.high.len() + self.mergeable.len()
    }

    pub fn high_len(&self) -> usize {
        self.high.len()
    }

    pub fn mergeable_len(&self) -> usize {
        self.mergeable.len()
    }

    /// True while at least one mergeable `Tick` is pending (the firmware
    /// idle/sleep decision and the "queue not empty" wake check use this
    /// to tell "only a stale tick left" from "real work pending").
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
    use crate::button_event::ButtonEvent;
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
                q.push(Event::Button(ButtonEvent::Pressed));
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
        // Every critical event survived the 64-tick flood; only one tick
        // remains.
        assert_eq!(q.high_len(), 4);
        assert_eq!(q.mergeable_len(), 1);
        assert_eq!(
            q.pop(),
            Some(Event::Button(ButtonEvent::Pressed)),
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
    fn idle_deadline_and_sync_completion_are_high_priority() {
        let mut q = EventQueue::new();
        q.push(Event::IdleDeadlineReached);
        q.push(Event::SyncCompleted(crate::app::SyncResult::Failed(
            "timeout".into(),
        )));
        assert_eq!(q.high_len(), 2);
        assert_eq!(q.pop(), Some(Event::IdleDeadlineReached));
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
        q.push(Event::Button(ButtonEvent::Released));
        assert!(q.has_pending_tick());
        // Drain high first: the tick is what remains.
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
        // Reply-carrying events ride the queue verbatim (Clone must not
        // lose fields); a Busy reply queued ahead of a tick stays intact.
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
}
