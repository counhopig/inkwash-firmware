//! DeviceContext: a single bundle of the firmware's long-lived shared state,
//! so screen/sync/control functions pass one `&mut DeviceContext` instead of
//! threading individual arguments through every call site. `clock` is
//! intentionally NOT here: it's a transient value re-read from the RTC each
//! poll, so it stays an explicit parameter where it's needed.
//!
//! `ble_control` is a main-loop handle to the dedicated BLE worker. The
//! worker owns the NimBLE session while this context polls callback facts and
//! forwards replies without touching thread-affine radio handles.
//!
//! The store fields are `&'a` (immutable) because their methods all take
//! `&self` (the underlying NVS handles have internal mutability); only
//! `board`, `wifi_mgr`, `usb_console`, and `ble_control` need `&'a mut`. This
//! lets a function read a store and mutate the board in the same scope
//! without fighting the borrow checker.

use anyhow::Result;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};

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

/// Platform fact retained between the successful PrepareSleep effect and the
/// final commit check. The operation IDs distinguish a stale prepare/commit
/// kick from the current token.
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

/// Fixed producer-owned latches used when the runtime input queue is full.
/// Each source keeps its own ordering and admission bound; mergeable facts
/// retain only their newest value. The latches are deliberately separate from
/// the runtime FIFO so a busy application queue cannot consume a source fact.
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
    /// Retain a source event without allocating beyond its fixed source
    /// bound. Returns the event to its producer when that source's owner must
    /// retry it before producing another one.
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

/// Fixed ownership capacity for every USB reply, including immediate Busy and
/// cached replies. A reply remains in this queue until the writer acknowledges
/// it or the bounded retry policy terminates it.
pub const USB_REPLY_PENDING_CAPACITY: usize = 32;
/// Fixed ownership capacity for every BLE reply. The queue covers both
/// deliveries waiting for the worker and replies already tracked by it.
pub const BLE_REPLY_PENDING_CAPACITY: usize = 32;
const USB_REPLY_MAX_RETRIES: u8 = 3;

/// Main-loop ownership of a USB reply until the writer task acknowledges the
/// frame. The rendered frame is retained so a short write or a full writer
/// queue can be retried without re-running the command.
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

/// One BLE reply accepted by the worker and awaiting its NimBLE notify-tx
/// completion, keyed by the transport session it was sent on. Field order:
/// `(session_id, generation, conn_handle, reply_id, delivery_id, command, reply)`.
pub type PendingBleReply = (u64, u64, u16, u64, Option<String>, Command, Reply);

/// BLE SetWifi keeps its logical transport key across the radio handoff. The
/// physical connection may disappear before verification/persistence finishes;
/// the eventual terminal reply then either uses a reconnected transport or is
/// explicitly terminated and released.
#[derive(Clone)]
pub struct PendingBleHandoff {
    pub session_id: u64,
    pub generation: u64,
    pub conn_handle: u16,
    pub id: Option<String>,
    pub command: Command,
}

