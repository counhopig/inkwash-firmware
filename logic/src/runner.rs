//! Effect-execution vocabulary shared by the application runtime and its
//! executors (host fakes and the firmware's real driver executor).
//!
//! `app::update` is pure; the single-consumer `Runtime` (`runtime` module)
//! owns `AppState`, the event queue, and batch execution. This module
//! keeps only what an executor needs: the per-category failure
//! classification, the outcome of running one effect, the async-kick
//! record, and the category-to-error mapping. The firmware crate provides
//! the real executor (`EffectRunner`) that runs each `Effect` against
//! `DeviceContext`'s drivers; a host harness provides a fake one that
//! records calls and scripts failures.

use crate::app::{
    Effect, EffectBatch, EffectBatchId, EffectCompletion, EffectError, EffectFailure, EffectId,
    EffectOutput, OperationId, RenderGeneration,
};

/// Whether a whole batch can run in the independent effect worker.
/// Main-thread resources are batch barriers: the caller keeps the entire
/// batch together so effect order and `AbortBatch` remain unchanged.
pub fn batch_is_worker_safe(batch: &EffectBatch) -> bool {
    batch.effects.iter().all(|effect| {
        matches!(
            effect,
            Effect::PersistAlarms(_)
                | Effect::PersistTodos(_)
                | Effect::PersistInbox(_)
                | Effect::PersistConfig(_)
                | Effect::PersistWifiCredentials(_)
                | Effect::PersistTimezone(_)
                | Effect::ApplySyncedData(_)
                | Effect::PersistSyncMetadata(_)
                | Effect::ClearSyncEtag
                | Effect::ClearRtcAlignEpoch
                | Effect::MarkInboxRead { .. }
                | Effect::PersistReminder(_)
                | Effect::CollectReminderFacts(_)
                | Effect::SetSyncInterval { .. }
                | Effect::PersistAlarmToggle { .. }
                | Effect::PersistTodoEdit { .. }
                | Effect::ProgramRtcAlarm(_)
                | Effect::WriteRtcTime(_)
                | Effect::DisableRtcAlarm
                | Effect::AcknowledgeRtcAlarm
        )
    })
}

/// Per-category failure classification, mirroring the state machine's
/// `EffectError` variants. The executor reports which category an
/// `anyhow`-style failure belongs to so the runner can map it to the
/// exact `EffectError` (ACK vs Persist vs Rtc vs Render vs Sync vs Tone
/// vs Sleep) and the state machine schedules the right retry path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectCategory {
    Ack,
    Persist,
    Rtc,
    Render,
    Sync,
    Tone,
    Sleep,
    Ble,
}

/// Outcome of running one `Effect` in the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutcome {
    /// The effect completed synchronously; feed `EffectCompleted` back.
    Completed(EffectOutput),
    /// The effect was kicked off asynchronously (no request id yet).
    Async,
    /// The effect started an async operation carrying a request id (e.g.
    /// an EPD refresh) that a later completion will echo back.
    AsyncWithId(u64),
}

/// A side-effect request the dispatch could not complete synchronously.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsyncKick {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effect: Effect,
    /// EPD request id echoed by the matching completion, when the kick is
    /// a Render whose panel refresh already started.
    pub request_id: Option<u64>,
}

impl AsyncKick {
    pub fn is_render(&self) -> bool {
        matches!(self.effect, Effect::Render(_))
    }
}

/// Executes one `Effect` against whatever the caller supplies. The
/// firmware implementation drives `DeviceContext`; a host harness drives
/// a recording fake. The error is returned as a category plus message so
/// the runner can build the exact `EffectError`.
pub trait EffectExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)>;
}

/// Owned result of executing one effect batch. The executor never applies the
/// resulting events itself; the Runtime enqueues them after every effect in
/// the current batch has run, so an A-completion-derived batch cannot interrupt
/// the current batch's remaining effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchNotice {
    Completed(EffectCompletion),
    Failed(EffectFailure),
    Async(AsyncKick),
}

