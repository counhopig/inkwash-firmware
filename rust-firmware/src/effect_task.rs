//! Independent effect execution task: moves synchronous `EffectRunner::run`
//! off the main loop so slow NVS writes, RTC I2C, and other blocking
//! operations no longer stall button / RTC / USB / BLE fact collection.
//!
//! ## Architecture
//!
//! The main loop reduces events to `EffectBatch`es via `Runtime::reduce_next`
//! (pure, fast, no I/O) and submits them through `EffectTask::submit_batch`.
//! A dedicated FreeRTOS task executes each batch in array order, honouring
//! `FailurePolicy::AbortBatch` / `Continue`, and returns `BatchNotice`s
//! (completion / failure) through a channel. The main loop reads notices via
//! `EffectTask::drain_notices` and feeds them back via `Runtime::submit_notice`.
//!
//! ## Driver ownership
//!
//! `DeviceContext` is `!Send` (it borrows `&mut Note4Board` with raw
//! `PinDriver` pointers), so it cannot cross the thread boundary. Instead,
//! the task owns a separate set of `Send`-safe driver handles:
//!
//! - **NVS stores**: `AlarmStore` / `TodoStore` / `InboxStore` / `PersistedCounters`
//!   wrap `EspDefaultNvs` handles. ESP-IDF's NVS API serialises access internally
//!   (per-namespace mutex); the stores are `unsafe impl Send` (see below).
//! - **RTC**: `RtcExecutor` is a channel sender to the RTC executor task (I2C
//!   serialised on the RTC task).
//! - **Sync**: `SyncTask` is a channel sender to the Wi-Fi sync task.
//! - **BLE**: `BleControl` uses channel senders internally.
//! - **Audio**: `AudioTask` owns a fixed-capacity `AudioMailbox`.
//! - **Display**: `EpdClient` is `Clone` (`Arc<Mutex<Canvas>>` + channel-based `EpdHandle`).
//!
//! ## RTC I2C safety
//!
//! All RTC operations go through `RtcExecutor`'s channel — the RTC executor task
//! owns the I2C bus. The effect task's RTC calls block on the reply channel but
//! never touch I2C directly, so there is no race with the main loop's clock polls.
//!
//! ## Battery / charging snapshot
//!
//! `Note4Board::battery_percent()` and `charge_snapshot()` use GPIO/ADC which
//! are `!Send`. The main loop snapshots these values each iteration and stores
//! them in `SharedTaskState`, which the task reads when drawing the Home
//! surface.
//!
//! ## Render coordination
//!
//! `RenderRegistry` is `Arc<std::sync::Mutex<RenderRegistry>>` (shared between
//! the main loop and the task). The task plans renders and kicks the EPD; the
//! main loop's EPD-completion handler updates the cache via the same registry.
//!
//! ## Constraints
//!
//! - Each batch executes serially within the task (no intra-batch concurrency).
//! - Async effects (Render, StartSync, StartBlePairing, EnterDeepSleep,
//!   PrepareSleep, CommitSleep) are only the last effect in their batch — the
//!   state machine enforces this.

use anyhow::Result;
use parking_lot::Mutex;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};

use inkwash_logic::app::{Effect, EffectBatch};
use inkwash_logic::runner::{
    execute_batch, BatchNotice, EffectCategory, EffectExecutor, EffectOutcome,
};

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::rtc_executor::RtcExecutor;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;

// ---- channel capacities ----

/// Bounded channel main loop → task. Capacity 1: the task drains each
/// batch before blocking; the main loop never queues more than one ahead.
const BATCH_CHANNEL_CAP: usize = 1;

/// Capacity of the notice channel (task → main). Must hold enough for a
/// single batch's worth of completions plus margin for the main loop's
/// fact-collection poll cycle.
const NOTICE_CHANNEL_CAP: usize = 8;

/// Stack size for the effect task. Covers NVS serde + I2C blocking waits
/// without overrunning the ESP32-S3's limited internal-RAM stack.
const EFFECT_TASK_STACK: usize = 8 * 1024;

