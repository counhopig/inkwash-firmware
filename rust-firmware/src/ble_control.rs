//! BLE GATT control channel for on-demand pairing (Phase 5).
//!
//! NimBLE owns thread-affine global state and its initialization can block for
//! several seconds. `BleControl` is therefore only a main-loop handle. A
//! dedicated worker owns the `BleSession` and serializes start, replies, and
//! stop commands; callback facts are forwarded through bounded channels.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::{Arc as StdArc, Condvar, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp32_nimble::utilities::mutex::Mutex;
use esp32_nimble::{
    uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice, NimbleProperties, NotifyTxStatus,
};

use crate::control;

const CHANNEL_CAPACITY: usize = 16;
const BLE_REPLY_MAX_RETRIES: u8 = 3;
/// BLE setup is driven from this worker, but the pthread stack itself lives
/// in internal RAM because the ESP32-S3 controller initialization runs with
/// the cache disabled and cannot safely touch a PSRAM stack. 16 KiB gives the
/// NimBLE host/controller initialization path headroom over the configured
/// 5120-byte NimBLE host stack; Wi-Fi is fully deinitialized before this
/// worker starts BLE, so the internal heap remains sufficient for the
/// controller.
const BLE_TASK_STACK: usize = 16 * 1024;
/// Match the ESP32-S3 controller's allocation capabilities. NimBLE host
/// buffers are separate; controller startup uses internal DMA-capable RAM.
const BLE_INTERNAL_CAPS: u32 =
    esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_DMA;