/// All of the firmware's shared, long-lived state, created once in `main()`
/// and threaded through the UI/sync/control layers by reference.
pub struct DeviceContext<'a> {
    pub board: &'a mut Note4Board,
    /// RTC executor client: the only way business/UI code reaches the RTC.
    /// The PCF8563 driver itself lives on the executor task
    /// (`rtc_executor.rs`); `DeviceContext` never holds a `Pcf8563`.
    pub rtc: &'a crate::rtc_executor::RtcExecutor,
    pub counters: &'a PersistedCounters,
    /// Handle to the sync task, which owns the process's one `WifiManager`
    /// (replaces the old direct `wifi_mgr` field - see `sync_task.rs`).
    pub sync: &'a SyncTask,
    pub alarm_store: &'a AlarmStore,
    pub todo_store: &'a TodoStore,
    pub inbox_store: &'a InboxStore,
    pub usb_console: &'a mut UsbConsole,
    pub usb_reply_writer: UsbReplyWriter,
    pub ble_control: &'a mut BleControl,
    /// Handle to the audio task (the sole owner of the ES8311 codec). `None`
    /// when the codec failed to initialise at boot (audio is a degraded-run
    /// category, never a boot blocker). Every tone - alarm ring, reminder -
    /// goes through this handle; nobody touches the codec directly.
    pub audio_task: Option<&'a crate::audio_task::AudioTask>,
    /// One long-running Wi-Fi operation at a time (the sync task
    /// serializes them anyway); `None` when idle. The receipt is polled by
    /// [`DeviceContext::poll_wifi_ops`].
    pub pending_wifi_op: Option<PendingWifiOp>,
    /// Session currently arbitrating the shared radio.  The ID is carried
    /// through every completion so a stale worker result cannot resume Wi-Fi
    /// for a newer pairing session.
    pub ble_session_id: Option<u64>,
    /// Monotonic generation of the current GATT connection. This is distinct
    /// from the pairing screen session: a reconnect must invalidate replies
    /// from the previous client even when pairing remains open.
    pub ble_connection_generation: Option<u64>,
    /// Handle of the single accepted GATT connection. Replies are routed to
    /// this handle so a rejected/reconnected client cannot receive an old
    /// session's notification.
    pub ble_connection_handle: Option<u16>,
    pub ble_wifi_suspended: bool,
    pub ble_set_wifi_after_resume: Option<WifiCreds>,
    pub ble_start_failure: Option<(u64, String)>,
    pub ble_start_cancelled: bool,
    /// BLE replies accepted by the worker but awaiting the NimBLE
    /// notify-tx completion callback. The tuple retains the original command
    /// so the command-session cache is completed only after actual delivery.
    pub pending_ble_replies: Vec<PendingBleReply>,
    /// BLE reply requests that could not yet reserve a worker reply slot.
    /// These own the rendered reply metadata until the current connection
    /// can accept them or the connection/session becomes stale.
    pub pending_ble_deliveries: VecDeque<PendingBleDelivery>,
    /// One producer-owned BLE reply retained when the fixed queue is full.
    /// A second overflow is explicitly rejected after this bounded latch.
    pub pending_ble_reply_latch: Option<PendingBleDelivery>,
    pub pending_ble_handoff: Option<PendingBleHandoff>,
    pub pending_ble_set_wifi_ack: Option<u64>,
    /// BLE pairing success is retained until the state-machine event is
    /// admitted; the notify completion itself has already been consumed.
    pub pending_ble_pairing_success: Option<(u64, inkwash_logic::app::BlePairingResult)>,
    /// Per-transport cache of the last id-tagged command that actually
    /// executed on USB or BLE, and the reply it produced. A resend with the
    /// same id + command on the same transport replays the cached reply
    /// instead of re-executing - see `dispatch_migrated_command`'s doc
    /// comment. Kept per-transport so a USB command's id never collides
    /// with a BLE command's id.
    pub command_sessions: inkwash_logic::command_sessions::CommandSessions,
    pub usb_session_id: u64,
    /// [`DeviceContext::poll_alarm_snapshot`] dispatches to AppRunner so
    /// the bounded alert loops (reminder overlays, which briefly own the
    /// main thread) can still ring an alarm through the same state machine.
    /// Clone the `Rc` out, then call `dispatch` with `self` as the driver
    /// context - no self-referential borrow.
    pub app_runner: std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
    /// Unified registry of in-flight AppRunner render kicks, shared by the
    /// main loop and the bounded alert loops so an EPD completion can
    /// always find its kick by `request_id`, regardless of who dispatched
    /// it.
    pub pending_renders:
        std::rc::Rc<std::cell::RefCell<inkwash_logic::epd_registry::RenderRegistry>>,
    /// Render kicks that arrived while the fixed registry was full. They
    /// retain ownership until a later turn admits them.
    pub pending_render_retries: VecDeque<crate::app_runner::AsyncKick>,
    /// One terminal EPD completion retained until its state-machine event is
    /// admitted. The EPD source has already emitted the fact, so clearing the
    /// registry before this handoff would otherwise strand the render op.
    pub pending_render_completion: Option<PendingRenderCompletion>,
    /// Sleep platform handshakes are held until the next complete polling
    /// turn so prepare/commit never recursively bypass input collection.
    pub pending_sleep_kick: Option<crate::app_runner::AsyncKick>,
    /// Wake-source configuration succeeded for this exact sleep token. This
    /// is populated only after PrepareSleep returns an async kick and is
    /// consumed by the collector's final prepare/commit facts.
    pub prepared_wake_plan: Option<PreparedWakePlan>,
    /// Disabled after a core boot fact failure. Shared by the main loop
    /// and every collection path so no entry can dispatch to a default
    /// AppState after a corrupt boot. Set by `main` from its own
    /// `app_runner_enabled`; `poll_alarm_snapshot` checks it before
    /// dispatching.
    pub app_runner_enabled: bool,
    /// Shared alarm-poll orchestration (edge tracking + sticky exit
    /// flag). Host-testable in `inkwash-logic::alarm_flow`; this field
    /// holds the state so `poll_alarm_snapshot` runs the exact same
    /// `AlarmPoll` logic the harness drives.
    pub alarm_poll: inkwash_logic::alarm_flow::AlarmPoll,
    /// Non-blocking RTC collection requests. The main loop submits a status
    /// read, then a consistent snapshot only after the status reply arrives.
    pub pending_alarm_status: Option<Receiver<anyhow::Result<crate::rtc_executor::AlarmStatus>>>,
    pub pending_alarm_snapshot:
        Option<Receiver<anyhow::Result<inkwash_logic::app::RtcAlarmSnapshot>>>,
    pub pending_clock_read: Option<Receiver<anyhow::Result<DateTime>>>,
    pub effect_task: &'a crate::effect_task::EffectTask,
    pub pending_effect_batch: Option<inkwash_logic::app::EffectBatch>,
    pub worker_batch_in_flight: bool,
    /// Bounded producer-owned events retained while the runtime queue or the
    /// worker continuation is busy. Events are admitted in FIFO order on the
    /// next service turn, so collection never waits for NVS/RTC work.
    pub pending_app_events: VecDeque<inkwash_logic::app::Event>,
    /// Event owned across an effect-worker service failure. This slot is
    /// filled before touching the worker, so a disconnected worker cannot
    /// consume a caller's event without a retry owner.
    pub pending_dispatch_event: Option<inkwash_logic::app::Event>,
    /// Source-owned latches retained while the runtime FIFO is full.
    pub pending_dispatch_sources: PendingDispatchSources,
    /// One bounded continuation for notices produced by a main-thread batch
    /// when Runtime's high-priority queue is temporarily full.
    pub pending_effect_notices: Option<Vec<inkwash_logic::runner::BatchNotice>>,
    /// USB replies retained until the bounded writer acknowledges them.
    pub pending_usb_replies: VecDeque<PendingUsbReply>,
    /// One producer-owned USB reply retained when the fixed queue is full.
    pub pending_usb_reply_latch: Option<PendingUsbReply>,
}