/// The completion envelope for exactly one worker batch.
///
/// Keeping the batch boundary explicit is important for `AbortBatch`: a
/// failed batch may legitimately produce fewer notices than it has effects,
/// but it must still release the one worker slot once its complete result has
/// been delivered.
#[derive(Debug)]
pub struct BatchResult {
    pub notices: Vec<BatchNotice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectTaskError {
    QueueFull,
    Disconnected,
}

/// A self-contained, `Send` set of driver handles that the effect task
/// owns. This duplicates the relevant fields of `DeviceContext` as owned /
/// clonable handles so the task never borrows `&mut DeviceContext`.
pub struct EffectDrivers {
    pub alarm_store: AlarmStore,
    pub todo_store: TodoStore,
    pub inbox_store: InboxStore,
    pub counters: PersistedCounters,
    pub rtc: RtcExecutor,
}

/// The effect task's executor: wraps `EffectDrivers` and implements
/// `EffectExecutor` by dispatching each effect to the appropriate driver.
/// This mirrors `EffectRunner` but operates on owned `Send` handles.
pub struct TaskExecutor {
    pub drivers: EffectDrivers,
}

impl EffectExecutor for TaskExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
        let d = &mut self.drivers;
        match effect {
            // ---- synchronous persistence effects ----
            Effect::PersistAlarms(list) => match AlarmStore::save(&d.alarm_store, list) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Alarms,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistTodos(list) => match TodoStore::save(&d.todo_store, list) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Todos,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistInbox(list) => match InboxStore::save(&d.inbox_store, list) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Inbox,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistConfig(cfg) => match d.counters.save_device_config(cfg) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Config,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistWifiCredentials(creds) => match d.counters.save_wifi_creds(creds) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::WifiCredentials,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistTimezone(offset) => {
                match d.counters.save_timezone_offset_minutes(*offset) {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::Timezone,
                        ),
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::ApplySyncedData(data) => {
                let result = (|| -> Result<()> {
                    AlarmStore::save(&d.alarm_store, &data.alarms)?;
                    TodoStore::save(&d.todo_store, &data.todos)?;
                    InboxStore::save(&d.inbox_store, &data.inbox)?;
                    d.inbox_store.ack_read(&data.inbox_read_acked)?;
                    d.alarm_store.clear_dirty_ids(&data.uploaded_alarm_ids)?;
                    d.todo_store.clear_dirty_ids(&data.uploaded_todo_ids)?;
                    if let Some(etag) = data.etag.as_deref() {
                        d.counters.save_sync_etag(etag)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::SyncApply,
                        ),
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::PersistSyncMetadata(meta) => {
                let result = (|| -> Result<()> {
                    if let Some(etag) = meta.etag.as_deref() {
                        d.counters.save_sync_etag(etag)?;
                    }
                    if let Some(epoch) = meta.last_sync_epoch {
                        d.counters.set_last_sync_epoch(epoch)?;
                    }
                    if let Some(epoch) = meta.rtc_align_epoch {
                        d.counters.set_rtc_align_epoch(epoch)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::SyncMetadata,
                        ),
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::ClearSyncEtag => match d.counters.clear_sync_etag() {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::SyncMetadata,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::ClearRtcAlignEpoch => match d.counters.clear_rtc_align_epoch() {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::SyncMetadata,
                    ),
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::MarkInboxRead { seq } => match d.inbox_store.mark_read(*seq) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::RenderDone,
                )),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistReminder(persistence) => {
                let result = (|| -> Result<()> {
                    for seq in &persistence.urgent_read_ids {
                        d.inbox_store.mark_read(*seq)?;
                    }
                    if let Some(date) = persistence.todo_date.as_deref() {
                        d.counters.set_todo_reminded_date(date)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::ReminderPersisted,
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::CollectReminderFacts(now) => {
                let payload =
                    crate::reminders::collect(&d.inbox_store, &d.todo_store, &d.counters, now)
                        .map_err(|err| (EffectCategory::Persist, format!("{err:#}")))?;
                Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::ReminderFacts(payload),
                ))
            }
            Effect::SetSyncInterval { minutes } => {
                match d.counters.set_sync_interval_minutes(*minutes) {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::RenderDone,
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::PersistAlarmToggle { alarms, toggled_id } => {
                let save_result = AlarmStore::save(&d.alarm_store, alarms);
                let dirty_result = d.alarm_store.mark_dirty(*toggled_id);
                match (save_result, dirty_result) {
                    (Ok(()), Ok(())) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::Alarms,
                        ),
                    )),
                    (Err(err), _) => Err((EffectCategory::Persist, format!("{err:#}"))),
                    (Ok(()), Err(err)) => Err((
                        EffectCategory::Persist,
                        format!("dirty mark failed: {err:#}"),
                    )),
                }
            }
            Effect::PersistTodoEdit { todos, edited_id } => {
                let save_result = TodoStore::save(&d.todo_store, todos);
                let dirty_result = d.todo_store.mark_dirty(*edited_id);
                match (save_result, dirty_result) {
                    (Ok(()), Ok(())) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::Todos,
                        ),
                    )),
                    (Err(err), _) => Err((EffectCategory::Persist, format!("{err:#}"))),
                    (Ok(()), Err(err)) => Err((
                        EffectCategory::Persist,
                        format!("dirty mark failed: {err:#}"),
                    )),
                }
            }
            // ---- RTC effects (channel-based, serialized on RTC task) ----
            Effect::ProgramRtcAlarm(regs) => match d.rtc.program(regs) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::RtcProgrammed,
                )),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::WriteRtcTime(dt) => match d.rtc.write_time(dt) {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::RtcTimeWritten(*dt),
                )),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::DisableRtcAlarm => match d.rtc.disable() {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::RtcProgrammed,
                )),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::AcknowledgeRtcAlarm => match d.rtc.acknowledge() {
                Ok(()) => Ok(EffectOutcome::Completed(
                    inkwash_logic::app::EffectOutput::AckDone,
                )),
                Err(err) => Err((EffectCategory::Ack, format!("{err:#}"))),
            },
            Effect::StartTone
            | Effect::StartReminderTone(_)
            | Effect::StopTone
            | Effect::Reply { .. }
            | Effect::Render(_)
            | Effect::StartSync(_)
            | Effect::PollUrgent
            | Effect::StartSetWifi(_)
            | Effect::StartBlePairing(_)
            | Effect::StopBlePairing => Err((
                EffectCategory::Render,
                "effect requires main-thread-owned resources".to_string(),
            )),
            Effect::EnterLightSleep(_)
            | Effect::EnterDeepSleep(_)
            | Effect::DisableLightSleep
            | Effect::PrepareSleep { .. }
            | Effect::CommitSleep(_) => Err((
                EffectCategory::Sleep,
                "sleep effect requires main-thread platform ownership".to_string(),
            )),
        }
    }
}

