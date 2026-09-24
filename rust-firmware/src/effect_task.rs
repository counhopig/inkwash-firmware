use anyhow::Result;
use parking_lot::Mutex;
use std::sync::mpsc::{
    sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
};
use std::time::Duration;

use inkwash_logic::app::{Effect, EffectBatch};
use inkwash_logic::runner::{
    execute_batch, BatchNotice, EffectCategory, EffectExecutor, EffectOutcome,
};

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::rtc_executor::RtcExecutor;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;

const BATCH_CHANNEL_CAP: usize = 1;

const NOTICE_CHANNEL_CAP: usize = 8;

const EFFECT_TASK_STACK: usize = 16 * 1024;

const IDLE_DRAIN: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct BatchResult {
    pub notices: Vec<BatchNotice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectTaskError {
    QueueFull,
    Disconnected,
}

pub struct EffectDrivers {
    pub alarm_store: AlarmStore,
    pub todo_store: TodoStore,
    pub inbox_store: InboxStore,
    pub counters: PersistedCounters,
    pub rtc: RtcExecutor,
}

pub struct TaskExecutor {
    pub drivers: EffectDrivers,
}

impl EffectExecutor for TaskExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
        let d = &mut self.drivers;
        match effect {
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
                let result = crate::sync_apply::apply(
                    data,
                    &d.counters,
                    &d.alarm_store,
                    &d.todo_store,
                    &d.inbox_store,
                );
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
                let result = d
                    .alarm_store
                    .mark_dirty(*toggled_id)
                    .and_then(|()| AlarmStore::save(&d.alarm_store, alarms));
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::Alarms,
                        ),
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::PersistTodoEdit { todos, edited_id } => {
                let result = d
                    .todo_store
                    .mark_dirty(*edited_id)
                    .and_then(|()| TodoStore::save(&d.todo_store, todos));
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(
                        inkwash_logic::app::EffectOutput::Persisted(
                            inkwash_logic::app::PersistTarget::Todos,
                        ),
                    )),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }

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

fn run(
    mut executor: TaskExecutor,
    batch_rx: Receiver<EffectBatch>,
    notice_tx: SyncSender<BatchResult>,
) {
    crate::heap_probe::register_current_task(crate::heap_probe::SLOT_EFFECT);
    let watchdog_subscribed = match crate::watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("effect-task watchdog subscribe failed: {err}");
            false
        }
    };

    loop {
        let batch = match batch_rx.recv_timeout(IDLE_DRAIN) {
            Ok(batch) => batch,
            Err(RecvTimeoutError::Timeout) => {
                if watchdog_subscribed {
                    crate::watchdog::feed();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if watchdog_subscribed {
            crate::watchdog::feed();
        }
        let result = BatchResult {
            notices: execute_batch(batch, &mut executor),
        };
        if notice_tx.send(result).is_err() {
            break;
        }
    }
}

pub struct EffectTask {
    batch_tx: SyncSender<EffectBatch>,
    notice_rx: Receiver<BatchResult>,
    pending_notice: Mutex<Option<BatchResult>>,
}

impl EffectTask {
    pub fn spawn(drivers: EffectDrivers) -> Result<Self> {
        let (batch_tx, batch_rx) = sync_channel::<EffectBatch>(BATCH_CHANNEL_CAP);
        let (notice_tx, notice_rx) = sync_channel::<BatchResult>(NOTICE_CHANNEL_CAP);

        crate::tasks::spawn(
            "effect-task",
            EFFECT_TASK_STACK,
            crate::tasks::PRIORITY_EFFECT,
            move || run(TaskExecutor { drivers }, batch_rx, notice_tx),
        )?;

        Ok(Self {
            batch_tx,
            notice_rx,
            pending_notice: Mutex::new(None),
        })
    }

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

    pub fn retain_notice(&self, notice: BatchResult) {
        let mut pending = self.pending_notice.lock();
        debug_assert!(pending.is_none());
        *pending = Some(notice);
    }

    pub fn has_retained_notice(&self) -> bool {
        self.pending_notice.lock().is_some()
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