/// Dispatches one migrated command into the shared state-machine runner.
/// Mirrors `control::dispatch`'s dedup contract: a client resend with the
/// same id + command replays the cached reply instead of re-executing.
/// Returns the reply the state machine produced for the channel that sent
/// the frame (if any). The caller writes it to the transport so the reply
/// is written exactly once.
///
/// `pre_event` (when `Some`) is pushed before the command in the same
/// pump: the runtime processes it first, so a time-sensitive command can
/// observe a just-collected fact (e.g. SetTimezone shifting the RTC needs
/// the current clock, which a fresh `Tick` provides).
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
    // Dedup: a resent duplicate of the exact (id, Command) replays the
    // cached reply rather than running the command again.
    if let Some(id) = id {
        if let Some(reply) = ctx
            .command_sessions
            .lookup(channel, session_id, id, &request)
        {
            return Ok(Some(reply));
        }
    }
    // Reserve the exact transport correlation before admitting the command.
    // The unified dispatcher may reduce the command immediately, or retain it
    // behind a worker batch; either way the Reply effect is delivered by the
    // main-thread reducer after the actual transport write.
    let request_id = id.map(str::to_owned);
    if ctx
        .command_sessions
        .reserve_pending(channel, session_id, request_id.clone(), request.clone())
        .is_err()
    {
        // The transport already owns another deferred command. Return Busy
        // through the caller's normal bounded transport delivery path; the
        // existing pending correlation remains untouched.
        return Ok(Some(Reply::Busy));
    }
    if let Some(pre) = pre_event {
        crate::dispatch_or_retain(runner, pre, ctx)?;
    }
    crate::dispatch_or_retain(runner, event, ctx)?;
    // Replies are delivered by `deliver_effect_replies`; returning `None`
    // prevents the transport caller from writing a second copy.
    Ok(None)
}