/// Execute one batch in array order without owning or touching Runtime.
/// Synchronous completion/failure notices are returned as owned events for the
/// application loop to enqueue. `AbortBatch` stops only effects after the
/// first failure; it never rolls back effects already executed.
pub fn execute_batch<E: EffectExecutor>(batch: EffectBatch, executor: &mut E) -> Vec<BatchNotice> {
    let mut notices = Vec::new();
    for (idx, effect) in batch.effects.iter().cloned().enumerate() {
        let effect_id = EffectId(idx as u64 + 1);
        match executor.run(&effect) {
            Ok(EffectOutcome::Completed(output)) => {
                notices.push(BatchNotice::Completed(EffectCompletion {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    output,
                }));
            }
            Ok(EffectOutcome::AsyncWithId(request_id)) => {
                notices.push(BatchNotice::Async(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: Some(request_id),
                }));
            }
            Ok(EffectOutcome::Async) => {
                notices.push(BatchNotice::Async(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: None,
                }));
            }
            Err((category, msg)) => {
                notices.push(BatchNotice::Failed(EffectFailure {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    error: err_for_category(category, &msg),
                }));
                if batch.failure_policy == crate::app::FailurePolicy::AbortBatch {
                    break;
                }
            }
        }
    }
    notices
}

/// Maps a per-category failure into the state machine's `EffectError`.
pub fn err_for_category(category: EffectCategory, msg: &str) -> EffectError {
    match category {
        EffectCategory::Ack => EffectError::Ack(msg.to_string()),
        EffectCategory::Persist => EffectError::Persist(msg.to_string()),
        EffectCategory::Rtc => EffectError::Rtc(msg.to_string()),
        EffectCategory::Render => EffectError::Render(msg.to_string()),
        EffectCategory::Sync => EffectError::Sync(msg.to_string()),
        EffectCategory::Tone => EffectError::Tone(msg.to_string()),
        EffectCategory::Sleep => EffectError::Sleep(msg.to_string()),
        EffectCategory::Ble => EffectError::Ble(msg.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{EffectBatchId, FailurePolicy, OperationId};

    struct Recorder {
        order: Vec<&'static str>,
    }

    impl EffectExecutor for Recorder {
        fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
            self.order.push(match effect {
                Effect::StartTone => "A",
                Effect::StopTone => "B",
                _ => "other",
            });
            Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
        }
    }

    #[test]
    fn execute_batch_runs_all_effects_before_returning_completion_notices() {
        let mut recorder = Recorder { order: Vec::new() };
        let notices = execute_batch(
            EffectBatch {
                id: EffectBatchId(1),
                operation_id: OperationId(1),
                render_generation: None,
                effects: vec![Effect::StartTone, Effect::StopTone],
                failure_policy: FailurePolicy::Continue,
            },
            &mut recorder,
        );
        assert_eq!(recorder.order, vec!["A", "B"]);
        assert_eq!(notices.len(), 2);
        assert!(matches!(notices[0], BatchNotice::Completed(_)));
        assert!(matches!(notices[1], BatchNotice::Completed(_)));
    }

    #[test]
    fn worker_route_keeps_main_thread_effects_as_whole_batch() {
        let worker_batch = EffectBatch {
            id: EffectBatchId(2),
            operation_id: OperationId(1),
            render_generation: None,
            effects: vec![
                Effect::PersistTimezone(480),
                Effect::ProgramRtcAlarm(crate::alarm_regs::AlarmRegs {
                    minute: 0,
                    hour: 0,
                    day: None,
                    weekday: None,
                }),
            ],
            failure_policy: FailurePolicy::AbortBatch,
        };
        assert!(batch_is_worker_safe(&worker_batch));

        let main_batch = EffectBatch {
            id: EffectBatchId(3),
            operation_id: OperationId(2),
            render_generation: None,
            effects: vec![Effect::PersistTimezone(480), Effect::StopTone],
            failure_policy: FailurePolicy::AbortBatch,
        };
        assert!(!batch_is_worker_safe(&main_batch));
    }
}
