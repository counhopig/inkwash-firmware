use crate::app::{
    Effect, EffectBatch, EffectBatchId, EffectCompletion, EffectError, EffectFailure, EffectId,
    EffectOutput, OperationId, RenderGeneration,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutcome {
    Completed(EffectOutput),

    Async,

    AsyncWithId(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsyncKick {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effect: Effect,

    pub request_id: Option<u64>,
}

impl AsyncKick {
    pub fn is_render(&self) -> bool {
        matches!(self.effect, Effect::Render(_))
    }
}

pub trait EffectExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchNotice {
    Completed(EffectCompletion),
    Failed(EffectFailure),
    Async(AsyncKick),
}

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
