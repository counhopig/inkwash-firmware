//! Tiered input queue for the unified application event loop (migration
//! step 2 of `docs/firmware-architecture.md`).
//!
//! The architecture splits incoming events into two classes:
//!
//! - **High-priority, never droppable**: RTC alarm snapshots, buttons,
//!   protocol requests, and effect completions or failures. The tier is
//!   bounded to `HIGH_CAPACITY`; when full, `try_push` returns the event as a
//!   backpressure signal so producers (GPIO ISR collector, RTC executor task,
//!   effect task) yield and retry instead of dropping a critical event.
//!   The capacity is large enough that steady-state firmware never hits it —
//!   saturation only occurs under pathological load.
//! - **Mergeable ordinary events**: facts that a newer event of the same kind
//!   fully replaces. Today that is exactly `Tick` (only the newest wall-clock
//!   time matters) — the class grows in later steps (duplicate RTC snapshot
//!   requests latch to one until the AF clears, stale render completions drop
//!   by generation).
//!
//! Keeping the tiers in one struct is what makes "critical events are not
//! squeezed out by ordinary events" structural instead of convention: a flood
//! of ticks collapses to a single slot and can never displace a queued button
//! or completion.
//!
//! Consumption order: the whole high-priority tier first (FIFO), then the
//! ordinary tier (FIFO, with the collapsed latest tick last). An ordinary
//! event is therefore allowed to be reordered *after* critical events that
//! arrived later; that is the documented trade of the two-tier model — a
//! late Tick carries no information a newer Tick does not.

use std::collections::VecDeque;

use crate::app::Event;

/// High-priority tier capacity. Saturated only under pathological load;
/// producers apply backpressure by retrying. Sized well above steady-state
/// in-flight critical events (RTC alarm, buttons, protocol, effect
/// completions) so it never blocks during normal operation.
pub const HIGH_CAPACITY: usize = 16;

/// Mergeable ordinary-tier capacity. `Tick` coalesces to one slot; this
/// capacity holds a tick plus a small margin of pending mergeable facts
/// without blocking producers.
pub const NORMAL_CAPACITY: usize = 4;

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

/// The single application event queue: a bounded high-priority FIFO plus a
/// bounded mergeable FIFO with per-kind coalescing.
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

    /// Attempts to enqueue one event, applying backpressure when the
    /// high-priority tier is full. Returns `Err(event)` if the critical tier
    /// is at capacity — the caller must retry (never drop).
    /// Mergeable events always succeed: a `Tick` coalesces with an existing
    /// one (dropping the older), and the mergeable tier is sized to hold at
    /// most one tick plus a small margin.
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

    /// Reinsert a fixed continuation ahead of events collected while an
    /// effect batch was in flight. This preserves the batch's completion
    /// order without allocating a second producer queue.
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

    /// Test-only fixture helper. Production code must use [`try_push`].
    #[cfg(test)]
    pub fn push(&mut self, event: Event) {
        self.try_push(event).expect("test queue fixture must fit");
    }

    /// Mergeable tier: `Tick` coalesces to the latest received time (any
    /// older queued tick is dropped). Other mergeable kinds append; if the
    /// tier is at capacity, the oldest non-tick mergeable event is evicted.
    /// `Tick` itself is always coalesced, never dropped.
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
    /// idle/sleep decision and the "queue not empty" wake check use this to
    /// tell "only a stale tick left" from "real work pending").
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

    /// Produces a distinct high-priority event for index `i`, cycling through
    /// button press/release, protocol commands, and completion/failure events
    /// to fill the high-priority tier.
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
        // Every critical event survived the 64-tick flood; only one tick
        // remains.
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

    // ---- backpressure: bounded capacity + try_push ----

    #[test]
    fn try_push_returns_backpressure_when_high_tier_is_full() {
        let mut q = EventQueue::new();
        // Fill the high-priority tier with distinct critical events.
        for i in 0..HIGH_CAPACITY {
            assert!(q.try_push(critical_at(i)).is_ok(), "push {i} must fit");
        }
        assert_eq!(q.high_len(), HIGH_CAPACITY);
        // One more critical event: backpressure.
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
        // The returned event is exactly the one that was rejected.
        assert_eq!(ev, Event::UsbCommand(Command::GetStatus));
    }

    #[test]
    fn try_push_never_rejects_mergeable_ticks() {
        // Even when the mergeable tier is "full of ticks", a new tick just
        // replaces the old one rather than rejecting.
        let mut q = EventQueue::new();
        for minute in 0..64u8 {
            q.try_push(Event::Tick(dt(9, minute))).unwrap();
        }
        // Only one tick should exist (coalesced).
        assert_eq!(q.mergeable_len(), 1);
        assert_eq!(q.pop(), Some(Event::Tick(dt(9, 63))));
        assert!(q.is_empty());
    }

    #[test]
    fn critical_events_preserved_under_high_tier_saturation() {
        // When the queue is full of critical events, a newly-arrived
        // critical event is returned as backpressure — but existing critical
        // events are never dropped or reordered.
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY - 1 {
            q.push(critical_at(i));
        }
        // Fill the last slot.
        q.push(completion());
        assert_eq!(q.high_len(), HIGH_CAPACITY);

        // One more would saturate — try_push returns backpressure.
        let critical = Event::UsbCommand(Command::GetStatus);
        assert_eq!(q.try_push(critical.clone()), Err(critical));

        // Drain: all HIGH_CAPACITY events still present, FIFO intact.
        for i in 0..HIGH_CAPACITY - 1 {
            assert_eq!(q.pop(), Some(critical_at(i)));
        }
        assert_eq!(q.pop(), Some(completion()));
        assert!(q.is_empty());
    }

    #[test]
    fn tick_flood_coalesces_to_latest_under_capacity() {
        // A flood of 1000 ticks still leaves only one merged slot.
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
        // After filling and draining the queue, a key release must still
        // pop correctly — it was never dropped.
        let mut q = EventQueue::new();
        for i in 0..HIGH_CAPACITY {
            q.push(critical_at(i));
        }
        for _ in 0..HIGH_CAPACITY {
            q.pop().unwrap();
        }
        // Now push a release — must go through.
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