/// The task's main loop: receive batches, execute them, send notices back.
fn run(
    mut executor: TaskExecutor,
    batch_rx: Receiver<EffectBatch>,
    notice_tx: SyncSender<BatchResult>,
) {
    // `recv()` returning `Err` means every sender was dropped: task shutdown.
    while let Ok(batch) = batch_rx.recv() {
        let result = BatchResult {
            notices: execute_batch(batch, &mut executor),
        };
        if notice_tx.send(result).is_err() {
            break;
        }
    }
}

/// Handle to the effect task, used by the main loop.
pub struct EffectTask {
    batch_tx: SyncSender<EffectBatch>,
    notice_rx: Receiver<BatchResult>,
    pending_notice: Mutex<Option<BatchResult>>,
}

impl EffectTask {
    /// Spawns the effect task with the given driver handles. The drivers
    /// (and their contained handles) are moved into the task thread.
    pub fn spawn(drivers: EffectDrivers) -> Result<Self> {
        let (batch_tx, batch_rx) = sync_channel::<EffectBatch>(BATCH_CHANNEL_CAP);
        let (notice_tx, notice_rx) = sync_channel::<BatchResult>(NOTICE_CHANNEL_CAP);

        std::thread::Builder::new()
            .name("effect-task".to_string())
            .stack_size(EFFECT_TASK_STACK)
            .spawn(move || run(TaskExecutor { drivers }, batch_rx, notice_tx))?;

        Ok(Self {
            batch_tx,
            notice_rx,
            pending_notice: Mutex::new(None),
        })
    }