const SERVICE_UUID: &str = "d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4";
const WRITE_CHAR_UUID: &str = "d2c25e51-5e22-48d8-a8b3-34f2f8e2c7d4";
const NOTIFY_CHAR_UUID: &str = "d2c25e52-5e22-48d8-a8b3-34f2f8e2c7d4";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleLifecycle {
    Connected {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
    },
    Disconnected {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleCommand {
    pub id: Option<String>,
    pub command: control::Command,
    pub session_id: u64,
    pub generation: u64,
    pub conn_handle: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleTaskResult {
    Started {
        session_id: u64,
    },
    Failed {
        session_id: u64,
        message: String,
    },
    Stopped {
        session_id: u64,
    },
    ReplyDelivered {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
        reply_id: u64,
    },
    ReplyFailed {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
        reply_id: u64,
    },
    ReplyTerminated {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
        reply_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleReplyError {
    QueueFull,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotifyTxEvent {
    attempt: NotifyAttempt,
    conn_handle: u16,
    success: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotifyAttempt {
    session_id: u64,
    generation: u64,
    conn_handle: u16,
    attempt_id: u64,
}

struct NotifyAttemptMailbox {
    armed: Option<NotifyAttempt>,
    /// NimBLE notify callbacks carry no opaque attempt id or connection epoch.
    /// Once a callback may have escaped (timeout, immediate notify failure, or
    /// disconnect), retire that numeric handle for this NimBLE session. A new
    /// connection generation may use another handle, but handle reuse is
    /// rejected until the whole session is restarted, so a late callback can
    /// never consume a new generation's inflight reply.
    retired_handles: [u64; 1024],
}

impl Default for NotifyAttemptMailbox {
    fn default() -> Self {
        Self {
            armed: None,
            retired_handles: [0; 1024],
        }
    }
}

impl NotifyAttemptMailbox {
    fn is_retired(&self, conn_handle: u16) -> bool {
        let word = usize::from(conn_handle) / 64;
        let bit = usize::from(conn_handle) % 64;
        self.retired_handles[word] & (1u64 << bit) != 0
    }

    fn retire_handle(&mut self, conn_handle: u16) {
        let word = usize::from(conn_handle) / 64;
        let bit = usize::from(conn_handle) % 64;
        self.retired_handles[word] |= 1u64 << bit;
    }

    fn arm(&mut self, attempt: NotifyAttempt) -> bool {
        if self.armed.is_some() || self.is_retired(attempt.conn_handle) {
            return false;
        }
        self.armed = Some(attempt);
        true
    }

    fn quarantine(&mut self, attempt: NotifyAttempt) {
        if self.armed == Some(attempt) {
            self.armed = None;
        }
        self.retire_handle(attempt.conn_handle);
    }

    fn release_generation(&mut self, session_id: u64, generation: u64, conn_handle: u16) {
        if self.armed.is_some_and(|attempt| {
            attempt.session_id == session_id
                && attempt.generation == generation
                && attempt.conn_handle == conn_handle
        }) {
            self.armed = None;
        }
        self.retire_handle(conn_handle);
    }

    fn take_for_callback(&mut self, conn_handle: u16) -> Option<NotifyAttempt> {
        let attempt = self.armed?;
        if attempt.conn_handle != conn_handle {
            return None;
        }
        self.armed.take()
    }
}

#[derive(Clone)]
struct NotifyTxSender {
    tx: mpsc::SyncSender<NotifyTxEvent>,
    pending: StdArc<StdMutex<VecDeque<NotifyTxEvent>>>,
    attempts: StdArc<StdMutex<NotifyAttemptMailbox>>,
}

fn send_notify_tx(sender: &NotifyTxSender, event: NotifyTxEvent) {
    let Ok(mut pending) = sender.pending.lock() else {
        log::error!("BLE notify-tx callback mailbox poisoned; awaiting timeout");
        return;
    };
    // Once one callback has spilled into the fallback FIFO, keep subsequent
    // callbacks there too. This preserves callback order across the channel
    // and fallback boundary, so an older attempt cannot be observed after a
    // newer in-flight attempt.
    if !pending.is_empty() {
        if pending.len() < CHANNEL_CAPACITY {
            pending.push_back(event);
        } else {
            log::warn!("BLE notify-tx callback mailbox full; awaiting timeout");
        }
        return;
    }
    match sender.tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(event)) | Err(mpsc::TrySendError::Disconnected(event)) => {
            // The callback cannot block. Preserve events in a bounded FIFO;
            // never overwrite an earlier callback. If even this mailbox is
            // full, the worker's in-flight timeout produces a terminal result.
            if pending.len() < CHANNEL_CAPACITY {
                pending.push_back(event);
            } else {
                log::warn!("BLE notify-tx callback mailbox full; awaiting timeout");
            }
        }
    }
}

#[derive(Clone)]
struct LifecycleSender {
    mailbox: StdArc<(StdMutex<LifecycleMailbox>, Condvar)>,
}

fn send_lifecycle(sender: &LifecycleSender, event: BleLifecycle) {
    let (lock, available) = &*sender.mailbox;
    let mut mailbox = lock.lock().expect("BLE lifecycle mailbox poisoned");
    while mailbox.queue.len() >= CHANNEL_CAPACITY {
        mailbox = available
            .wait(mailbox)
            .expect("BLE lifecycle mailbox poisoned while waiting");
    }
    mailbox.push(event);
}

#[derive(Clone, Copy)]
struct SequencedLifecycle {
    sequence: u64,
    event: BleLifecycle,
}

struct LifecycleMailbox {
    next_sequence: u64,
    delivered_sequence: u64,
    queue: VecDeque<SequencedLifecycle>,
}

impl Default for LifecycleMailbox {
    fn default() -> Self {
        Self {
            next_sequence: 1,
            delivered_sequence: 0,
            queue: VecDeque::with_capacity(CHANNEL_CAPACITY),
        }
    }
}

impl LifecycleMailbox {
    fn push(&mut self, event: BleLifecycle) {
        let observation = SequencedLifecycle {
            sequence: self.next_sequence,
            event,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.queue.push_back(observation);
    }

    fn pop(&mut self) -> Option<BleLifecycle> {
        let observation = self.queue.pop_front();
        if let Some(observation) = observation {
            debug_assert!(observation.sequence > self.delivered_sequence);
            self.delivered_sequence = self.delivered_sequence.max(observation.sequence);
            Some(observation.event)
        } else {
            None
        }
    }
}

fn poll_lifecycle(mailbox: &StdArc<(StdMutex<LifecycleMailbox>, Condvar)>) -> Option<BleLifecycle> {
    let (lock, available) = &**mailbox;
    let mut mailbox = lock.lock().ok()?;
    let event = mailbox.pop();
    if event.is_some() {
        // The worker callback waits for a slot rather than overwriting a
        // required connect/disconnect fact.
        available.notify_one();
    }
    event
}

enum WorkerCommand {
    Start {
        name: String,
        session_id: u64,
    },
    Stop {
        session_id: u64,
    },
    Reply {
        session_id: u64,
        generation: u64,
        conn_handle: u16,
        reply_id: u64,
        json: String,
    },
}

struct PendingReply {
    session_id: u64,
    generation: u64,
    conn_handle: u16,
    reply_id: u64,
    json: String,
    queued: bool,
    retry_count: u8,
}

/// Worker-thread ownership that is held back until the radio is actually
/// needed. Every field is `Send`; `ensure_worker` moves them into the thread.
struct WorkerLaunch {
    command_rx: mpsc::Receiver<WorkerCommand>,
    tx: mpsc::SyncSender<BleCommand>,
    lifecycle_sender: LifecycleSender,
    result_tx: mpsc::SyncSender<BleTaskResult>,
}

/// Main-loop handle for the BLE worker. No NimBLE handle crosses this type's
/// thread boundary; the worker owns the session for its whole lifetime.
pub struct BleControl {
    command_tx: mpsc::SyncSender<WorkerCommand>,
    rx: mpsc::Receiver<BleCommand>,
    lifecycle_mailbox: StdArc<(StdMutex<LifecycleMailbox>, Condvar)>,
    result_rx: mpsc::Receiver<BleTaskResult>,
    /// `Some` until the worker thread has been created on demand.
    worker_launch: Option<WorkerLaunch>,
    pending_replies: Vec<PendingReply>,
    pending_stop: Option<u64>,
    next_reply_id: u64,
}

impl BleControl {
    /// Starts the worker thread. NimBLE is not initialized until the state
    /// machine submits `StartBlePairing`, so boot and the main loop remain
    /// responsive while the radio is being brought up.
    pub fn spawn() -> Result<Self> {
        let (command_tx, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let lifecycle_mailbox =
            StdArc::new((StdMutex::new(LifecycleMailbox::default()), Condvar::new()));
        let lifecycle_mailbox_for_worker = StdArc::clone(&lifecycle_mailbox);
        let (result_tx, result_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        // The worker thread is NOT created here. Bleeding 16 KiB of internal
        // RAM plus a permanently parked pthread from boot, for a radio that is
        // unused until the user opens the pairing screen, is waste on a device
        // whose scarcest resource is internal RAM. The channel ends still
        // exist, so no command can be lost; the OS thread is created by
        // `ensure_worker` on the first `Start`.
        Ok(Self {
            command_tx,
            rx,
            lifecycle_mailbox,
            result_rx,
            worker_launch: Some(WorkerLaunch {
                command_rx,
                tx,
                lifecycle_sender: LifecycleSender {
                    mailbox: lifecycle_mailbox_for_worker,
                },
                result_tx,
            }),
            pending_replies: Vec::new(),
            pending_stop: None,
            next_reply_id: 1,
        })
    }

    /// Creates the worker thread the first time it is needed. The worker is
    /// never torn down once spawned (it parks between sessions), so the launch
    /// material is consumed exactly once.
    fn ensure_worker(&mut self) -> Result<()> {
        let Some(launch) = self.worker_launch.take() else {
            return Ok(());
        };
        log::info!("BLE worker thread starting on demand");
        crate::tasks::spawn_internal_stack("ble", BLE_TASK_STACK, move || {
            run(
                launch.command_rx,
                launch.tx,
                launch.lifecycle_sender,
                launch.result_tx,
            )
        })
    }

    /// Queue radio initialization without blocking the main loop. The worker
    /// thread is created here on first use, not at boot.
    pub fn start(&mut self, name: &str, session_id: u64) -> Result<()> {
        self.ensure_worker()?;
        self.command_tx
            .try_send(WorkerCommand::Start {
                name: name.to_string(),
                session_id,
            })
            .map_err(|err| anyhow!("BLE worker start queue unavailable: {err}"))
    }

    /// Queue radio teardown without blocking the main loop. The worker's
    /// command order guarantees Stop runs before a later Start.
    pub fn stop(&mut self, session_id: u64) -> Result<()> {
        match self.command_tx.try_send(WorkerCommand::Stop { session_id }) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.pending_stop = Some(session_id);
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(anyhow!("BLE worker stop queue unavailable: disconnected"))
            }
        }
    }

    pub fn poll_result(&mut self) -> Option<BleTaskResult> {
        let mut result = self.result_rx.try_recv().ok()?;
        match result {
            BleTaskResult::ReplyDelivered { reply_id, .. }
            | BleTaskResult::ReplyTerminated { reply_id, .. } => {
                self.pending_replies
                    .retain(|reply| reply.reply_id != reply_id);
            }
            BleTaskResult::ReplyFailed {
                session_id,
                generation,
                conn_handle,
                reply_id,
            } => {
                if let Some(index) = self
                    .pending_replies
                    .iter()
                    .position(|reply| reply.reply_id == reply_id)
                {
                    let terminate = {
                        let reply = &mut self.pending_replies[index];
                        if reply.retry_count >= BLE_REPLY_MAX_RETRIES {
                            true
                        } else {
                            reply.retry_count = reply.retry_count.saturating_add(1);
                            reply.queued = false;
                            false
                        }
                    };
                    if terminate {
                        self.pending_replies
                            .retain(|reply| reply.reply_id != reply_id);
                        result = BleTaskResult::ReplyTerminated {
                            session_id,
                            generation,
                            conn_handle,
                            reply_id,
                        };
                    }
                }
            }
            _ => {}
        }
        Some(result)
    }

    pub fn poll_command(&mut self) -> Option<BleCommand> {
        match self.rx.try_recv() {
            Ok(parsed) => Some(parsed),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                log::warn!("BLE: command channel disconnected");
                None
            }
        }
    }

    pub fn poll_lifecycle(&self) -> Option<BleLifecycle> {
        poll_lifecycle(&self.lifecycle_mailbox)
    }

    pub fn drop_pending_generation(&mut self, session_id: u64, generation: u64) {
        self.pending_replies
            .retain(|reply| reply.session_id != session_id || reply.generation != generation);
    }

    /// Forward a reply to the worker-owned notify characteristic.
    pub fn write_reply(
        &mut self,
        reply: &control::Reply,
        id: Option<&str>,
        session_id: u64,
        generation: u64,
        conn_handle: u16,
    ) -> Result<u64, BleReplyError> {
        let json = control::render_reply(reply, id);
        if self.pending_replies.len() >= CHANNEL_CAPACITY {
            return Err(BleReplyError::QueueFull);
        }
        let reply_id = self.next_reply_id;
        self.next_reply_id = self.next_reply_id.wrapping_add(1).max(1);
        let pending = PendingReply {
            session_id,
            generation,
            conn_handle,
            reply_id,
            json: json.clone(),
            queued: false,
            retry_count: 0,
        };
        // Reserve the bounded ownership record before touching the worker
        // queue. A full queue therefore leaves an owned retry record instead
        // of making enqueue and tracking a two-step lossy operation.
        self.pending_replies.push(pending);
        match self.command_tx.try_send(WorkerCommand::Reply {
            session_id,
            generation,
            conn_handle,
            reply_id,
            json,
        }) {
            Ok(()) => {
                self.pending_replies
                    .last_mut()
                    .expect("reply reservation")
                    .queued = true;
            }
            Err(mpsc::TrySendError::Full(_)) => {
                // Accepted into the bounded retry buffer. The caller must
                // treat this as accepted so command-session ownership is not
                // cancelled while the worker drains its command queue.
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.pending_replies.pop();
                return Err(BleReplyError::Disconnected);
            }
        }
        Ok(reply_id)
    }

    pub fn retry_pending_replies(&mut self, session_id: Option<u64>, generation: Option<u64>) {
        if let Some(stop_session_id) = self.pending_stop {
            match self.command_tx.try_send(WorkerCommand::Stop {
                session_id: stop_session_id,
            }) {
                Ok(()) => self.pending_stop = None,
                Err(mpsc::TrySendError::Full(_)) => return,
                Err(mpsc::TrySendError::Disconnected(_)) => self.pending_stop = None,
            }
        }
        self.pending_replies.retain(|reply| {
            session_id == Some(reply.session_id) && generation == Some(reply.generation)
        });
        for reply in &mut self.pending_replies {
            if reply.queued {
                continue;
            }
            match self.command_tx.try_send(WorkerCommand::Reply {
                session_id: reply.session_id,
                generation: reply.generation,
                conn_handle: reply.conn_handle,
                reply_id: reply.reply_id,
                json: reply.json.clone(),
            }) {
                Ok(()) => reply.queued = true,
                Err(mpsc::TrySendError::Full(_)) => break,
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    }
}

impl Drop for BleControl {
    fn drop(&mut self) {
        // Best-effort wakeup; dropping the last sender also makes the worker
        // leave its receive loop and drop any session it still owns.
        let _ = self
            .command_tx
            .try_send(WorkerCommand::Stop { session_id: 0 });
    }
}

fn run(
    command_rx: mpsc::Receiver<WorkerCommand>,
    tx: mpsc::SyncSender<BleCommand>,
    lifecycle_sender: LifecycleSender,
    result_tx: mpsc::SyncSender<BleTaskResult>,
) {
    let mut session = None;
    let mut active_session_id = None;
    loop {
        match command_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(WorkerCommand::Start {
                name: _name,
                session_id,
            }) => {
                log::info!("BLE worker: Start session {session_id}");
                // Never tear down an active session to service a duplicate or
                // corrupted Start. Stop must be received first; dropping the
                // NimBLE session here could run deinit on a live controller.
                if session.is_some() {
                    log::warn!(
                        "BLE worker: rejecting Start session {session_id}; active session {:?}",
                        active_session_id
                    );
                    continue;
                }
                log_stack_high_watermark("before BLE init");
                match BleSession::start(session_id) {
                    Ok(new_session) => {
                        session = Some(new_session);
                        active_session_id = Some(session_id);
                        log_stack_high_watermark("after BLE init");
                        if result_tx
                            .send(BleTaskResult::Started { session_id })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        log_stack_high_watermark("after BLE init failure");
                        if result_tx
                            .send(BleTaskResult::Failed {
                                session_id,
                                message: format!("{err:#}"),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            Ok(WorkerCommand::Stop { session_id }) => {
                log::info!("BLE worker: Stop session {session_id}");
                if session.is_some() && (session_id == 0 || active_session_id == Some(session_id)) {
                    session.take();
                    active_session_id = None;
                } else if session.is_some() {
                    log::warn!(
                        "BLE worker: ignoring stale Stop session {session_id}; active session {:?}",
                        active_session_id
                    );
                }
                if result_tx
                    .send(BleTaskResult::Stopped { session_id })
                    .is_err()
                {
                    break;
                }
            }
            Ok(WorkerCommand::Reply {
                session_id,
                generation,
                conn_handle,
                reply_id,
                json,
            }) => {
                let result: Result<NotifyAttempt, (bool, String)> =
                    if let Some(active) = session.as_mut() {
                        if active.inflight.is_some() {
                            Err((true, "another BLE reply is awaiting notify-tx".to_string()))
                        } else if active.matches(session_id, generation, conn_handle) {
                            let attempt = active.next_attempt(generation, conn_handle);
                            match active.notify(json.as_bytes(), conn_handle, attempt) {
                                Ok(()) => Ok(attempt),
                                Err(err) => Err((false, format!("{err:?}"))),
                            }
                        } else {
                            Err((false, "stale BLE session/connection".to_string()))
                        }
                    } else {
                        Err((false, "BLE session is stopped".to_string()))
                    };
                match result {
                    Ok(attempt) => {
                        if let Some(active) = session.as_mut() {
                            active.inflight = Some(InflightReply {
                                session_id,
                                generation,
                                conn_handle,
                                reply_id,
                                attempt_id: attempt.attempt_id,
                                sent_at: std::time::Instant::now(),
                            });
                        }
                    }
                    Err((retryable, err)) => {
                        log::warn!("BLE reply {reply_id} notify enqueue failed: {err}");
                        if result_tx
                            .send(if retryable {
                                BleTaskResult::ReplyFailed {
                                    session_id,
                                    generation,
                                    conn_handle,
                                    reply_id,
                                }
                            } else {
                                BleTaskResult::ReplyTerminated {
                                    session_id,
                                    generation,
                                    conn_handle,
                                    reply_id,
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(active) = session.as_mut() {
            while let Some(event) = active.poll_lifecycle() {
                if let BleLifecycle::Disconnected {
                    session_id,
                    generation,
                    conn_handle,
                } = event
                {
                    if active.inflight.as_ref().is_some_and(|reply| {
                        reply.session_id == session_id
                            && reply.generation == generation
                            && reply.conn_handle == conn_handle
                    }) {
                        if let Some(reply) = active.inflight.take() {
                            active.quarantine_attempt(NotifyAttempt {
                                session_id: reply.session_id,
                                generation: reply.generation,
                                conn_handle: reply.conn_handle,
                                attempt_id: reply.attempt_id,
                            });
                            if result_tx
                                .send(BleTaskResult::ReplyTerminated {
                                    session_id: reply.session_id,
                                    generation: reply.generation,
                                    conn_handle: reply.conn_handle,
                                    reply_id: reply.reply_id,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    active.release_notify_generation(session_id, generation, conn_handle);
                }
                send_lifecycle(&lifecycle_sender, event);
            }
            if let Some(command) = active.pending_command.take() {
                match tx.try_send(command) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(command)) => {
                        active.pending_command = Some(command);
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
            if active.pending_command.is_none() {
                while let Some(command) = active.poll_command() {
                    match tx.try_send(command) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(command)) => {
                            active.pending_command = Some(command);
                            break;
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            }
            while let Some(event) = active.poll_notify_tx() {
                let Some(inflight) = active.inflight.as_ref() else {
                    continue;
                };
                if inflight.session_id != event.attempt.session_id
                    || inflight.generation != event.attempt.generation
                    || inflight.conn_handle != event.attempt.conn_handle
                    || inflight.conn_handle != event.conn_handle
                    || inflight.attempt_id != event.attempt.attempt_id
                {
                    log::warn!("BLE stale notify-tx callback ignored");
                    continue;
                }
                let inflight = active.inflight.take().expect("inflight checked above");
                let result = if event.success {
                    BleTaskResult::ReplyDelivered {
                        session_id: inflight.session_id,
                        generation: inflight.generation,
                        conn_handle: inflight.conn_handle,
                        reply_id: inflight.reply_id,
                    }
                } else {
                    BleTaskResult::ReplyFailed {
                        session_id: inflight.session_id,
                        generation: inflight.generation,
                        conn_handle: inflight.conn_handle,
                        reply_id: inflight.reply_id,
                    }
                };
                if result_tx.send(result).is_err() {
                    break;
                }
            }
            if active
                .inflight
                .as_ref()
                .is_some_and(|reply| reply.sent_at.elapsed() > Duration::from_secs(2))
            {
                if let Some(reply) = active.inflight.take() {
                    active.quarantine_attempt(NotifyAttempt {
                        session_id: reply.session_id,
                        generation: reply.generation,
                        conn_handle: reply.conn_handle,
                        attempt_id: reply.attempt_id,
                    });
                    if result_tx
                        .send(BleTaskResult::ReplyTerminated {
                            session_id: reply.session_id,
                            generation: reply.generation,
                            conn_handle: reply.conn_handle,
                            reply_id: reply.reply_id,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
    session.take();
}

/// ESP-IDF reports this value in bytes (not the FreeRTOS word units used by
/// the upstream API). Keeping the measurement in the worker log makes a
/// device trace sufficient to distinguish a controller allocation failure
/// from a worker-stack exhaustion before the next BLE regression.
fn log_stack_high_watermark(stage: &str) {
    let free_bytes =
        unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark2(std::ptr::null_mut()) };
    log::info!("BLE worker stack: {stage}, high-water free={free_bytes} bytes");
}

/// The NimBLE-owning half of the BLE implementation. It never crosses the
/// worker boundary, including its callback channels and notify handle.
struct BleSession {
    rx: mpsc::Receiver<BleCommand>,
    _tx: mpsc::SyncSender<BleCommand>,
    lifecycle_mailbox: StdArc<(StdMutex<LifecycleMailbox>, Condvar)>,
    notify_tx_rx: mpsc::Receiver<NotifyTxEvent>,
    notify_tx_pending: StdArc<StdMutex<VecDeque<NotifyTxEvent>>>,
    notify_attempts: StdArc<StdMutex<NotifyAttemptMailbox>>,
    notify_char: Arc<Mutex<BLECharacteristic>>,
    connection_generation: Arc<AtomicU32>,
    connection_handle: Arc<AtomicU32>,
    session_id: u64,
    inflight: Option<InflightReply>,
    next_attempt_id: u64,
    pending_command: Option<BleCommand>,
}

struct InflightReply {
    session_id: u64,
    generation: u64,
    conn_handle: u16,
    reply_id: u64,
    attempt_id: u64,
    sent_at: std::time::Instant,
}

impl BleSession {
    fn start(session_id: u64) -> Result<Self> {
        let free = unsafe { esp_idf_svc::sys::heap_caps_get_free_size(BLE_INTERNAL_CAPS) };
        let largest =
            unsafe { esp_idf_svc::sys::heap_caps_get_largest_free_block(BLE_INTERNAL_CAPS) };
        log::info!("BLE preflight: internal free={free} largest_block={largest} bytes");
        if !inkwash_logic::ble_memory::sufficient_internal_heap(free, largest) {
            return Err(anyhow!(
                "BLE unavailable: internal heap free={free} largest_block={largest}"
            ));
        }
        BLEDevice::init();
        let result = Self::start_initialized(session_id);
        if result.is_err() {
            Self::shutdown_nimble();
        }
        result
    }

    /// Stop advertising while the NimBLE host is still alive. The crate's
    /// `deinit_full()` resets its global advertising object *after* stopping
    /// the host; if advertising is still active that reset calls
    /// `ble_gap_adv_stop()` after `nimble_port_deinit()`, which is a
    /// use-after-deinit on ESP-IDF and panics in `ble_gap_adv_active()`.
    fn shutdown_nimble() {
        let advertising = BLEDevice::take().get_advertising();
        if advertising.lock().is_advertising() {
            if let Err(err) = advertising.lock().stop() {
                log::warn!("BLE advertising stop before deinit failed: {err:?}");
            }
        }
        if let Err(err) = BLEDevice::deinit_full() {
            log::warn!("BLE cleanup after stop failed: {err:?}");
        }
    }

    fn start_initialized(session_id: u64) -> Result<Self> {
        let device = BLEDevice::take();
        let ble_advertising = device.get_advertising();
        let server = device.get_server();

        let lifecycle_mailbox =
            StdArc::new((StdMutex::new(LifecycleMailbox::default()), Condvar::new()));
        let lifecycle_sender = LifecycleSender {
            mailbox: Arc::clone(&lifecycle_mailbox),
        };
        let connection_generation = Arc::new(AtomicU32::new(0));
        const NO_CONNECTION: u32 = u16::MAX as u32;
        let connection_handle = Arc::new(AtomicU32::new(NO_CONNECTION));
        let generation_for_command = Arc::clone(&connection_generation);
        let (notify_tx, notify_tx_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let notify_tx_pending =
            StdArc::new(StdMutex::new(VecDeque::with_capacity(CHANNEL_CAPACITY)));
        let notify_attempts = StdArc::new(StdMutex::new(NotifyAttemptMailbox::default()));
        let notify_sender = NotifyTxSender {
            tx: notify_tx,
            pending: StdArc::clone(&notify_tx_pending),
            attempts: Arc::clone(&notify_attempts),
        };
        let lc_tx = lifecycle_sender.clone();
        let generation_for_connect = Arc::clone(&connection_generation);
        let handle_for_connect = Arc::clone(&connection_handle);
        server.on_connect(move |server, desc| {
            let conn_handle = desc.conn_handle();
            if handle_for_connect
                .compare_exchange(
                    NO_CONNECTION,
                    u32::from(conn_handle),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                log::warn!("BLE second client rejected (conn_handle={conn_handle})");
                let _ = server.disconnect(conn_handle);
                return;
            }
            log::info!("BLE client connected");
            let generation = u64::from(generation_for_connect.fetch_add(1, Ordering::Relaxed) + 1);
            send_lifecycle(
                &lc_tx,
                BleLifecycle::Connected {
                    session_id,
                    generation,
                    conn_handle,
                },
            );
        });
        let lc_tx = lifecycle_sender;
        let generation_for_disconnect = Arc::clone(&connection_generation);
        let handle_for_disconnect = Arc::clone(&connection_handle);
        server.on_disconnect(move |desc, _reason| {
            let conn_handle = desc.conn_handle();
            if handle_for_disconnect
                .compare_exchange(
                    u32::from(conn_handle),
                    NO_CONNECTION,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            log::info!("BLE client disconnected ({_reason:?})");
            let generation = u64::from(generation_for_disconnect.load(Ordering::Relaxed));
            send_lifecycle(
                &lc_tx,
                BleLifecycle::Disconnected {
                    session_id,
                    generation,
                    conn_handle,
                },
            );
        });

        let control_service = server.create_service(uuid128!(SERVICE_UUID));
        let write_char = control_service
            .lock()
            .create_characteristic(uuid128!(WRITE_CHAR_UUID), NimbleProperties::WRITE);
        let notify_char = control_service.lock().create_characteristic(
            uuid128!(NOTIFY_CHAR_UUID),
            NimbleProperties::READ | NimbleProperties::NOTIFY,
        );
        notify_char.lock().set_value(b"");

        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let tx_for_callback = tx.clone();
        write_char.lock().on_write(move |args| {
            let conn_handle = args.desc().conn_handle();
            let generation = u64::from(generation_for_command.load(Ordering::Acquire));
            match String::from_utf8(args.recv_data().to_vec()) {
                Ok(line) => match control::parse_command(&line) {
                    Ok((id, command)) => {
                        if let Err(err) = tx_for_callback.try_send(BleCommand {
                            id,
                            command,
                            session_id,
                            generation,
                            conn_handle,
                        }) {
                            log::warn!("BLE: command queue unavailable: {err}");
                            args.reject();
                        }
                    }
                    Err(err) => {
                        log::warn!("BLE: failed to parse command '{line}': {err}");
                        args.reject();
                    }
                },
                Err(_) => {
                    log::warn!("BLE: received non-UTF-8 command data");
                    args.reject();
                }
            }
        });

        let notify_sender_for_callback = notify_sender;
        notify_char.lock().on_notify_tx(move |event| {
            let conn_handle = event
                .desc()
                .map(|desc| desc.conn_handle())
                .unwrap_or(u16::MAX);
            let Some(attempt) = notify_sender_for_callback
                .attempts
                .lock()
                .ok()
                .and_then(|mut attempts| attempts.take_for_callback(conn_handle))
            else {
                log::warn!("BLE notify-tx callback has no armed attempt; ignoring");
                return;
            };
            let success = matches!(event.status(), NotifyTxStatus::SuccessNotify);
            send_notify_tx(
                &notify_sender_for_callback,
                NotifyTxEvent {
                    attempt,
                    conn_handle,
                    success,
                },
            );
        });

        ble_advertising
            .lock()
            .set_data(
                BLEAdvertisementData::new()
                    .name("Inkwash")
                    .add_service_uuid(uuid128!(SERVICE_UUID)),
            )
            .map_err(|e| anyhow!("BLE set advertisement data failed: {e:?}"))?;
        ble_advertising
            .lock()
            .start()
            .map_err(|e| anyhow!("BLE start advertising failed: {e:?}"))?;
        log::info!("BLE advertising started");

        Ok(Self {
            rx,
            _tx: tx,
            lifecycle_mailbox,
            notify_tx_rx,
            notify_tx_pending,
            notify_attempts,
            notify_char,
            connection_generation,
            connection_handle,
            session_id,
            inflight: None,
            next_attempt_id: 1,
            pending_command: None,
        })
    }

    fn poll_command(&self) -> Option<BleCommand> {
        self.rx.try_recv().ok()
    }

    fn poll_lifecycle(&self) -> Option<BleLifecycle> {
        poll_lifecycle(&self.lifecycle_mailbox)
    }

    fn poll_notify_tx(&self) -> Option<NotifyTxEvent> {
        match self.notify_tx_rx.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
                self.notify_tx_pending.lock().ok()?.pop_front()
            }
        }
    }

    fn notify(
        &self,
        json: &[u8],
        conn_handle: u16,
        attempt: NotifyAttempt,
    ) -> std::result::Result<(), esp32_nimble::BLEError> {
        let armed = self
            .notify_attempts
            .lock()
            .expect("BLE notify attempt mailbox poisoned")
            .arm(attempt);
        if !armed {
            return Err(esp32_nimble::BLEError::fail().expect_err("BLE notify attempt rejected"));
        }
        self.notify_char.lock().set_value(json);
        match self.notify_char.lock().notify_with(json, conn_handle) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.notify_attempts
                    .lock()
                    .expect("BLE notify attempt mailbox poisoned")
                    .quarantine(attempt);
                Err(err)
            }
        }
    }

    fn next_attempt(&mut self, generation: u64, conn_handle: u16) -> NotifyAttempt {
        let attempt = NotifyAttempt {
            session_id: self.session_id,
            generation,
            conn_handle,
            attempt_id: self.next_attempt_id,
        };
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        attempt
    }

    fn quarantine_attempt(&self, attempt: NotifyAttempt) {
        self.notify_attempts
            .lock()
            .expect("BLE notify attempt mailbox poisoned")
            .quarantine(attempt);
    }

    fn release_notify_generation(&self, session_id: u64, generation: u64, conn_handle: u16) {
        self.notify_attempts
            .lock()
            .expect("BLE notify attempt mailbox poisoned")
            .release_generation(session_id, generation, conn_handle);
        while self.notify_tx_rx.try_recv().is_ok() {}
        if let Ok(mut pending) = self.notify_tx_pending.lock() {
            pending.retain(|event| {
                event.attempt.session_id != session_id
                    || event.attempt.generation != generation
                    || event.attempt.conn_handle != conn_handle
            });
        }
    }

    fn matches(&self, session_id: u64, generation: u64, conn_handle: u16) -> bool {
        self.session_id == session_id
            && u64::from(self.connection_generation.load(Ordering::Acquire)) == generation
            && self.connection_handle.load(Ordering::Acquire) == u32::from(conn_handle)
            && self.inflight.is_none()
    }
}

impl Drop for BleSession {
    fn drop(&mut self) {
        BleSession::shutdown_nimble();
        log::info!("BLE control torn down; advertising stopped");
    }
}
