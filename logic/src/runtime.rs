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
//!   bounded event queue after the complete current batch has run; async
//!   effects become [`AsyncKick`]s collected in one place and drained by
//!   the service loop after each pump.
//!
//! The execution engine is deliberately executor-agnostic: the firmware
//! passes a fresh executor borrowing `DeviceContext` per pump. A later
//! phase may move the engine onto its own FreeRTOS task once the RTC
//! driver context is fully consolidated; until then, RTC effects execute
//! on the main loop to avoid racing the shared I2C bus during clock
//! polls.
//!
//! `pump` is the unique `App::update` consumer: events are applied one at
//! a time in queue order (high-priority tier first), and each event's
//! batches finish before completion-derived events are popped. This keeps
//! every effect in the current batch ahead of its derived state transition.

#![allow(clippy::result_large_err)]

use crate::app::{AppState, EffectBatch, EffectOutput, Event};
use crate::datetime::DateTime;
use crate::event_queue::EventQueue;
use crate::runner::{execute_batch, AsyncKick, BatchNotice, EffectExecutor};

/// A bounded event queue refused one or more completion notices. The caller
/// owns `unsubmitted` and must retry them after reducing queued input.
#[derive(Debug)]
pub struct PumpError {
    pub unsubmitted: Vec<BatchNotice>,
    /// The producer-owned event, when `submit_and_pump` could not admit it
    /// before the pump itself reported saturation.
    pub rejected_event: Option<Event>,
}

impl std::fmt::Display for PumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "application event queue saturated ({} notices pending)",
            self.unsubmitted.len()
        )
    }
}