    /// Submit one effect batch to the task. Returns `Err` if the task has
    /// exited (channel closed). The batch executes asynchronously; completion
    /// notices arrive via [`drain_notices`].
    pub fn try_submit_batch(
        &self,
        batch: EffectBatch,
    ) -> std::result::Result<(), (EffectBatch, EffectTaskError)> {
        match self.batch_tx.try_send(batch) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(batch)) => Err((batch, EffectTaskError::QueueFull)),
            Err(TrySendError::Disconnected(batch)) => Err((batch, EffectTaskError::Disconnected)),
        }
    }

    /// Retain the one notice that the Runtime queue could not admit. The main
    /// loop stops draining after this point, so the bounded worker channel
    /// provides backpressure without dropping notices.
    pub fn retain_notice(&self, notice: BatchResult) {
        let mut pending = self.pending_notice.lock();
        debug_assert!(pending.is_none());
        *pending = Some(notice);
    }

    pub fn try_next_notice(&self) -> Result<Option<BatchResult>, EffectTaskError> {
        if let Some(notice) = self.pending_notice.lock().take() {
            return Ok(Some(notice));
        }
        match self.notice_rx.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(EffectTaskError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkwash_logic::app::{Effect, EffectBatch, FailurePolicy};
    use inkwash_logic::runner::{BatchNotice, EffectExecutor};

    /// Minimal executor that records effects instead of executing them.
    struct RecordingExecutor {
        effects: Vec<Effect>,
    }

    impl EffectExecutor for RecordingExecutor {
        fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
            self.effects.push(effect.clone());
            Ok(EffectOutcome::Completed(
                inkwash_logic::app::EffectOutput::RenderDone,
            ))
        }
    }

    #[test]
    fn execute_batch_processes_effects_in_order() {
        let effects = vec![
            Effect::PersistTimezone(0),
            Effect::SetSyncInterval { minutes: 30 },
        ];
        let batch = EffectBatch {
            id: 1,
            operation_id: None,
            render_generation: 0,
            effects,
            failure_policy: FailurePolicy::Continue,
        };

        let mut exec = RecordingExecutor {
            effects: Vec::new(),
        };
        let notices = execute_batch(batch, &mut exec);

        assert_eq!(exec.effects.len(), 2);
        assert!(notices
            .iter()
            .all(|n| matches!(n, BatchNotice::Completed(_))));
    }

    #[test]
    fn execute_batch_continues_past_failure_on_continue() {
        struct FailingExecutor;
        impl EffectExecutor for FailingExecutor {
            fn run(&mut self, _effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                Err((EffectCategory::Persist, "disk full".into()))
            }
        }

        let batch = EffectBatch {
            id: 1,
            operation_id: None,
            render_generation: 0,
            effects: vec![
                Effect::PersistTimezone(0),
                Effect::SetSyncInterval { minutes: 30 },
            ],
            failure_policy: FailurePolicy::Continue,
        };

        let mut exec = FailingExecutor;
        let notices = execute_batch(batch, &mut exec);
        assert_eq!(
            notices
                .iter()
                .filter(|n| matches!(n, BatchNotice::Failed(_)))
                .count(),
            2
        );
    }

    #[test]
    fn execute_batch_aborts_on_failure_with_abortbatch() {
        struct FailingExecutor;
        impl EffectExecutor for FailingExecutor {
            fn run(&mut self, _effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                Err((EffectCategory::Persist, "disk full".into()))
            }
        }

        let batch = EffectBatch {
            id: 1,
            operation_id: None,
            render_generation: 0,
            effects: vec![
                Effect::PersistTimezone(0),
                Effect::SetSyncInterval { minutes: 30 },
            ],
            failure_policy: FailurePolicy::AbortBatch,
        };

        let mut exec = FailingExecutor;
        let notices = execute_batch(batch, &mut exec);
        assert_eq!(
            notices
                .iter()
                .filter(|n| matches!(n, BatchNotice::Failed(_)))
                .count(),
            1
        );
    }

    #[test]
    fn abortbatch_failure_still_produces_one_owned_batch_result() {
        struct FailingExecutor;
        impl EffectExecutor for FailingExecutor {
            fn run(&mut self, _effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
                Err((EffectCategory::Persist, "disk full".into()))
            }
        }

        let batch = EffectBatch {
            id: 7,
            operation_id: None,
            render_generation: 0,
            effects: vec![
                Effect::PersistTimezone(0),
                Effect::SetSyncInterval { minutes: 30 },
            ],
            failure_policy: FailurePolicy::AbortBatch,
        };
        let mut exec = FailingExecutor;
        let result = BatchResult {
            notices: execute_batch(batch, &mut exec),
        };

        // AbortBatch intentionally returns one failure for the two-effect
        // batch. The envelope is the completion signal, so the caller can
        // release the worker slot and continue with the next batch.
        assert_eq!(result.notices.len(), 1);
        assert!(matches!(result.notices[0], BatchNotice::Failed(_)));
    }
}
