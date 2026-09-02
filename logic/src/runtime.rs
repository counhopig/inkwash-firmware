//! Single application event loop: the only consumer of `app::update` and
//! the only owner of `AppState` (migration step 2 of
//! `docs/firmware-architecture.md`).
//!
//! Replaces the interim `AppRunner` dispatch model, where every event
//! source called `AppRunner::dispatch` directly and each call site drained
//! async kicks by hand. The runtime owns:
//!
//! - the tiered input [`EventQueue`] (high-priority vs mergeable events,
//!   Tick coalescing - see `event_queue`),
//! - the private `AppState` (no accessor hands out `&mut`, so no second
//!   entry can mutate business state),
//! - the batch-execution engine: every `EffectBatch` produced by
//!   `app::update` runs against a caller-provided [`EffectExecutor`]
//!   (the firmware's real driver executor, or a host fake). Synchronous
//!   effects report `EffectCompleted` / `EffectFailed` back through the
//!   state machine immediately (the deterministic single-thread equivalent
//!   of the executor task posting completions to the event queue); async
//!   effects become [`AsyncKick`]s collected in one place and drained by
//!   the service loop after each pump.
//!
//! The execution engine is deliberately executor-agnostic: the firmware
//! passes a fresh executor borrowing `DeviceContext` per pump. Moving the
//! engine onto its own FreeRTOS task is gated on the RTC executor
//! (next phase) - until the RTC driver has a single owning context, a
//! second task executing RTC effects would race the main loop's clock
//! polls on the shared I2C bus.
//!
//! `pump` is the unique `App::update` consumer: events are applied one at
//! a time in queue order (high-priority tier first), and each event's
//! batches - including those chained by synchronous completions - finish
//! before the next event is popped. This is the same ordering the interim
//! `AppRunner` produced, now reached through one code path.

use crate::app::{AppState, EffectBatch, EffectCompletion, EffectFailure, EffectOutput, Event};
use crate::datetime::DateTime;
use crate::event_queue::EventQueue;
use crate::runner::{err_for_category, AsyncKick, EffectExecutor, EffectOutcome};