impl DeviceContext<'_> {
    /// Enqueue a USB reply and retain its exact correlation and rendered frame
    /// until the writer reports completion. A full/disconnected writer is
    /// represented by `accepted = false`; the owned frame remains retryable.
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

    /// Consume writer acknowledgements and retry rejected owned frames. Cache
    /// ownership is released only after a successful acknowledgement and is
    /// guarded by the original transport session.
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

    /// A disconnected USB session owns no future reply delivery. Remove only
    /// old-session records; exact session matching prevents this from
    /// cancelling a request in a newly opened session with a reused ID.
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

    /// Dispatches one non-command event (reminder facts, etc.) into the
    /// shared state-machine runner. The unified dispatcher owns admission,
    /// effect execution, completion feedback, and kick routing.
    pub fn dispatch_event(&mut self, event: inkwash_logic::app::Event) -> anyhow::Result<()> {
        let runner = self.app_runner.clone();
        crate::dispatch_or_retain(&runner, event, self)
    }

    /// Services one queued USB command from any UI loop. Returns
    /// `(visible_change, activity)`: `visible_change` is true when a
    /// successful command may have changed visible device state (the
    /// current screen should redraw), and `activity` is true when *any*
    /// command frame was received - used by the main loop's idle/deep-sleep
    /// tracking, where idle counts "no USB frames" (not just
    /// state-changing ones) as idle.
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
        // Migrated commands run through the state machine (Phase 5): the
        // runner's effects (persist/RTC) execute and its Reply effect is
        // written back to USB with the frame's correlation id.
        // SetRtc carries an absolute value and needs no pre-read. For
        // SetTimezone, use the latest completed clock fact already held by
        // the state machine; command collection never waits on I2C.
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
            // Any in-flight alarm snapshot was read before the new clock was
            // committed. Drop that stale completion and restart AF sampling;
            // the state machine's RtcTimeWritten fact is authoritative.
            self.pending_alarm_status = None;
            self.pending_alarm_snapshot = None;
            self.pending_clock_read = None;
            self.alarm_poll.observe_alarm_flag(false);
        }
        let changed = matches!(reply, Some(Reply::Ok));
        Ok((changes_visible_state && changed, true))
    }

    /// Polls the RTC alarm AF edge from a collection path and dispatches a
    /// consistent snapshot to AppRunner via `rtc_alarm_callback`. The
    /// snapshot is only consumed after all three facts (time, AF, AIE)
    /// read consistently; a read failure keeps the edge retryable so the
    /// next poll retries. Returns `true` when AppRunner decided to start
    /// ringing (the caller should return to the main loop / render the
    /// alarm screen).
    ///
    /// This is the only alarm entry available while a page owns the main
    /// thread - `main`'s AF fast path does not run then. It must NOT ACK
    /// or rearm the RTC itself; that stays with the state machine via the
    /// callback.
    /// Polls the RTC alarm AF edge from a collection path and dispatches a
    /// consistent snapshot to AppRunner via the shared `app_runner`
    /// handle. The snapshot is only consumed after all three facts
    /// (time, AF, AIE) read consistently; a read failure keeps the edge
    /// retryable so the next poll retries. Returns `true` when AppRunner
    /// decided to start ringing (the caller should return to the main
    /// loop which drives the ring state machine).
    ///
    /// This is the only alarm entry available while a page owns the main
    /// thread - `main`'s AF fast path does not run then. It must NOT ACK
    /// or rearm the RTC itself; that stays with the state machine via the
    /// dispatch.
    pub fn poll_alarm_snapshot(&mut self) -> anyhow::Result<bool> {
        // Safe mode exits before this runtime context is created. Keep the
        // guard as a defensive check so a platform collection path cannot
        // dispatch an alarm into an uninitialised application state.
        if !self.app_runner_enabled {
            return Ok(false);
        }
        // Collection is a two-step non-blocking exchange. GPIO5/AF polling
        // never waits for I2C: status and the consistent snapshot each stay
        // in a receiver until the executor completes them.
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
        // Firing is now a pure state-machine affair: the SM entered
        // Screen::AlarmRinging, the executor started the alarm tone through
        // the audio task (non-blocking) and rendered the ring frame; ENTER
        // and the ring-deadline Tick dismiss through the SM's shared
        // `dismiss_ringing`. Nothing blocks here - the unified loop keeps
        // serving Button/Tick/USB/BLE/EPD events while the alarm rings.
        Ok(firing)
    }

    /// Runs an urgent/full sync when its wall-clock boundary advances. The
    /// work itself happens on the sync task; this only advances the
    /// boundary cursors and dispatches the command (returning `false`, the
    /// completion side effects - redraw, RTC re-arm - are applied by
    /// [`DeviceContext::poll_wifi_ops`]). Failed attempts are retried at
    /// the next boundary.
    /// Dispatches a full sync to the sync task and records the pending
    /// receipt. Returns `Ok(true)` when dispatched, `Ok(false)` when
    /// another Wi-Fi operation is already in flight (the caller replies
    /// busy / skips).
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

    /// Dispatches Wi-Fi credential verification to the sync task.
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

    /// Cancels a BLE SetWifi handoff after its transport reply can no longer
    /// be delivered. The radio must still be stopped so the normal Wi-Fi
    /// resume receipt restores ownership consistently.
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

    /// Begins BLE radio arbitration without waiting on the sync task.  The
    /// worker is started only after its SuspendForBle receipt arrives.
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

    /// Queue Wi-Fi restoration after the matching BLE worker Stop receipt.
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

    /// Main consumes a failed preflight/suspend attempt as a normal BLE
    /// lifecycle fact, so the state machine returns to Settings and queues
    /// its ordinary Stop teardown path.
    pub fn take_ble_start_failure(&mut self) -> Option<(u64, String)> {
        self.ble_start_failure.take()
    }

    /// Applies a BLE worker result that is independent of the state-machine
    /// screen.  Returns `true` when the result belongs to the current radio
    /// session and may affect Wi-Fi ownership.
    pub fn ble_stopped(&mut self, session_id: u64) -> bool {
        if self.ble_session_id != Some(session_id) {
            return false;
        }
        self.resume_wifi_after_ble();
        true
    }

    /// Cancels a start whose Wi-Fi suspend receipt has not arrived yet.  In
    /// that window there is no BLE session to stop; polling the suspend
    /// receipt will restore Wi-Fi directly instead of queueing a stale Start
    /// behind the already-issued UI exit.
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

    /// Polls the pending Wi-Fi operation's receipt (non-blocking). Called
    /// every main-loop iteration and by collection paths via
    /// [`DeviceContext::poll_background`]; applies completion side effects
    /// (RTC re-arm, NTP alignment, deferred transport replies) and returns
    /// the completed operation's event for the caller.
    pub fn poll_wifi_ops(&mut self) -> anyhow::Result<()> {
        let Some(op) = self.pending_wifi_op.take() else {
            return Ok(());
        };
        match op {
            PendingWifiOp::Sync { reply } => match reply.try_recv() {
                Ok(result) => {
                    // Every completed sync - scheduled or on-demand - is
                    // fed through the state machine, which owns the merged
                    // data apply (Effect::ApplySyncedData) and the transport
                    // reply when a SyncNow slot awaits. The RTC/NVS-
                    // maintenance side effects stay on the main loop here.
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
                    // Every SetWifi is state-machine-routed (its reply is
                    // delivered through the machine when a slot awaits).
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

    /// Feeds a completed sync receipt into the state machine as
    /// `Event::SyncCompleted`. The machine applies the merged data
    /// (`Effect::ApplySyncedData`) for every sync - scheduled and
    /// on-demand - and lets the unified dispatcher resolve any pending
    /// transport reply.
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

    /// Feeds a completed SetWifi result through the unified application
    /// dispatcher. The state machine and dispatcher own reply correlation.
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