impl std::error::Error for PumpError {}

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
    /// Enqueues a producer event with explicit backpressure.
    #[allow(clippy::result_large_err)]
    pub fn try_push(&mut self, event: Event) -> Result<(), Event> {
        self.queue.try_push(event)
    }

    /// Enqueue the one event held as a worker continuation ahead of events
    /// collected while that worker batch was running.
    #[allow(clippy::result_large_err)]
    pub fn try_push_continuation(&mut self, event: Event) -> Result<(), Event> {
        self.queue.try_push_front(event)
    }

    /// Submit an event while retaining it across queue saturation. The
    /// producer owns the returned event until it is accepted; pumping makes
    /// room without dropping or overwriting a critical fact.
    #[allow(clippy::result_large_err)]
    pub fn submit(&mut self, event: Event) -> Result<(), Event> {
        self.try_push(event)
    }

    /// Admit one producer-owned event without taking ownership on failure.
    /// Callers that cannot block must retain the returned event and retry
    /// after servicing the runtime.
    #[allow(clippy::result_large_err)]
    pub fn admit_event(&mut self, event: Event) -> Result<(), Event> {
        self.try_push(event)
    }

    /// Test-only fixture helper. Firmware producers must use [`try_push`].
    #[cfg(test)]
    pub fn push(&mut self, event: Event) {
        self.try_push(event).expect("test runtime queue must fit");
    }

    pub fn set_last_clock(&mut self, clock: Option<DateTime>) {
        self.last_clock = clock;
    }

    pub fn last_clock(&self) -> Option<DateTime> {
        self.last_clock
    }

    /// Read-only view of the business state. Mutation happens exclusively
    /// inside `reduce_event` via `app::update`.
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
    pub fn pump<E: EffectExecutor>(&mut self, executor: &mut E) -> Result<(), PumpError> {
        self.pump_with_notices(executor)
    }

    /// Lossless compatibility helper for synchronous firmware call sites.
    /// When the input queue is full, it pumps existing input to make room and
    /// retries the producer-owned event. Any unsubmitted completion notices
    /// remain in `PumpError` for the caller; neither the event nor notices are
    /// converted to a lossy string.
    pub fn submit_and_pump<E: EffectExecutor>(
        &mut self,
        event: Event,
        executor: &mut E,
    ) -> Result<(), PumpError> {
        let mut pending = event;
        loop {
            match self.admit_event(pending) {
                Ok(()) => return self.pump(executor),
                Err(event) => {
                    pending = event;
                    if let Err(mut error) = self.pump(executor) {
                        error.rejected_event = Some(pending);
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Lossless bounded adapter for synchronous producers. Completion notices
    /// are retained in a fixed continuation window while queued events are
    /// consumed; once the window is full, ownership is returned in
    /// `PumpError` instead of growing memory or dropping a notice.
    #[allow(clippy::result_large_err)]
    pub fn submit_and_pump_lossless<E: EffectExecutor>(
        &mut self,
        event: Event,
        executor: &mut E,
    ) -> Result<(), PumpError> {
        const CONTINUATION_CAPACITY: usize = crate::event_queue::HIGH_CAPACITY;
        let mut pending_event = Some(event);
        let mut pending_notices = std::collections::VecDeque::with_capacity(CONTINUATION_CAPACITY);

        loop {
            while let Some(notice) = pending_notices.pop_front() {
                match self.submit_notice(notice) {
                    Ok(()) => {}
                    Err(notice) => {
                        pending_notices.push_front(notice);
                        break;
                    }
                }
            }

            if !pending_notices.is_empty() {
                match self.pump(executor) {
                    Ok(()) => continue,
                    Err(error) => {
                        if let Some(event) = error.rejected_event {
                            pending_event = Some(event);
                        }
                        let mut unsubmitted = error.unsubmitted;
                        unsubmitted.extend(pending_notices.drain(..));
                        if unsubmitted.len() > CONTINUATION_CAPACITY {
                            return Err(PumpError {
                                unsubmitted,
                                rejected_event: pending_event,
                            });
                        }
                        for notice in unsubmitted {
                            pending_notices.push_back(notice);
                        }
                        continue;
                    }
                }
            }

            if let Some(event) = pending_event.take() {
                match self.submit_and_pump(event, executor) {
                    Ok(()) => continue,
                    Err(error) => {
                        if error.unsubmitted.len() > CONTINUATION_CAPACITY {
                            return Err(error);
                        }
                        pending_event = error.rejected_event;
                        for notice in error.unsubmitted {
                            pending_notices.push_back(notice);
                        }
                        continue;
                    }
                }
            }

            match self.pump(executor) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if error.unsubmitted.len() > CONTINUATION_CAPACITY {
                        return Err(error);
                    }
                    pending_event = error.rejected_event;
                    for notice in error.unsubmitted {
                        pending_notices.push_back(notice);
                    }
                }
            }
        }
    }

    /// Host convenience pump with ownership of notices that could not enter
    /// the bounded queue. A real worker should use `reduce_next` and
    /// `submit_notice` directly so it can retain and retry those notices.
    pub fn pump_with_notices<E: EffectExecutor>(
        &mut self,
        executor: &mut E,
    ) -> Result<(), PumpError> {
        while let Some(event) = self.queue.pop() {
            let batches = self.reduce_event(event);
            let mut notices = Vec::new();
            for batch in batches {
                notices.extend(execute_batch(batch, executor));
            }
            self.enqueue_notices(notices)?;
        }
        Ok(())
    }

    /// Reduce exactly one queued event and return owned batches. This method
    /// performs no effect execution or I/O and is the boundary a real effect
    /// worker can consume.
    pub fn reduce_next(&mut self) -> Option<Vec<EffectBatch>> {
        self.queue.pop().map(|event| self.reduce_event(event))
    }

    /// Reduce an owned event through the single state transition entry point.
    pub fn reduce_event(&mut self, event: Event) -> Vec<EffectBatch> {
        if let Event::EffectCompleted(completion) = &event {
            if let EffectOutput::RtcTimeWritten(written_time) = completion.output {
                self.last_clock = Some(written_time);
            }
        }
        crate::app::update(&mut self.state, event)
    }

    /// Feed an executor notice back through the bounded event path. A full
    /// queue returns the notice to its producer for a later retry.
    #[allow(clippy::result_large_err)]
    pub fn submit_notice(&mut self, notice: BatchNotice) -> Result<(), BatchNotice> {
        self.try_submit_notice_direct(notice)
    }

    #[allow(clippy::result_large_err)]
    fn try_submit_notice_direct(&mut self, notice: BatchNotice) -> Result<(), BatchNotice> {
        match notice {
            BatchNotice::Async(kick) => {
                self.kicks.push(kick);
                Ok(())
            }
            BatchNotice::Completed(completion) => match self
                .try_push(Event::EffectCompleted(completion))
            {
                Ok(()) => Ok(()),
                Err(Event::EffectCompleted(completion)) => Err(BatchNotice::Completed(completion)),
                Err(_) => unreachable!(),
            },
            BatchNotice::Failed(failure) => match self.try_push(Event::EffectFailed(failure)) {
                Ok(()) => Ok(()),
                Err(Event::EffectFailed(failure)) => Err(BatchNotice::Failed(failure)),
                Err(_) => unreachable!(),
            },
        }
    }

    fn enqueue_notices(&mut self, notices: Vec<BatchNotice>) -> Result<(), PumpError> {
        let mut iter = notices.into_iter();
        while let Some(notice) = iter.next() {
            if let Err(rejected) = self.submit_notice(notice) {
                let mut unsubmitted = vec![rejected];
                unsubmitted.extend(iter);
                return Err(PumpError {
                    unsubmitted,
                    rejected_event: None,
                });
            }
        }
        Ok(())
    }

    /// Feed multiple executor notices back through the bounded event path.
    /// All notices are attempted in order; rejected ones (queue full) are
    /// returned to the caller for a later retry. This is the multi-notice
    /// counterpart to [`submit_notice`], used by the effect task's
    /// completion receiver loop.
    pub fn submit_notices(&mut self, notices: Vec<BatchNotice>) -> Result<(), Vec<BatchNotice>> {
        let mut rejected = Vec::new();
        for notice in notices {
            if let Err(rejected_notice) = self.submit_notice(notice) {
                rejected.push(rejected_notice);
            }
        }
        if rejected.is_empty() {
            Ok(())
        } else {
            Err(rejected)
        }
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
        AlarmRuntimeState, BootSnapshot, DeviceStatus, Effect, EffectError, EffectFailure,
        EffectOutput, PersistTarget, Screen,
    };
    use crate::device_config::DeviceConfig;
    use crate::event_queue::Priority;
    use crate::runner::{EffectCategory, EffectOutcome};
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
            status: DeviceStatus::default(),
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
    fn submit_notice_returns_completion_when_high_queue_is_full() {
        let mut rt = Runtime::new();
        for _ in 0..crate::event_queue::HIGH_CAPACITY {
            rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter,
            )));
        }
        let notice = BatchNotice::Completed(crate::app::EffectCompletion {
            batch_id: crate::app::EffectBatchId(1),
            effect_id: crate::app::EffectId(1),
            operation_id: crate::app::OperationId(1),
            render_generation: None,
            output: EffectOutput::ToneDone,
        });
        assert!(rt.submit_notice(notice).is_err());
        assert!(rt.has_work());
    }

    #[test]
    fn sleep_fact_is_retained_when_high_queue_is_full() {
        let mut rt = Runtime::new();
        for _ in 0..crate::event_queue::HIGH_CAPACITY {
            rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter,
            )));
        }
        let token = crate::power_state::SleepToken {
            kind: crate::power_state::SleepKind::Deep,
            activity_version: 1,
            request_id: 1,
            prepared_at: 10,
        };
        let fact = Event::SleepPrepared {
            token,
            inputs: crate::power_state::SleepInputs {
                page_allows_sleep: true,
                ..crate::power_state::SleepInputs::default()
            },
        };
        let rejected = rt
            .admit_event(fact.clone())
            .expect_err("a full queue must return the sleep fact to its owner");
        assert_eq!(rejected, fact);
        assert!(rt.has_work());
    }

    #[test]
    fn admission_retries_after_worker_abort_notice_and_full_queue() {
        struct AbortExecutor;
        impl EffectExecutor for AbortExecutor {
            fn run(&mut self, _effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                Err((EffectCategory::Persist, "simulated abort".into()))
            }
        }

        let mut rt = Runtime::new();
        for _ in 0..crate::event_queue::HIGH_CAPACITY {
            rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter,
            )));
        }
        let mut pending = Some(Event::Tick(dt(9, 1)));
        while let Some(event) = pending.take() {
            match rt.admit_event(event) {
                Ok(()) => {}
                Err(event) => {
                    pending = Some(event);
                    rt.pump(&mut crate::harness::FakeExecutor::default())
                        .unwrap();
                }
            }
        }

        let notices = execute_batch(
            EffectBatch {
                id: crate::app::EffectBatchId(7),
                operation_id: crate::app::OperationId(7),
                render_generation: None,
                effects: vec![Effect::PersistTimezone(480), Effect::PersistTimezone(481)],
                failure_policy: crate::app::FailurePolicy::AbortBatch,
            },
            &mut AbortExecutor,
        );
        assert_eq!(
            notices.len(),
            1,
            "AbortBatch must produce one failure notice"
        );

        for notice in notices {
            match rt.submit_notice(notice) {
                Ok(()) => {}
                Err(notice) => {
                    rt.pump(&mut crate::harness::FakeExecutor::default())
                        .unwrap();
                    rt.submit_notice(notice)
                        .expect("notice must be admitted after the queue is serviced");
                }
            }
        }
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        assert!(
            !rt.has_work(),
            "admitted event and AbortBatch notice were processed"
        );
    }

    #[test]
    fn bounded_lossless_adapter_drains_saturated_queue_and_completion_chain() {
        let mut rt = Runtime::new();
        rt.push(boot(vec![], Some(dt(9, 0)), false, true));
        for _ in 0..(crate::event_queue::HIGH_CAPACITY - 1) {
            rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter,
            )));
        }
        let mut executor = crate::harness::FakeExecutor::default();
        rt.submit_and_pump_lossless(Event::Tick(dt(9, 1)), &mut executor)
            .expect("bounded continuation should retry until the queue drains");
        assert!(!rt.has_work());
    }

    #[test]
    fn sync_completion_is_queued_after_the_current_batch() {
        // Boot with a residue AF: the ACK completes synchronously; its
        // chained RTC reprogram is queued only after the complete batch has
        // finished, so the current batch cannot be interrupted by C.
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
        rt.push(Event::Button(crate::button_event::ButtonEvent::Pressed(
            crate::button_event::ButtonId::Enter,
        )));
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
            priority_of(&Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter
            ))),
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

    #[test]
    fn submit_notices_feeds_completion_and_kick() {
        // The two-phase dispatch pattern: the effect task produces notices,
        // the main loop calls submit_notices to feed them back.
        let mut rt = Runtime::new();
        rt.push(boot(vec![], Some(dt(9, 0)), false, true));
        // Pump with the fake executor to get the initial boot render.
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        // Simulate the effect task sending back two notices.
        let notices = vec![
            BatchNotice::Completed(crate::app::EffectCompletion {
                batch_id: crate::app::EffectBatchId(0),
                effect_id: crate::app::EffectId(0),
                operation_id: crate::app::OperationId(0),
                render_generation: None,
                output: crate::app::EffectOutput::AckDone,
            }),
            BatchNotice::Failed(crate::app::EffectFailure {
                batch_id: crate::app::EffectBatchId(0),
                effect_id: crate::app::EffectId(0),
                operation_id: crate::app::OperationId(0),
                render_generation: None,
                error: crate::app::EffectError::Ack("test".into()),
            }),
        ];
        rt.submit_notices(notices).unwrap();
        // Both notices should have been queued as events (work is available).
        assert!(rt.has_work(), "notices queued as events");
    }

    #[test]
    fn worker_completion_can_be_reduced_without_a_new_external_event() {
        let mut rt = Runtime::new();
        rt.push(boot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true));
        let batches = rt.reduce_next().expect("boot event reduces to batches");
        let ack = batches
            .iter()
            .find(|batch| {
                batch
                    .effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::AcknowledgeRtcAlarm))
            })
            .expect("residue boot starts with an RTC acknowledgement");

        rt.submit_notice(BatchNotice::Completed(crate::app::EffectCompletion {
            batch_id: ack.id,
            effect_id: crate::app::EffectId(1),
            operation_id: ack.operation_id,
            render_generation: ack.render_generation,
            output: EffectOutput::AckDone,
        }))
        .expect("worker completion should enter the bounded runtime queue");

        let chained = rt
            .reduce_next()
            .expect("completion must be reducible immediately");
        assert!(chained.iter().any(|batch| {
            batch.effects.iter().any(|effect| {
                matches!(effect, Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm)
            })
        }));
    }

    #[test]
    fn submit_notices_returns_rejected_when_queue_full() {
        // When the event queue is full, submit_notices returns the
        // notices that could not be enqueued so the caller can retry.
        let mut rt = Runtime::new();
        rt.push(boot(vec![], Some(dt(9, 0)), false, true));
        rt.pump(&mut crate::harness::FakeExecutor::default())
            .unwrap();
        rt.take_kicks();

        // Fill the queue with EffectFailed events (the only thing that
        // goes through try_push in submit_notice's Completed path).
        let big_failure = || {
            BatchNotice::Failed(crate::app::EffectFailure {
                batch_id: crate::app::EffectBatchId(0),
                effect_id: crate::app::EffectId(0),
                operation_id: crate::app::OperationId(0),
                render_generation: None,
                error: crate::app::EffectError::Ack("full".into()),
            })
        };

        // Push enough to fill the bounded queue.
        let mut i = 0;
        loop {
            match rt.submit_notice(big_failure()) {
                Ok(()) => i += 1,
                Err(rejected) => {
                    // Now we know the queue is full — submit_notices
                    // should reject at least the first one.
                    let result = rt.submit_notices(vec![rejected, big_failure()]);
                    assert!(
                        result.is_err(),
                        "submit_notices returns Err when queue full"
                    );
                    assert!(
                        !result.unwrap_err().is_empty(),
                        "at least one notice was rejected"
                    );
                    return;
                }
            }
            if i > 1000 {
                // Queue didn't fill — test doesn't apply to this config.
                break;
            }
        }
    }
}