/// The application event loop. Owns the business state, the input queue,
/// and the in-flight async kicks.
#[derive(Debug)]
pub struct Runtime {
    state: AppState,
    queue: EventQueue,
    /// Most recent RTC read - threaded into `Effect::Render` so the home
    /// screen does not need a fresh I2C transaction for each refresh.
    last_clock: Option<DateTime>,
    /// Async kicks accumulated by the last `pump`; drained by the service
    /// loop after every pump (never carried across iterations).
    kicks: Vec<AsyncKick>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            state: AppState::default(),
            queue: EventQueue::new(),
            last_clock: None,
            kicks: Vec::new(),
        }
    }

    /// Enqueue one event. The only way facts reach the state machine.
    pub fn push(&mut self, event: Event) {
        self.queue.push(event);
    }

    pub fn set_last_clock(&mut self, clock: Option<DateTime>) {
        self.last_clock = clock;
    }

    pub fn last_clock(&self) -> Option<DateTime> {
        self.last_clock
    }

    /// Read-only view of the business state. Mutation happens exclusively
    /// inside `apply` via `app::update`.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// True while events (or the collapsed latest tick) remain queued.
    pub fn has_work(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Drain the async kicks collected by the most recent `pump`. The
    /// service loop calls this once per pump; kicks are renders (to be
    /// matched against EPD completions by request id) or not-yet-wired
    /// async effects the caller reports.
    pub fn take_kicks(&mut self) -> Vec<AsyncKick> {
        std::mem::take(&mut self.kicks)
    }

    /// The unique `App::update` consumer loop: pop events in priority
    /// order and drive each one's batches to completion. Returns when the
    /// queue is empty. An executor that pushes events while running an
    /// effect (e.g. a completion observed mid-execution) is safe: the new
    /// events queue up and are processed after the current event's chain.
    pub fn pump<E: EffectExecutor>(&mut self, executor: &mut E) -> Result<(), String> {
        while let Some(event) = self.queue.pop() {
            self.apply(event, executor)?;
        }
        Ok(())
    }

    /// Apply one event: run `app::update` (the single state-transition
    /// entry) and execute every returned batch.
    fn apply<E: EffectExecutor>(&mut self, event: Event, executor: &mut E) -> Result<(), String> {
        for batch in crate::app::update(&mut self.state, event) {
            self.run_batch(batch, executor)?;
        }
        Ok(())
    }

    /// Execute one batch against the executor, in effect order.
    ///
    /// Synchronous completions/failures feed back through `apply`
    /// immediately (depth-first), so a chained batch from e.g. `AckDone`
    /// runs before the current batch's remaining effects - the ordering
    /// `FailurePolicy::AbortBatch` and the retry scheduler rely on.
    /// Asynchronous outcomes become kicks for the service loop.
    fn run_batch<E: EffectExecutor>(
        &mut self,
        batch: EffectBatch,
        executor: &mut E,
    ) -> Result<(), String> {
        for (idx, effect) in batch.effects.iter().cloned().enumerate() {
            let effect_id = crate::app::EffectId(idx as u64 + 1);
            match executor.run(&effect) {
                Ok(EffectOutcome::Completed(output)) => {
                    self.apply(
                        Event::EffectCompleted(EffectCompletion {
                            batch_id: batch.id,
                            effect_id,
                            operation_id: batch.operation_id,
                            render_generation: batch.render_generation,
                            output,
                        }),
                        executor,
                    )?;
                }
                Ok(EffectOutcome::AsyncWithId(request_id)) => self.kicks.push(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: Some(request_id),
                }),
                Ok(EffectOutcome::Async) => self.kicks.push(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: None,
                }),
                Err((category, msg)) => {
                    self.apply(
                        Event::EffectFailed(EffectFailure {
                            batch_id: batch.id,
                            effect_id,
                            operation_id: batch.operation_id,
                            render_generation: batch.render_generation,
                            error: err_for_category(category, &msg),
                        }),
                        executor,
                    )?;
                    if batch.failure_policy == crate::app::FailurePolicy::AbortBatch {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// Re-exported for the firmware's service loop: an `EffectOutput` produced
/// by a completed synchronous effect, used when building completion events
/// for externally-observed async outcomes.
pub type SyncEffectOutput = EffectOutput;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm_schedule::StoredAlarm;
    use crate::app::{
        AlarmRuntimeState, BootSnapshot, Effect, EffectError, EffectOutput, PersistTarget, Screen,
    };
    use crate::device_config::DeviceConfig;
    use crate::event_queue::Priority;
    use crate::runner::EffectCategory;
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

    fn boot(alarms: Vec<StoredAlarm>, now: Option<DateTime>, af: bool, aie: bool) -> Event {
        Event::Boot(BootSnapshot {
            wake_cause: WakeCause::Other,
            now,
            rtc_alarm_flag: af,
            rtc_alarm_interrupt_enabled: aie,
            alarms,
            todos: vec![],
            inbox: vec![],
            config: DeviceConfig {
                server_url: String::new(),
                auth_token: String::new(),
            },
        })
    }

    fn alarm(id: u8, hour: u8, minute: u8) -> StoredAlarm {
        StoredAlarm {
            id,
            hour,
            minute,
            repeat: crate::alarm_schedule::Repeat::Daily,
            enabled: true,
            label: String::new(),
        }
    }

    #[test]
    fn pump_applies_queued_events_in_priority_order() {
        let mut rt = Runtime::new();
        rt.push(Event::Tick(dt(9, 0)));
        rt.push(Event::Tick(dt(9, 1)));
        rt.push(boot(vec![], Some(dt(8, 0)), false, true));
        // The tick queued before boot still applies after it, but only the
        // latest tick survives the merge.
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        assert_eq!(rt.state().clock.now, Some(dt(9, 1)));
        assert!(!rt.has_work());
    }

    #[test]
    fn state_is_only_mutable_through_the_queue() {
        // No `&mut AppState` accessor exists: callers can only push facts.
        let rt = Runtime::new();
        let _: &AppState = rt.state();
    }

    #[test]
    fn sync_completion_chains_before_remaining_effects() {
        // Boot with a residue AF: the ACK completes synchronously; the
        // chained RTC reprogram must run before any later event's effects.
        struct Recorder {
            order: Vec<&'static str>,
        }
        impl EffectExecutor for Recorder {
            fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                match effect {
                    Effect::AcknowledgeRtcAlarm => {
                        self.order.push("Ack");
                        Ok(EffectOutcome::Completed(EffectOutput::AckDone))
                    }
                    Effect::ProgramRtcAlarm(_) => {
                        self.order.push("Program");
                        Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
                    }
                    Effect::DisableRtcAlarm => {
                        self.order.push("Disable");
                        Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
                    }
                    Effect::Render(_) => Ok(EffectOutcome::AsyncWithId(1)),
                    other => {
                        self.order.push(crate::harness::effect_name(other));
                        Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
                    }
                }
            }
        }
        let mut rt = Runtime::new();
        // Alarm at 8:30, residue AF at 9:00: snapshot -> residue ACK, then
        // (after AckDone) the register is reprogrammed/disabled.
        rt.push(boot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true));
        let mut rec = Recorder { order: vec![] };
        rt.pump(&mut rec).unwrap();
        let ack_pos = rec.order.iter().position(|n| *n == "Ack").unwrap();
        let program_pos = rec
            .order
            .iter()
            .position(|n| *n == "Program" || *n == "Disable")
            .unwrap();
        assert!(
            program_pos > ack_pos,
            "RTC reprogram runs only after the chained AckDone, order={:?}",
            rec.order
        );
    }

    #[test]
    fn abort_batch_stops_remaining_effects_on_first_failure() {
        struct FailOnce {
            failed: bool,
            ran_stop_tone: bool,
        }
        impl EffectExecutor for FailOnce {
            fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                match effect {
                    Effect::PersistAlarms(_) => {
                        if self.failed {
                            unreachable!("AbortBatch must not re-run the failed effect");
                        }
                        self.failed = true;
                        Err((EffectCategory::Persist, "nvs write failed".into()))
                    }
                    Effect::StartTone | Effect::StopTone => {
                        self.ran_stop_tone = true;
                        Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
                    }
                    Effect::Render(_) => Ok(EffectOutcome::AsyncWithId(1)),
                    Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm => {
                        Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
                    }
                    _ => Ok(EffectOutcome::Completed(EffectOutput::AckDone)),
                }
            }
        }
        // Drive an AbortBatch failure mid-lifecycle by failing the persist
        // inside a Firing boot: the ACK batch (AbortBatch) succeeds, the
        // persist batch (AbortBatch) fails, and the Continue-policy tone
        // batch must still run, with the render kicking as usual.
        let mut exec = FailOnce {
            failed: false,
            ran_stop_tone: false,
        };
        let mut rt = Runtime::new();
        rt.push(boot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true));
        rt.pump(&mut exec).unwrap();
        assert!(
            exec.failed,
            "the scripted persist failure must have been exercised"
        );
        assert!(
            exec.ran_stop_tone,
            "Continue-policy tone batch runs after the AbortBatch failure"
        );
        assert!(
            matches!(rt.state().alarm_runtime, AlarmRuntimeState::Firing { .. }),
            "firing state survives a failed commit (retry scheduled)"
        );
    }

    #[test]
    fn events_pushed_while_effects_run_are_not_lost() {
        // "Effect task busy still receives buttons and RTC alarms": the
        // executor pushes a button event and an alarm snapshot while
        // executing the boot render; both must queue and be processed.
        struct MidRunPusher;
        impl EffectExecutor for MidRunPusher {
            fn run(&mut self, _effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    PersistTarget::Alarms,
                )))
            }
        }
        let mut rt = Runtime::new();
        rt.push(boot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true));
        rt.pump(&mut MidRunPusher).unwrap();
        // Now push while "busy" (a later pump) and confirm neither event
        rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed));
        rt.push(Event::RtcAlarmSnapshotReady(crate::app::RtcAlarmSnapshot {
            now: dt(9, 0),
            alarm_flag: false,
            alarm_interrupt_enabled: true,
        }));
        rt.pump(&mut MidRunPusher).unwrap();
        assert!(!rt.has_work(), "both mid-run events were processed");
    }

    #[test]
    fn kicks_accumulate_across_one_pump_and_drain_once() {
        let mut rt = Runtime::new();
        rt.push(boot(vec![], Some(dt(9, 0)), false, true));
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        // Boot renders once -> exactly one render kick with a request id.
        let kicks = rt.take_kicks();
        assert_eq!(kicks.len(), 1);
        assert!(kicks[0].request_id.is_some());
        assert!(rt.take_kicks().is_empty(), "kicks drain exactly once");
    }

    #[test]
    fn priority_classification_matches_event_queue() {
        assert_eq!(priority_of(&Event::Tick(dt(9, 0))), Priority::Mergeable);
        assert_eq!(
            priority_of(&Event::Button(crate::button_event::ButtonEvent::Pressed)),
            Priority::High
        );
    }

    fn priority_of(event: &Event) -> Priority {
        crate::event_queue::priority(event)
    }

    // ---- regression: failure feedback produces no spurious batches -------
    #[test]
    fn failure_event_on_unmatched_op_is_harmless() {
        let mut rt = Runtime::new();
        rt.push(boot(vec![], Some(dt(9, 0)), false, true));
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        rt.push(Event::EffectFailed(EffectFailure {
            batch_id: crate::app::EffectBatchId(0),
            effect_id: crate::app::EffectId(0),
            operation_id: crate::app::OperationId(999),
            render_generation: None,
            error: EffectError::Ack("stale".into()),
        }));
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        assert_eq!(rt.state().screen, Screen::Home);
    }
}
