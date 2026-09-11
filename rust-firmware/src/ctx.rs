use anyhow::Result;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use crate::alarms::AlarmStore;
use crate::ble_control::BleControl;
use crate::board::Note4Board;
use crate::control::{Channel, Command, Reply};
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::{PersistedCounters, WifiCreds};
use crate::sync;
use crate::sync_task::{PendingWifiOp, SyncTask};
use crate::todos::TodoStore;
use crate::usb_console::{QueuedReply, ReplyQueueError, UsbConsole, UsbReplyWriter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedWakePlan {
    pub token: inkwash_logic::power_state::SleepToken,
    pub prepare_operation_id: inkwash_logic::app::OperationId,
    pub commit_operation_id: Option<inkwash_logic::app::OperationId>,
}

pub struct PendingRenderCompletion {
    pub kick: crate::app_runner::AsyncKick,
    pub output: inkwash_logic::app::EffectOutput,
    pub failure: Option<inkwash_logic::app::EffectError>,
}

pub struct PendingDispatchSources {
    pub tick: Option<inkwash_logic::app::Event>,
    pub power_poll: Option<inkwash_logic::app::Event>,
    pub rtc_alarm_snapshot: Option<inkwash_logic::app::Event>,
    pub buttons: VecDeque<inkwash_logic::app::Event>,
    pub usb_command: Option<inkwash_logic::app::Event>,
    pub ble_command: Option<inkwash_logic::app::Event>,
    pub lifecycle: VecDeque<inkwash_logic::app::Event>,
    pub worker: VecDeque<inkwash_logic::app::Event>,
    pub sleep: VecDeque<inkwash_logic::app::Event>,
    pub boot: Option<inkwash_logic::app::Event>,
    pub scheduler: Option<inkwash_logic::app::Event>,
}

const BUTTON_PENDING_CAPACITY: usize = 3;
const LIFECYCLE_PENDING_CAPACITY: usize = 4;
const WORKER_PENDING_CAPACITY: usize = 8;
const SLEEP_PENDING_CAPACITY: usize = 2;

impl Default for PendingDispatchSources {
    fn default() -> Self {
        Self {
            tick: None,
            power_poll: None,
            rtc_alarm_snapshot: None,
            buttons: VecDeque::with_capacity(BUTTON_PENDING_CAPACITY),
            usb_command: None,
            ble_command: None,
            lifecycle: VecDeque::with_capacity(LIFECYCLE_PENDING_CAPACITY),
            worker: VecDeque::with_capacity(WORKER_PENDING_CAPACITY),
            sleep: VecDeque::with_capacity(SLEEP_PENDING_CAPACITY),
            boot: None,
            scheduler: None,
        }
    }
}

impl PendingDispatchSources {
    #[allow(clippy::result_large_err)]
    pub fn retain(
        &mut self,
        event: inkwash_logic::app::Event,
    ) -> Result<(), inkwash_logic::app::Event> {
        use inkwash_logic::app::Event;
        match event {
            Event::Tick(_) => {
                self.tick = Some(event);
                Ok(())
            }
            Event::PowerPoll(_) => {
                self.power_poll = Some(event);
                Ok(())
            }
            Event::RtcAlarmSnapshotReady(_) => {
                self.rtc_alarm_snapshot = Some(event);
                Ok(())
            }
            Event::Button(_) => {
                if self.buttons.len() >= BUTTON_PENDING_CAPACITY {
                    return Err(event);
                }
                self.buttons.push_back(event);
                Ok(())
            }
            Event::UsbCommand(_) => {
                if self.usb_command.is_some() {
                    return Err(event);
                }
                self.usb_command = Some(event);
                Ok(())
            }
            Event::BleCommand(_) => {
                if self.ble_command.is_some() {
                    return Err(event);
                }
                self.ble_command = Some(event);
                Ok(())
            }
            Event::BlePairingStarted
            | Event::BlePairingSucceeded(_)
            | Event::BlePairingFailed(_)
            | Event::BleDisconnected => {
                if self.lifecycle.len() >= LIFECYCLE_PENDING_CAPACITY {
                    return Err(event);
                }
                self.lifecycle.push_back(event);
                Ok(())
            }
            Event::SyncCompleted(_)
            | Event::SetWifiVerified(_)
            | Event::UrgentPollCompleted { .. }
            | Event::UrgentPollFailed
            | Event::ReminderFacts(_)
            | Event::ReminderDue(_)
            | Event::WifiConfigApplied(_)
            | Event::EffectCompleted(_)
            | Event::EffectFailed(_) => {
                if self.worker.len() >= WORKER_PENDING_CAPACITY {
                    return Err(event);
                }
                self.worker.push_back(event);
                Ok(())
            }
            Event::SleepPrepared { .. } | Event::SleepCommitted(_) | Event::SleepCancelled(_) => {
                if self.sleep.len() >= SLEEP_PENDING_CAPACITY {
                    return Err(event);
                }
                self.sleep.push_back(event);
                Ok(())
            }
            Event::Boot(_) => {
                if self.boot.is_some() {
                    return Err(event);
                }
                self.boot = Some(event);
                Ok(())
            }
            Event::SyncBoundaryDue | Event::SyncSchedulerConfigured(_) => {
                if self.scheduler.is_some() {
                    return Err(event);
                }
                self.scheduler = Some(event);
                Ok(())
            }
        }
    }

    pub fn take_next(&mut self) -> Option<inkwash_logic::app::Event> {
        self.tick
            .take()
            .or_else(|| self.power_poll.take())
            .or_else(|| self.rtc_alarm_snapshot.take())
            .or_else(|| self.buttons.pop_front())
            .or_else(|| self.usb_command.take())
            .or_else(|| self.ble_command.take())
            .or_else(|| self.lifecycle.pop_front())
            .or_else(|| self.worker.pop_front())
            .or_else(|| self.sleep.pop_front())
            .or_else(|| self.boot.take())
            .or_else(|| self.scheduler.take())
    }

    pub fn is_empty(&self) -> bool {
        self.tick.is_none()
            && self.power_poll.is_none()
            && self.rtc_alarm_snapshot.is_none()
            && self.buttons.is_empty()
            && self.usb_command.is_none()
            && self.ble_command.is_none()
            && self.lifecycle.is_empty()
            && self.worker.is_empty()
            && self.sleep.is_empty()
            && self.boot.is_none()
            && self.scheduler.is_none()
    }
}

pub const USB_REPLY_PENDING_CAPACITY: usize = 32;

pub const BLE_REPLY_PENDING_CAPACITY: usize = 32;
const USB_REPLY_MAX_RETRIES: u8 = 3;

pub struct PendingUsbReply {
    pub session_id: u64,
    pub id: Option<String>,
    pub command: Command,
    pub reply: Reply,
    pub queued: QueuedReply,
    pub accepted: bool,
    pub retry_count: u8,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingBleDelivery {
    pub session_id: u64,
    pub generation: u64,
    pub conn_handle: u16,
    pub id: Option<String>,
    pub command: Command,
    pub reply: Reply,
}

pub type PendingBleReply = (u64, u64, u16, u64, Option<String>, Command, Reply);

#[derive(Clone)]
pub struct PendingBleHandoff {
    pub session_id: u64,
    pub generation: u64,
    pub conn_handle: u16,
    pub id: Option<String>,
    pub command: Command,
}

pub struct DeviceContext<'a> {
    pub board: &'a mut Note4Board,

    pub rtc: &'a crate::rtc_executor::RtcExecutor,
    pub counters: &'a PersistedCounters,

    pub sync: &'a SyncTask,
    pub alarm_store: &'a AlarmStore,
    pub todo_store: &'a TodoStore,
    pub inbox_store: &'a InboxStore,
    pub usb_console: &'a mut UsbConsole,
    pub usb_reply_writer: UsbReplyWriter,
    pub ble_control: &'a mut BleControl,

    pub audio_task: Option<&'a crate::audio_task::AudioTask>,

    pub pending_wifi_op: Option<PendingWifiOp>,

    pub ble_session_id: Option<u64>,

    pub ble_connection_generation: Option<u64>,

    pub ble_connection_handle: Option<u16>,
    pub ble_wifi_suspended: bool,
    pub ble_set_wifi_after_resume: Option<WifiCreds>,
    pub ble_start_failure: Option<(u64, String)>,
    pub ble_start_cancelled: bool,

    pub pending_ble_replies: Vec<PendingBleReply>,

    pub pending_ble_deliveries: VecDeque<PendingBleDelivery>,

    pub pending_ble_reply_latch: Option<PendingBleDelivery>,
    pub pending_ble_handoff: Option<PendingBleHandoff>,
    pub pending_ble_set_wifi_ack: Option<u64>,

    pub pending_ble_pairing_success: Option<(u64, inkwash_logic::app::BlePairingResult)>,

    pub command_sessions: inkwash_logic::command_sessions::CommandSessions,
    pub usb_session_id: u64,

    pub app_runner: std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,

    pub pending_renders:
        std::rc::Rc<std::cell::RefCell<inkwash_logic::epd_registry::RenderRegistry>>,

    pub pending_render_retries: VecDeque<crate::app_runner::AsyncKick>,

    pub pending_render_completion: Option<PendingRenderCompletion>,

    pub pending_sleep_kick: Option<crate::app_runner::AsyncKick>,

    pub prepared_wake_plan: Option<PreparedWakePlan>,

    pub app_runner_enabled: bool,

    pub alarm_poll: inkwash_logic::alarm_flow::AlarmPoll,

    pub pending_alarm_status: Option<Receiver<anyhow::Result<crate::rtc_executor::AlarmStatus>>>,
    pub pending_alarm_snapshot:
        Option<Receiver<anyhow::Result<inkwash_logic::app::RtcAlarmSnapshot>>>,
    pub pending_clock_read: Option<Receiver<anyhow::Result<DateTime>>>,
    pub effect_task: &'a crate::effect_task::EffectTask,
    pub pending_effect_batch: Option<inkwash_logic::app::EffectBatch>,

    pub pending_effect_batches: VecDeque<inkwash_logic::app::EffectBatch>,
    pub worker_batch_in_flight: bool,

    pub pending_app_events: VecDeque<inkwash_logic::app::Event>,

    pub pending_dispatch_event: Option<inkwash_logic::app::Event>,

    pub pending_dispatch_sources: PendingDispatchSources,

    pub pending_effect_notices: Option<Vec<inkwash_logic::runner::BatchNotice>>,

    pub pending_usb_replies: VecDeque<PendingUsbReply>,

    pub pending_usb_reply_latch: Option<PendingUsbReply>,
}

pub(crate) fn dispatch_migrated_command(
    ctx: &mut DeviceContext<'_>,
    runner: &std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
    event: inkwash_logic::app::Event,
    pre_event: Option<inkwash_logic::app::Event>,
    request: Command,
    id: Option<&str>,
    session_id: u64,
) -> anyhow::Result<Option<Reply>> {
    let channel = match &event {
        inkwash_logic::app::Event::UsbCommand(_) => Channel::Usb,
        inkwash_logic::app::Event::BleCommand(_) => Channel::Ble,
        _ => return Ok(None),
    };

    if let Some(id) = id {
        if let Some(reply) = ctx
            .command_sessions
            .lookup(channel, session_id, id, &request)
        {
            return Ok(Some(reply));
        }
    }

    let request_id = id.map(str::to_owned);
    if ctx
        .command_sessions
        .reserve_pending(channel, session_id, request_id.clone(), request.clone())
        .is_err()
    {
        return Ok(Some(Reply::Busy));
    }
    if let Some(pre) = pre_event {
        crate::dispatch_or_retain(runner, pre, ctx)?;
    }
    crate::dispatch_or_retain(runner, event, ctx)?;

    Ok(None)
}

impl DeviceContext<'_> {
    pub fn queue_usb_reply(
        &mut self,
        session_id: u64,
        id: Option<&str>,
        command: &Command,
        reply: &Reply,
    ) -> Result<(), String> {
        if self.pending_usb_replies.iter().any(|pending| {
            pending.session_id == session_id
                && pending.id.as_deref() == id
                && pending.command == *command
                && pending.reply == *reply
        }) || self
            .pending_usb_reply_latch
            .as_ref()
            .is_some_and(|pending| {
                pending.session_id == session_id
                    && pending.id.as_deref() == id
                    && pending.command == *command
                    && pending.reply == *reply
            })
        {
            return Ok(());
        }
        if self.pending_usb_replies.len() >= USB_REPLY_PENDING_CAPACITY {
            if self.pending_usb_reply_latch.is_some() {
                return Err(format!(
                    "USB reply mailbox and producer latch full (capacity {})",
                    USB_REPLY_PENDING_CAPACITY
                ));
            }
            let queued = self.usb_reply_writer.prepare(reply, id);
            self.pending_usb_reply_latch = Some(PendingUsbReply {
                session_id,
                id: id.map(str::to_owned),
                command: command.clone(),
                reply: reply.clone(),
                queued,
                accepted: false,
                retry_count: 0,
            });
            return Ok(());
        }
        let queued = self.usb_reply_writer.prepare(reply, id);
        let accepted = match self.usb_reply_writer.enqueue_owned(queued.clone()) {
            Ok(_) => true,
            Err(ReplyQueueError::Full(_)) => false,
            Err(ReplyQueueError::Disconnected(_)) => false,
        };
        self.pending_usb_replies.push_back(PendingUsbReply {
            session_id,
            id: id.map(str::to_owned),
            command: command.clone(),
            reply: reply.clone(),
            queued,
            accepted,
            retry_count: 0,
        });
        Ok(())
    }

    pub fn service_usb_reply_writer(&mut self) {
        while let Ok(Some(ack)) = self.usb_reply_writer.try_completion() {
            let Some(index) = self
                .pending_usb_replies
                .iter()
                .position(|pending| pending.queued.sequence == ack.sequence)
            else {
                if self
                    .pending_usb_reply_latch
                    .as_ref()
                    .is_some_and(|pending| pending.queued.sequence == ack.sequence)
                {
                    let mut pending = self
                        .pending_usb_reply_latch
                        .take()
                        .expect("USB producer latch");
                    if ack.result.is_err() {
                        pending.accepted = false;
                        pending.retry_count = pending.retry_count.saturating_add(1);
                        if pending.retry_count <= USB_REPLY_MAX_RETRIES {
                            self.pending_usb_reply_latch = Some(pending);
                        } else {
                            self.cancel_usb_reply(&pending);
                        }
                    } else {
                        self.complete_usb_reply(pending);
                    }
                }
                log::debug!("ignoring stale USB reply completion {}", ack.sequence);
                continue;
            };
            if ack.result.is_err() {
                let mut pending = self.pending_usb_replies.remove(index).expect("reply index");
                pending.accepted = false;
                pending.retry_count = pending.retry_count.saturating_add(1);
                log::warn!(
                    "USB reply write failed for session {}: {:?}",
                    pending.session_id,
                    ack.result
                );
                if pending.retry_count <= USB_REPLY_MAX_RETRIES {
                    self.pending_usb_replies.insert(index, pending);
                } else {
                    self.cancel_usb_reply(&pending);
                }
            } else {
                let pending = self.pending_usb_replies.remove(index).expect("reply index");
                self.complete_usb_reply(pending);
            }
        }

        if let Some(pending) = self.pending_usb_reply_latch.as_mut() {
            if !pending.accepted && pending.session_id == self.usb_session_id {
                match self.usb_reply_writer.retry(pending.queued.clone()) {
                    Ok(()) => pending.accepted = true,
                    Err(ReplyQueueError::Full(_)) => {}
                    Err(ReplyQueueError::Disconnected(_)) => {
                        pending.retry_count = pending.retry_count.saturating_add(1);
                    }
                }
            }
        }
        if self
            .pending_usb_reply_latch
            .as_ref()
            .is_some_and(|pending| pending.retry_count > USB_REPLY_MAX_RETRIES)
        {
            let pending = self
                .pending_usb_reply_latch
                .take()
                .expect("USB producer latch");
            self.cancel_usb_reply(&pending);
        }

        let mut terminated_index = None;
        for index in 0..self.pending_usb_replies.len() {
            let Some(pending) = self.pending_usb_replies.get_mut(index) else {
                break;
            };
            if pending.accepted || pending.session_id != self.usb_session_id {
                continue;
            }
            let queued = pending.queued.clone();
            match self.usb_reply_writer.retry(queued) {
                Ok(()) => pending.accepted = true,
                Err(ReplyQueueError::Full(_)) => break,
                Err(ReplyQueueError::Disconnected(_)) => {
                    pending.retry_count = pending.retry_count.saturating_add(1);
                    if pending.retry_count > USB_REPLY_MAX_RETRIES {
                        terminated_index = Some(index);
                    }
                    break;
                }
            }
        }
        if let Some(index) = terminated_index {
            let failed = self.pending_usb_replies.remove(index).expect("reply index");
            self.cancel_usb_reply(&failed);
        }
    }

    fn complete_usb_reply(&mut self, pending: PendingUsbReply) {
        if let Some(id) = pending.id {
            self.command_sessions.complete_terminal(
                Channel::Usb,
                pending.session_id,
                id,
                pending.command,
                pending.reply,
            );
        } else {
            self.command_sessions.complete_untagged_terminal(
                Channel::Usb,
                pending.session_id,
                pending.command,
                pending.reply,
            );
        }
    }

    fn cancel_usb_reply(&mut self, pending: &PendingUsbReply) {
        self.command_sessions.cancel_pending(
            Channel::Usb,
            pending.session_id,
            pending.id.as_deref(),
            &pending.command,
        );
        log::error!(
            "USB reply delivery terminated after {} retries for session {}",
            pending.retry_count,
            pending.session_id
        );
    }

    pub fn drop_stale_usb_replies(&mut self) {
        let current = self.usb_session_id;
        let mut index = 0;
        while index < self.pending_usb_replies.len() {
            if self.pending_usb_replies[index].session_id == current {
                index += 1;
                continue;
            }
            let pending = self.pending_usb_replies.remove(index).expect("reply index");
            self.command_sessions.cancel_pending(
                Channel::Usb,
                pending.session_id,
                pending.id.as_deref(),
                &pending.command,
            );
        }
        if self
            .pending_usb_reply_latch
            .as_ref()
            .is_some_and(|pending| pending.session_id != current)
        {
            let pending = self
                .pending_usb_reply_latch
                .take()
                .expect("USB producer latch");
            self.cancel_usb_reply(&pending);
        }
    }

    pub fn dispatch_event(&mut self, event: inkwash_logic::app::Event) -> anyhow::Result<()> {
        let runner = self.app_runner.clone();
        crate::dispatch_or_retain(&runner, event, self)
    }

    pub fn poll_usb_control(&mut self, _now: Option<&DateTime>) -> anyhow::Result<(bool, bool)> {
        let Some((id, cmd)) = self.usb_console.poll_command() else {
            return Ok((false, false));
        };
        let changes_visible_state = !matches!(cmd, Command::GetStatus);
        if matches!(cmd, Command::SyncNow) && self.pending_wifi_op.is_some() {
            let reply = Reply::Busy;
            if let Err(err) = self.queue_usb_reply(self.usb_session_id, id.as_deref(), &cmd, &reply)
            {
                log::error!("USB Busy reply could not be retained: {err}");
                self.command_sessions.cancel_pending(
                    Channel::Usb,
                    self.usb_session_id,
                    id.as_deref(),
                    &cmd,
                );
            }
            return Ok((false, true));
        }

        let pre_event = if matches!(cmd, Command::SetTimezone { .. }) {
            self.app_runner
                .borrow()
                .last_clock()
                .map(inkwash_logic::app::Event::Tick)
        } else {
            None
        };
        let is_time_write = matches!(cmd, Command::SetRtc { .. } | Command::SetTimezone { .. });
        let request_for_cache = cmd.clone();
        let usb_session_id = self.usb_session_id;
        let event = inkwash_logic::app::Event::UsbCommand(cmd.clone());
        let runner = self.app_runner.clone();
        let reply = dispatch_migrated_command(
            self,
            &runner,
            event,
            pre_event,
            cmd,
            id.as_deref(),
            usb_session_id,
        )?;
        if let Some(reply) = &reply {
            if let Err(err) = self.queue_usb_reply(
                self.usb_session_id,
                id.as_deref(),
                &request_for_cache,
                reply,
            ) {
                log::error!("USB reply could not be retained: {err}");
                self.command_sessions.cancel_pending(
                    Channel::Usb,
                    self.usb_session_id,
                    id.as_deref(),
                    &request_for_cache,
                );
            }
        }
        if is_time_write && matches!(reply, Some(Reply::Ok)) {
            self.pending_alarm_status = None;
            self.pending_alarm_snapshot = None;
            self.pending_clock_read = None;
            self.alarm_poll.observe_alarm_flag(false);
        }
        let changed = matches!(reply, Some(Reply::Ok));
        Ok((changes_visible_state && changed, true))
    }

    pub fn poll_alarm_snapshot(&mut self) -> anyhow::Result<bool> {
        if !self.app_runner_enabled {
            return Ok(false);
        }

        if let Some(reply) = self.pending_alarm_snapshot.take() {
            match reply.try_recv() {
                Ok(Ok(snapshot)) => {
                    self.alarm_poll.mark_snapshot_dispatched();
                    self.dispatch_event(inkwash_logic::app::Event::RtcAlarmSnapshotReady(
                        snapshot,
                    ))?;
                    return Ok(matches!(
                        self.app_runner.borrow().state().screen,
                        inkwash_logic::app::Screen::AlarmRinging
                    ));
                }
                Ok(Err(err)) => {
                    log::warn!("RTC alarm snapshot read failed; stays retryable: {err:#}");
                }
                Err(TryRecvError::Empty) => {
                    self.pending_alarm_snapshot = Some(reply);
                    return Ok(false);
                }
                Err(TryRecvError::Disconnected) => {
                    log::warn!("RTC alarm snapshot executor disconnected");
                }
            }
        }

        if let Some(reply) = self.pending_alarm_status.take() {
            match reply.try_recv() {
                Ok(Ok(status)) => {
                    if self.alarm_poll.observe_alarm_flag(status.alarm_flag) {
                        match self.rtc.request_snapshot() {
                            Ok(snapshot) => self.pending_alarm_snapshot = Some(snapshot),
                            Err(err) => log::warn!("RTC snapshot request failed: {err:#}"),
                        }
                    }
                }
                Ok(Err(err)) => log::warn!("RTC alarm status read failed: {err:#}"),
                Err(TryRecvError::Empty) => {
                    self.pending_alarm_status = Some(reply);
                    return Ok(false);
                }
                Err(TryRecvError::Disconnected) => {
                    log::warn!("RTC alarm status executor disconnected");
                }
            }
        }

        if self.pending_alarm_status.is_none() && self.pending_alarm_snapshot.is_none() {
            match self.rtc.request_alarm_status() {
                Ok(status) => self.pending_alarm_status = Some(status),
                Err(err) => log::warn!("RTC alarm status request failed: {err:#}"),
            }
        }
        let firing = false;

        Ok(firing)
    }

    pub fn settle_sleep_reads(&mut self, timeout: Duration) {
        let Some(reply) = self.pending_alarm_status.take() else {
            return;
        };
        match reply.recv_timeout(timeout) {
            Ok(Ok(status)) => {
                if self.alarm_poll.observe_alarm_flag(status.alarm_flag) {
                    match self.rtc.request_snapshot() {
                        Ok(snapshot) => self.pending_alarm_snapshot = Some(snapshot),
                        Err(err) => log::warn!("RTC snapshot request failed: {err:#}"),
                    }
                }
            }
            Ok(Err(err)) => log::warn!("RTC alarm status read failed: {err:#}"),
            Err(RecvTimeoutError::Timeout) => {
                self.pending_alarm_status = Some(reply);
            }
            Err(RecvTimeoutError::Disconnected) => {
                log::warn!("RTC alarm status executor disconnected");
            }
        }
    }

    pub fn start_sync(&mut self, now: DateTime) -> Result<bool> {
        if self.pending_wifi_op.is_some() || self.ble_wifi_suspended {
            return Ok(false);
        }
        let reply = self.sync.sync_now(now)?;
        self.pending_wifi_op = Some(PendingWifiOp::Sync { reply });
        Ok(true)
    }

    pub fn start_urgent_poll(&mut self) -> Result<()> {
        if self.pending_wifi_op.is_some() || self.ble_wifi_suspended {
            anyhow::bail!("another Wi-Fi operation is already in progress");
        }
        let reply = self.sync.poll_urgent()?;
        self.pending_wifi_op = Some(PendingWifiOp::UrgentPoll { reply });
        Ok(())
    }

    pub fn start_set_wifi(&mut self, creds: WifiCreds) -> Result<bool> {
        if self.pending_wifi_op.is_some() {
            return Ok(false);
        }
        if self.ble_wifi_suspended {
            if self.ble_set_wifi_after_resume.is_some() {
                return Ok(false);
            }
            self.ble_set_wifi_after_resume = Some(creds);
            if let Err(err) = self.stop_ble_pairing() {
                self.ble_set_wifi_after_resume = None;
                return Err(err);
            }
            return Ok(true);
        }
        let reply = self.sync.set_wifi(creds)?;
        self.pending_wifi_op = Some(PendingWifiOp::SetWifi { reply });
        Ok(true)
    }

    pub fn abort_ble_set_wifi_handoff(&mut self) -> Result<()> {
        self.ble_set_wifi_after_resume = None;
        if self.ble_session_id.is_some() {
            if let Err(err) = self.stop_ble_pairing() {
                self.ble_wifi_suspended = false;
                self.ble_session_id = None;
                self.ble_start_cancelled = false;
                return Err(err);
            }
        }
        Ok(())
    }

    fn start_ble_set_wifi_after_resume(&mut self, creds: WifiCreds) -> anyhow::Result<()> {
        match self.sync.set_wifi(creds) {
            Ok(reply) => {
                log::info!("BLE SetWifi handoff: verifying credentials");
                self.pending_wifi_op = Some(PendingWifiOp::PostBleSetWifi { reply });
            }
            Err(err) => {
                log::error!("BLE SetWifi handoff: failed to queue Wi-Fi verification: {err:#}");
                self.feed_set_wifi_verified(Err(format!(
                    "Wi-Fi verification could not start: {err:#}"
                )))?;
            }
        }
        Ok(())
    }

    pub fn start_ble_pairing(
        &mut self,
        request: &inkwash_logic::app::BlePairingRequest,
    ) -> Result<bool> {
        if self.pending_wifi_op.is_some()
            || self.ble_session_id.is_some()
            || self.ble_wifi_suspended
        {
            return Ok(false);
        }
        let reply = self.sync.suspend_for_ble()?;
        self.ble_session_id = Some(request.session_id);
        self.ble_start_cancelled = false;
        self.pending_wifi_op = Some(PendingWifiOp::SuspendForBle {
            reply,
            session_id: request.session_id,
            name: request.name.clone(),
        });
        Ok(true)
    }

    fn resume_wifi_after_ble(&mut self) {
        if self.pending_wifi_op.is_some() || !self.ble_wifi_suspended {
            if !self.ble_wifi_suspended {
                self.ble_session_id = None;
            }
            return;
        }
        match self.sync.resume_after_ble() {
            Ok(reply) => {
                self.pending_wifi_op = Some(PendingWifiOp::ResumeAfterBle { reply });
            }
            Err(err) => {
                self.ble_set_wifi_after_resume = None;
                self.ble_wifi_suspended = false;
                self.ble_session_id = None;
                self.ble_start_cancelled = false;
                log::error!("Failed to queue Wi-Fi resume after BLE: {err:#}");
            }
        }
    }

    pub fn take_ble_start_failure(&mut self) -> Option<(u64, String)> {
        self.ble_start_failure.take()
    }

    pub fn ble_stopped(&mut self, session_id: u64) -> bool {
        if self.ble_session_id != Some(session_id) {
            return false;
        }
        self.resume_wifi_after_ble();
        true
    }

    pub fn stop_ble_pairing(&mut self) -> Result<()> {
        if matches!(
            &self.pending_wifi_op,
            Some(PendingWifiOp::SuspendForBle { .. })
        ) {
            self.ble_start_cancelled = true;
            return Ok(());
        }
        self.ble_control.stop(self.ble_session_id.unwrap_or(0))
    }

    pub fn poll_wifi_ops(&mut self) -> anyhow::Result<()> {
        let Some(op) = self.pending_wifi_op.take() else {
            return Ok(());
        };
        match op {
            PendingWifiOp::Sync { reply } => match reply.try_recv() {
                Ok(result) => {
                    self.feed_sync_completed(&result)?;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::Sync { reply });
                }
                Err(_) => {
                    self.feed_sync_completed(&sync::SyncResult {
                        outcome: Err(anyhow::anyhow!("sync task disconnected")),
                        ntp_epoch: None,
                    })?;
                }
            },
            PendingWifiOp::SetWifi { reply } => match reply.try_recv() {
                Ok(result) => {
                    self.feed_set_wifi_verified(result.map_err(|e| e.to_string()))?;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::SetWifi { reply });
                }
                Err(_) => {
                    self.feed_set_wifi_verified(Err(
                        "Wi-Fi verification task disconnected".to_string()
                    ))?;
                }
            },
            PendingWifiOp::PostBleSetWifi { reply } => match reply.try_recv() {
                Ok(Ok(verified)) => {
                    log::info!("BLE SetWifi handoff: Wi-Fi credentials verified");
                    self.feed_set_wifi_verified(Ok(verified))?;
                }
                Ok(Err(err)) => {
                    log::warn!("BLE SetWifi handoff: Wi-Fi verification failed: {err}");
                    self.feed_set_wifi_verified(Err(err.to_string()))?;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::PostBleSetWifi { reply });
                }
                Err(_) => {
                    log::warn!("BLE SetWifi handoff: Wi-Fi verification task disconnected");
                    self.feed_set_wifi_verified(Err(
                        "Wi-Fi verification task disconnected".to_string()
                    ))?;
                }
            },
            PendingWifiOp::UrgentPoll { reply } => match reply.try_recv() {
                Ok(Ok(available)) => {
                    self.dispatch_event(inkwash_logic::app::Event::UrgentPollCompleted {
                        available,
                    })?;
                }
                Ok(Err(err)) => {
                    log::warn!("Urgent poll failed: {err}");
                    self.dispatch_event(inkwash_logic::app::Event::UrgentPollFailed)?;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::UrgentPoll { reply });
                }
                Err(_) => {
                    self.dispatch_event(inkwash_logic::app::Event::UrgentPollFailed)?;
                }
            },
            PendingWifiOp::SuspendForBle {
                reply,
                session_id,
                name,
            } => match reply.try_recv() {
                Ok(Ok(())) => {
                    self.ble_wifi_suspended = true;
                    let still_pairing = {
                        let runner = self.app_runner.borrow();
                        inkwash_logic::app::ble_pairing_session_matches(
                            &runner.state().screen,
                            session_id,
                        )
                    };
                    if self.ble_start_cancelled || !still_pairing {
                        self.ble_start_cancelled = false;
                        self.resume_wifi_after_ble();
                    } else if let Err(err) = self.ble_control.start(&name, session_id) {
                        self.ble_start_failure = Some((
                            session_id,
                            format!("BLE worker start queue failed: {err:#}"),
                        ));
                    }
                }
                Ok(Err(err)) => {
                    self.ble_start_failure =
                        Some((session_id, format!("Wi-Fi suspend failed: {err:#}")));
                    self.ble_session_id = Some(session_id);
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::SuspendForBle {
                        reply,
                        session_id,
                        name,
                    });
                }
                Err(_) => {
                    self.ble_start_failure =
                        Some((session_id, "Wi-Fi suspend task disconnected".to_string()));
                }
            },
            PendingWifiOp::ResumeAfterBle { reply } => match reply.try_recv() {
                Ok(Ok(())) => {
                    let post_ble_set_wifi = self.ble_set_wifi_after_resume.take();
                    self.ble_wifi_suspended = false;
                    self.ble_session_id = None;
                    if let Some(creds) = post_ble_set_wifi {
                        self.start_ble_set_wifi_after_resume(creds)?;
                    }
                }
                Ok(Err(err)) => {
                    self.ble_set_wifi_after_resume = None;
                    self.ble_wifi_suspended = false;
                    self.ble_session_id = None;
                    self.ble_start_cancelled = false;
                    log::error!("Wi-Fi resume after BLE failed: {err:#}");
                    self.feed_set_wifi_verified(Err(format!(
                        "Wi-Fi resume after BLE failed: {err:#}"
                    )))?;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::ResumeAfterBle { reply });
                }
                Err(_) => {
                    self.ble_set_wifi_after_resume = None;
                    self.ble_wifi_suspended = false;
                    self.ble_session_id = None;
                    self.ble_start_cancelled = false;
                    log::error!("Wi-Fi resume task disconnected after BLE");
                    self.feed_set_wifi_verified(Err(
                        "Wi-Fi resume task disconnected after BLE".to_string()
                    ))?;
                }
            },
        }
        Ok(())
    }

    fn feed_sync_completed(&mut self, result: &sync::SyncResult) -> anyhow::Result<()> {
        let sm_result = match &result.outcome {
            Ok(sync::SyncOutcome::Applied {
                alarms,
                todos,
                inbox,
                inbox_read_acked,
                inbox_truncated,
                etag,
                uploaded_alarm_ids,
                uploaded_todo_ids,
            }) => {
                let last_sync_epoch = self
                    .app_runner
                    .borrow()
                    .last_clock()
                    .map(|now| now.to_unix())
                    .unwrap_or(0);
                inkwash_logic::app::SyncResult::OkWithMetadata {
                    data: inkwash_logic::app::SyncedData {
                        alarms: alarms.clone(),
                        todos: todos.clone(),
                        inbox: inbox.clone(),
                        inbox_read_acked: inbox_read_acked.clone(),
                        inbox_truncated: *inbox_truncated,
                        etag: etag.clone(),
                        uploaded_alarm_ids: uploaded_alarm_ids.clone(),
                        uploaded_todo_ids: uploaded_todo_ids.clone(),
                    },
                    last_sync_epoch,
                    ntp_epoch: result.ntp_epoch,
                }
            }
            Err(err) => inkwash_logic::app::SyncResult::Failed(err.to_string()),
        };
        let runner = self.app_runner.clone();
        crate::dispatch_or_retain(
            &runner,
            inkwash_logic::app::Event::SyncCompleted(sm_result),
            self,
        )
    }

    fn feed_set_wifi_verified(
        &mut self,
        result: Result<crate::storage::WifiCreds, String>,
    ) -> anyhow::Result<()> {
        let runner = self.app_runner.clone();
        crate::dispatch_or_retain(
            &runner,
            inkwash_logic::app::Event::SetWifiVerified(result),
            self,
        )
    }
}
