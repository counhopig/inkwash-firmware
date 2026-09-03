//! DeviceContext: a single bundle of the firmware's long-lived shared state,
//! so screen/sync/control functions pass one `&mut DeviceContext` instead of
//! threading individual arguments through every call site. `clock` is
//! intentionally NOT here: it's a transient value re-read from the RTC each
//! poll, so it stays an explicit parameter where it's needed.
//!
//! `ble_control` holds a `&'a mut Option<BleControl>` (not a `BleControl`
//! directly) because its *contents* have a distinct lifetime from the rest
//! of this struct - owned by the main loop, populated only while the BLE
//! pairing screen is open, and torn down (`= None`) when leaving it. Storing
//! the reference to the slot rather than threading `&mut Option<BleControl>`
//! as a separate parameter everywhere means every blocking screen that goes
//! through `DeviceContext` automatically gets the chance to reply `busy` to
//! a queued BLE command instead of leaving BLE the one control channel with
//! no reply at all during a ring/reminder.
//!
//! The store fields are `&'a` (immutable) because their methods all take
//! `&self` (the underlying NVS handles have internal mutability); only
//! `board`, `wifi_mgr`, `usb_console`, and `ble_control` need `&'a mut`. This
//! lets a function read a store and mutate the board in the same scope
//! without fighting the borrow checker.

use anyhow::Result;
use std::sync::mpsc::TryRecvError;

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
use crate::usb_console::UsbConsole;

/// Wall-clock sync cursors shared by Home and every blocking UI loop.
/// Keeping them here prevents each screen from inventing its own timer and
/// ensures a successful first sync disables the fresh-device fast path.
pub struct SyncScheduler {
    last_urgent_boundary: u64,
    last_full_boundary: u64,
    never_synced: bool,
    /// True once a successful full sync has fetched the server state while
    /// the urgent flag was set. The server keeps answering `urgent: true`
    /// until the message is read, so without this flag the device would
    /// run a *full* sync at every 30 s boundary while any unread urgent
    /// message exists. Cleared when a poll reports no
    /// urgent content; a failed urgent-triggered sync leaves it false so
    /// the next boundary retries.
    urgent_synced: bool,
}

impl SyncScheduler {
    pub fn new(now: Option<&DateTime>, counters: &PersistedCounters) -> Self {
        let unix = now.map(|dt| dt.to_unix()).unwrap_or(0);
        let interval = counters.sync_interval_minutes().unwrap_or(60) as u64;
        Self {
            last_urgent_boundary: inkwash_logic::scheduler::boundary_index(unix, 30),
            last_full_boundary: inkwash_logic::scheduler::boundary_index(unix, interval * 60),
            never_synced: counters.last_sync_epoch().unwrap_or(None).is_none(),
            urgent_synced: false,
        }
    }
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
    pub ble_control: &'a mut Option<BleControl>,
    pub sync_scheduler: SyncScheduler,
    /// One long-running Wi-Fi operation at a time (the sync task
    /// serializes them anyway); `None` when idle. The receipt is polled by
    /// [`DeviceContext::poll_wifi_ops`].
    pub pending_wifi_op: Option<PendingWifiOp>,
    /// The last id-tagged command `control::dispatch` actually executed
    /// (USB or BLE, whichever came last), and the reply it produced.
    /// Resent duplicates of that exact `(id, Command)` replay the cached
    /// reply instead of re-executing - see `control::dispatch`'s doc
    /// comment for why this exists and why the key includes `Command`,
    /// not just `id`.
    pub last_command: Option<(String, Command, Reply)>,
    /// When a state-machine-routed SyncNow started a network sync, this
    /// remembers where to write the SM's eventual `Reply` (the transport +
    /// correlation id of the frame that requested it). `None` when no
    /// SM-routed sync is awaiting its receipt. Cleared when the receipt
    /// arrives (or the sync is superseded).
    pub sm_sync_reply_target: Option<(Channel, String)>,
    /// Same as `sm_sync_reply_target` for a state-machine-routed SetWifi:
    /// where to write the machine's eventual reply for the wifi receipt.
    pub sm_wifi_reply_target: Option<(Channel, String)>,
    /// [[`DeviceContext::poll_alarm_snapshot`]] dispatches to AppRunner so
    /// blocking pages (which own the main thread) can still ring an alarm
    /// through the same state machine. Clone the `Rc` out, then call
    /// `dispatch` with `self` as the driver context - no self-referential
    /// borrow.
    pub app_runner: std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
    /// Unified registry of in-flight AppRunner render kicks, shared by the
    /// main loop and every blocking page so an EPD completion can always
    /// find its kick by `request_id`, regardless of who dispatched it.
    pub pending_renders:
        std::rc::Rc<std::cell::RefCell<inkwash_logic::epd_registry::RenderRegistry>>,
    /// Disabled after a core boot fact failure. Shared by the main loop
    /// *and* every blocking page so no entry can dispatch to a default
    /// AppState after a corrupt boot. Set by `main` from its own
    /// `app_runner_enabled`; `poll_alarm_snapshot` checks it before
    /// dispatching.
    pub app_runner_enabled: bool,
    /// Shared alarm-poll orchestration (edge tracking + sticky exit
    /// flag). Host-testable in `inkwash-logic::alarm_flow`; this field
    /// holds the state so `poll_alarm_snapshot` runs the exact same
    /// `AlarmPoll` logic the harness drives.
    pub alarm_poll: inkwash_logic::alarm_flow::AlarmPoll,
}

/// Background-poll outcome vocabulary lives in `inkwash-logic` (pure,
/// host-testable); re-exported here so every firmware call site keeps
/// using `crate::ctx::BackgroundOutcome`.
pub use inkwash_logic::background_outcome::BackgroundOutcome;

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
) -> Option<Reply> {
    let channel = match &event {
        inkwash_logic::app::Event::UsbCommand(_) => Channel::Usb,
        inkwash_logic::app::Event::BleCommand(_) => Channel::Ble,
        _ => return None,
    };
    // Dedup: a resent duplicate of the exact (id, Command) replays the
    // cached reply rather than running the command again.
    if let Some(id) = id {
        if let Some((last_id, last_cmd, last_reply)) = &ctx.last_command {
            if last_id == id && *last_cmd == request {
                return Some(last_reply.clone());
            }
        }
    }
    let last_clock = runner.borrow().last_clock();
    let mut reply_for_channel = None;
    {
        let mut executor = crate::app_runner::EffectRunner::new(ctx, last_clock);
        let mut runtime = runner.borrow_mut();
        if let Some(pre) = pre_event {
            runtime.push(pre);
        }
        runtime.push(event);
        if let Err(err) = runtime.pump(&mut executor) {
            log::warn!("Command dispatch through state machine failed: {err}");
        }
        for (reply_channel, reply) in executor.take_replies() {
            if reply_channel == channel && reply_for_channel.is_none() {
                reply_for_channel = Some(reply);
            }
        }
    }
    for kick in runner.borrow_mut().take_kicks() {
        // A StartSync kick means the state machine accepted a SyncNow and a
        // network sync is now running. Its eventual reply arrives with the
        // sync receipt; remember the transport + correlation id to write it
        // to then (there is no synchronous reply for an accepted SyncNow).
        match &kick.effect {
            inkwash_logic::app::Effect::StartSync(_) => {
                if let Some(id) = id {
                    ctx.sm_sync_reply_target = Some((channel, id.to_string()));
                }
            }
            inkwash_logic::app::Effect::StartSetWifi(_) => {
                if let Some(id) = id {
                    ctx.sm_wifi_reply_target = Some((channel, id.to_string()));
                }
            }
            _ => {}
        }
        let mut reg = ctx.pending_renders.borrow_mut();
        if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
            reg.register(kick);
        } else {
            log::warn!(
                "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                kick.effect,
                kick.operation_id
            );
        }
    }
    // Update the dedup cache with the real result (like control::dispatch).
    if let (Some(id), Some(reply)) = (id, &reply_for_channel) {
        ctx.last_command = Some((id.to_string(), request, reply.clone()));
    }
    reply_for_channel
}

impl DeviceContext<'_> {
    /// Services one queued USB command from any UI loop. Returns
    /// `(visible_change, activity)`: `visible_change` is true when a
    /// successful command may have changed visible device state (the
    /// current screen should redraw), and `activity` is true when *any*
    /// command frame was received - used by the main loop's idle/deep-sleep
    /// tracking, where idle counts "no USB frames" (not just
    /// state-changing ones) as idle.
    pub fn poll_usb_control(&mut self, _now: Option<&DateTime>) -> (bool, bool) {
        let Some((id, cmd)) = self.usb_console.poll_command() else {
            return (false, false);
        };
        let changes_visible_state = !matches!(cmd, Command::GetStatus);
        // Migrated commands run through the state machine (Stage 3): the
        // runner's effects (persist/RTC) execute and its Reply effect is
        // written back to USB with the frame's correlation id.
        // A time-sensitive migrated command (SetTimezone shifts the RTC
        // by the offset delta from the current clock) needs a fresh
        // time fact: push a Tick from the latest RTC read first so the
        // state machine's clock is current when it computes the shift.
        let pre_event = if matches!(cmd, Command::SetTimezone { .. }) {
            self.rtc
                .read_time()
                .ok()
                .map(inkwash_logic::app::Event::Tick)
        } else {
            None
        };
        let event = inkwash_logic::app::Event::UsbCommand(cmd.clone());
        let runner = self.app_runner.clone();
        let reply = dispatch_migrated_command(self, &runner, event, pre_event, cmd, id.as_deref());
        if let Some(reply) = &reply {
            crate::usb_console::write_reply(reply, id.as_deref());
        }
        let changed = matches!(reply, Some(Reply::Ok));
        (changes_visible_state && changed, true)
    }

    /// Polls the RTC alarm AF edge from a blocking page and dispatches a
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
    /// Polls the RTC alarm AF edge from a blocking page and dispatches a
    /// consistent snapshot to AppRunner via the shared `app_runner`
    /// handle. The snapshot is only consumed after all three facts
    /// (time, AF, AIE) read consistently; a read failure keeps the edge
    /// retryable so the next poll retries. Returns `true` when AppRunner
    /// decided to start ringing (the caller should return to the main
    /// loop which drives the blocking ring screen).
    ///
    /// This is the only alarm entry available while a page owns the main
    /// thread - `main`'s AF fast path does not run then. It must NOT ACK
    /// or rearm the RTC itself; that stays with the state machine via the
    /// dispatch.
    pub fn poll_alarm_snapshot(&mut self) -> bool {
        // A corrupt boot (core_failed) disables AppRunner for every entry
        // - the main loop's AF fast path and any blocking page. Without
        // this guard a default AppState would interpret a real AF as
        // residue and ACK it off.
        if !self.app_runner_enabled {
            return false;
        }
        // Run the *same* `AlarmPoll` orchestration the host harness
        // drives (inkwash-logic::alarm_flow): AF edge -> consistent
        // snapshot -> dispatch -> exit flag. The driver reads the real
        // RTC; the dispatcher forwards through the shared runner. The
        // poll state is moved out of `self` so driver + dispatcher can
        // both borrow `self` while the poll runs, then put back.
        let mut poll = std::mem::take(&mut self.alarm_poll);
        let runner = self.app_runner.clone();
        let pending_renders = self.pending_renders.clone();
        let mut host = CtxAlarmHost {
            ctx: self,
            runner,
            pending_renders,
        };
        let firing = poll.poll(&mut host);
        if let Some(err) = poll.take_error() {
            log::warn!("Blocking page RTC alarm snapshot read failed; stays retryable: {err}");
        }
        self.alarm_poll = poll;
        if firing {
            // Drive the blocking ring/dismiss loop right here so the alarm
            // covers the current page (main's AF fast path is not running).
            // ACK / persist already happened via the dispatch above.
            log::info!("RTC alarm fired over blocking page; entering ring screen");
            if let Err(err) =
                crate::alarms::ring_screen(self.board, self.usb_console, self.ble_control.as_mut())
            {
                log::error!("ring_screen failed: {err}");
            }
            let mut poll = std::mem::take(&mut self.alarm_poll);
            let runner = self.app_runner.clone();
            let pending_renders = self.pending_renders.clone();
            let mut host = CtxAlarmHost {
                ctx: self,
                runner,
                pending_renders,
            };
            poll.ring_dismiss(&mut host);
            self.alarm_poll = poll;
        }
        firing
    }

    /// Services scheduled sync and due-todo reminders. AppRunner owns
    /// the alarm business (boot/home/RTC alarm) so alarm handling lives
    /// outside this path - any AF-driven ringing flows through
    /// `Event::RtcAlarmSnapshotReady` and the state machine. This
    /// function only schedules network + reminder work, mirroring what
    /// it did before AppRunner existed; the legacy alarm branch is
    /// gone so the two paths cannot race for the same AF.
    pub fn poll_runtime(&mut self, now: &DateTime) -> BackgroundOutcome {
        let sync_outcome = if self.poll_scheduled_sync(now) {
            BackgroundOutcome::VisibleChanged
        } else {
            BackgroundOutcome::NoChange
        };
        // A reminder's outcome (AlarmHandled / VisibleChanged / NoChange)
        // merges with the sync outcome by stable priority; a plain
        // reminder dismissal stays VisibleChanged even when no sync ran.
        let reminder_outcome = self.poll_reminders(now);
        sync_outcome.merge(reminder_outcome)
    }

    fn poll_reminders(&mut self, now: &DateTime) -> BackgroundOutcome {
        crate::reminders::poll(self, now)
    }

    /// Runs an urgent/full sync when its wall-clock boundary advances. The
    /// work itself happens on the sync task; this only advances the
    /// boundary cursors and dispatches the command (returning `false`, the
    /// completion side effects - redraw, RTC re-arm - are applied by
    /// [`DeviceContext::poll_wifi_ops`]). Failed attempts are retried at
    /// the next boundary.
    pub fn poll_scheduled_sync(&mut self, now: &DateTime) -> bool {
        let server_configured = self
            .counters
            .device_config()
            .map(|cfg| cfg.is_some())
            .unwrap_or(false);
        let wifi_configured = self
            .counters
            .wifi_creds()
            .map(|creds| creds.is_some())
            .unwrap_or(false);
        if !server_configured || !wifi_configured {
            return false;
        }

        let interval = self.counters.sync_interval_minutes().unwrap_or(60) as u64;
        let unix = now.to_unix();
        let urgent_due = inkwash_logic::scheduler::boundary_advanced(
            unix,
            30,
            self.sync_scheduler.last_urgent_boundary,
        );
        let full_due = inkwash_logic::scheduler::boundary_advanced(
            unix,
            interval * 60,
            self.sync_scheduler.last_full_boundary,
        );
        if !urgent_due && !full_due {
            return false;
        }

        // One op at a time; cursors were advanced when the previous one
        // dispatched, so the next boundary still fires on time.
        if self.pending_wifi_op.is_some() {
            return false;
        }

        if full_due || (self.sync_scheduler.never_synced && urgent_due) {
            self.sync_scheduler.last_full_boundary =
                inkwash_logic::scheduler::boundary_index(unix, interval * 60);
            self.sync_scheduler.last_urgent_boundary =
                inkwash_logic::scheduler::boundary_index(unix, 30);
            log::info!("Aligned sync due (interval {interval} min); dispatching");
            // The state machine owns the start decision (single-flight
            // arbitration); the event only reports that the boundary fired.
            // The executor's StartSync effect performs the real dispatch.
            let runner = self.app_runner.clone();
            self.dispatch_sync_boundary(&runner);
            return false;
        }

        if urgent_due {
            self.sync_scheduler.last_urgent_boundary =
                inkwash_logic::scheduler::boundary_index(unix, 30);
            log::info!("Urgent poll boundary; dispatching");
            match self.sync.poll_urgent() {
                Ok(reply) => {
                    self.pending_wifi_op = Some(PendingWifiOp::UrgentPoll { reply });
                }
                Err(err) => log::warn!("Failed to dispatch urgent poll: {err}"),
            }
        }
        false
    }

    /// Dispatches a full sync to the sync task and records the pending
    /// receipt. Returns `Ok(true)` when dispatched, `Ok(false)` when
    /// another Wi-Fi operation is already in flight (the caller replies
    /// busy / skips).
    pub fn start_sync(&mut self, now: DateTime) -> Result<bool> {
        if self.pending_wifi_op.is_some() {
            return Ok(false);
        }
        let reply = self.sync.sync_now(now)?;
        self.pending_wifi_op = Some(PendingWifiOp::Sync { reply });
        Ok(true)
    }

    /// Dispatches a Wi-Fi verification + save to the sync task.
    pub fn start_set_wifi(&mut self, creds: WifiCreds) -> Result<bool> {
        if self.pending_wifi_op.is_some() {
            return Ok(false);
        }
        let reply = self.sync.set_wifi(creds)?;
        self.pending_wifi_op = Some(PendingWifiOp::SetWifi { reply });
        Ok(true)
    }

    /// Polls the pending Wi-Fi operation's receipt (non-blocking). Called
    /// every main-loop iteration and by blocking screens via
    /// [`DeviceContext::poll_background`]; applies completion side effects
    /// (RTC re-arm, NTP alignment, deferred transport replies) and returns
    /// the completed operation's event for the caller.
    pub fn poll_wifi_ops(&mut self) {
        let Some(op) = self.pending_wifi_op.take() else {
            return;
        };
        match op {
            PendingWifiOp::Sync { reply } => match reply.try_recv() {
                Ok(result) => {
                    // Every completed sync - scheduled or on-demand - is
                    // fed through the state machine, which owns the merged
                    // data apply (Effect::ApplySyncedData) and the transport
                    // reply (only when a SyncNow slot awaits, tracked in
                    // `sm_sync_reply_target`). The RTC/NVS-maintenance
                    // side effects stay on the main loop here.
                    let ok = result.outcome.is_ok();
                    self.feed_sync_completed(&result);
                    self.apply_sync_side_effects(ok, result.ntp_epoch);
                    self.sm_sync_reply_target = None;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::Sync { reply });
                }
                Err(_) => {}
            },
            PendingWifiOp::SetWifi { reply } => match reply.try_recv() {
                Ok(result) => {
                    // Every SetWifi is state-machine-routed (its reply is
                    // delivered through the machine when a slot awaits).
                    self.feed_set_wifi_completed(result.map_err(|e| e.to_string()));
                    self.sm_wifi_reply_target = None;
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::SetWifi { reply });
                }
                Err(_) => {}
            },
            PendingWifiOp::UrgentPoll { reply } => match reply.try_recv() {
                Ok(Ok(true)) => {
                    // The server keeps answering
                    // `urgent: true` until the message is read, and a
                    // successful full sync already fetched it - so further
                    // polls with the flag still set must not re-run the
                    // full sync at every 30 s boundary. Cleared when a
                    // poll reports no urgent content.
                    if self.sync_scheduler.urgent_synced {
                        log::info!("Urgent flag still set and already synced; skipping full sync");
                    } else {
                        log::info!("Urgent message available; dispatching full sync");
                        match self.rtc.read_time() {
                            Ok(now) => {
                                if let Err(err) = self.start_sync(now) {
                                    log::warn!("Failed to dispatch sync after urgent poll: {err}");
                                }
                            }
                            Err(err) => log::warn!("RTC read failed after urgent poll: {err}"),
                        }
                    }
                }
                Ok(Ok(false)) => {
                    self.sync_scheduler.urgent_synced = false;
                }
                Ok(Err(err)) => {
                    log::warn!("Urgent poll failed: {err}");
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::UrgentPoll { reply });
                }
                Err(_) => {}
            },
        }
    }

    /// The RTC/NVS-maintenance half of a completed sync: re-point the
    /// PCF8563 alarm slot at the newly-synced alarm list (the executor's
    /// `ApplySyncedData` already wrote the list to NVS), apply the daily
    /// NTP alignment and clear the fresh-device/urgent flags. The merged
    /// data apply + transport reply are owned by the state machine; this
    /// only does what the machine cannot (I2C RTC work on the main thread).
    fn apply_sync_side_effects(&mut self, ok: bool, ntp_epoch: Option<u64>) {
        if ok {
            // The merged alarm list is in NVS (applied by the machine);
            // re-point the PCF8563's single alarm slot at the new nearest
            // alarm.
            match self.rtc.read_time() {
                Ok(now) => {
                    // Record when this sync ran (main-thread NVS write; the
                    // network task never writes NVS). The auto-sync checker
                    // uses this to know how long since the last sync.
                    if let Err(err) = self.counters.set_last_sync_epoch(now.to_unix()) {
                        log::warn!("Failed to record last-sync time: {err}");
                    }
                    match self.alarm_store.load().and_then(|list| {
                        crate::alarms::program_hardware_alarm_via(self.rtc, &list, &now)
                    }) {
                        Ok(()) => {}
                        Err(err) => {
                            log::warn!("Failed to re-arm hardware alarm after sync: {err}")
                        }
                    }
                }
                Err(err) => log::warn!("RTC read failed after sync (alarm not re-armed): {err}"),
            }
            // Daily NTP alignment: the sync task captured the NTP time
            // while Wi-Fi was up; apply it here (the task never touches
            // the I2C bus).
            if let Some(epoch) = ntp_epoch {
                let tz = self.counters.timezone_offset_minutes().unwrap_or(0);
                let dt = DateTime::from_unix(epoch).shifted_minutes(tz as i32);
                match self.rtc.write_time(&dt) {
                    Ok(()) => {
                        log::info!("NTP alignment applied to RTC");
                        if let Err(err) = self.counters.set_rtc_align_epoch(epoch) {
                            log::warn!("Failed to record RTC alignment time: {err}");
                        }
                    }
                    Err(err) => log::warn!("Failed to write NTP time to RTC: {err}"),
                }
            }
            self.sync_scheduler.never_synced = false;
            // A successful full sync fetched the current server state; if
            // the urgent flag is still set, that message is already here
            // and the 30 s urgent polls must not re-run the full sync
            // until the flag clears.
            self.sync_scheduler.urgent_synced = true;
            log::info!("Full sync completed");
        }
    }

    /// Feeds a completed sync receipt into the state machine as
    /// `Event::SyncCompleted`. The machine applies the merged data
    /// (`Effect::ApplySyncedData`) for every sync - scheduled and
    /// on-demand - and emits a `Reply` only when a SyncNow slot awaits
    /// (tracked in `sm_sync_reply_target`); when present, that reply is
    /// written to the stored transport + correlation id exactly once.
    fn feed_sync_completed(&mut self, result: &sync::SyncResult) {
        let sm_result = match &result.outcome {
            Ok(sync::SyncOutcome::Applied {
                alarms,
                todos,
                inbox,
                inbox_read_acked,
                inbox_truncated,
                etag,
            }) => inkwash_logic::app::SyncResult::Ok {
                data: inkwash_logic::app::SyncedData {
                    alarms: alarms.clone(),
                    todos: todos.clone(),
                    inbox: inbox.clone(),
                    inbox_read_acked: inbox_read_acked.clone(),
                    inbox_truncated: *inbox_truncated,
                    etag: etag.clone(),
                },
            },
            Err(err) => inkwash_logic::app::SyncResult::Failed(err.to_string()),
        };
        let target = self.sm_sync_reply_target.take();
        let runner = self.app_runner.clone();
        let result = self.dispatch_sync_completed(&runner, sm_result);
        if let (Some(reply), Some((channel, id))) = (result, target) {
            write_reply_to_channel(self, channel, &reply, Some(&id));
        }
    }

    /// Feeds a completed SetWifi result into the state machine as
    /// `Event::SetWifiCompleted`, then writes the machine's Reply effect to
    /// the transport that requested the SetWifi (stored in
    /// `sm_wifi_reply_target`).
    fn feed_set_wifi_completed(&mut self, result: Result<(), String>) {
        let Some((channel, id)) = self.sm_wifi_reply_target.take() else {
            return;
        };
        let runner = self.app_runner.clone();
        let reply = self.dispatch_set_wifi_completed(&runner, result);
        if let Some(reply) = reply {
            write_reply_to_channel(self, channel, &reply, Some(&id));
        }
    }

    /// Pushes one `SetWifiCompleted` into the runner, pumps, and returns
    /// the reply the state machine recorded for the wifi operation.
    fn dispatch_set_wifi_completed(
        &mut self,
        runner: &std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
        result: Result<(), String>,
    ) -> Option<Reply> {
        let last_clock = runner.borrow().last_clock();
        let mut reply_for_channel = None;
        {
            let mut executor = crate::app_runner::EffectRunner::new(self, last_clock);
            let mut runtime = runner.borrow_mut();
            runtime.push(inkwash_logic::app::Event::SetWifiCompleted(result));
            if let Err(err) = runtime.pump(&mut executor) {
                log::warn!("SetWifiCompleted dispatch through state machine failed: {err}");
            }
            for (reply_channel, reply) in executor.take_replies() {
                if (reply_channel == Channel::Usb || reply_channel == Channel::Ble)
                    && reply_for_channel.is_none()
                {
                    reply_for_channel = Some(reply);
                }
            }
        }
        for kick in runner.borrow_mut().take_kicks() {
            let mut reg = self.pending_renders.borrow_mut();
            if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
                reg.register(kick);
            } else {
                log::warn!(
                    "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                    kick.effect,
                    kick.operation_id
                );
            }
        }
        reply_for_channel
    }

    /// Pushes one `SyncBoundaryDue` into the runner and pumps. The state
    /// machine decides whether a scheduled sync may start (single-flight +
    /// configured); if it emits StartSync, the executor performs the real
    /// dispatch inside the pump. No reply is expected (scheduled syncs have
    /// no transport slot).
    fn dispatch_sync_boundary(
        &mut self,
        runner: &std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
    ) {
        let last_clock = runner.borrow().last_clock();
        {
            let mut executor = crate::app_runner::EffectRunner::new(self, last_clock);
            let mut runtime = runner.borrow_mut();
            runtime.push(inkwash_logic::app::Event::SyncBoundaryDue);
            if let Err(err) = runtime.pump(&mut executor) {
                log::warn!("SyncBoundaryDue dispatch through state machine failed: {err}");
            }
        }
        for kick in runner.borrow_mut().take_kicks() {
            let mut reg = self.pending_renders.borrow_mut();
            if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
                reg.register(kick);
            } else {
                log::warn!(
                    "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                    kick.effect,
                    kick.operation_id
                );
            }
        }
    }

    /// Pushes one `SyncCompleted` into the runner, pumps, and returns the
    /// reply the state machine recorded for the sync (the Ok/Error for the
    /// pending SyncNow slot). The caller writes it to the stored target.
    fn dispatch_sync_completed(
        &mut self,
        runner: &std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
        result: inkwash_logic::app::SyncResult,
    ) -> Option<Reply> {
        let last_clock = runner.borrow().last_clock();
        let mut reply_for_channel = None;
        {
            let mut executor = crate::app_runner::EffectRunner::new(self, last_clock);
            let mut runtime = runner.borrow_mut();
            runtime.push(inkwash_logic::app::Event::SyncCompleted(result));
            if let Err(err) = runtime.pump(&mut executor) {
                log::warn!("SyncCompleted dispatch through state machine failed: {err}");
            }
            for (reply_channel, reply) in executor.take_replies() {
                if (reply_channel == Channel::Usb || reply_channel == Channel::Ble)
                    && reply_for_channel.is_none()
                {
                    reply_for_channel = Some(reply);
                }
            }
        }
        for kick in runner.borrow_mut().take_kicks() {
            let mut reg = self.pending_renders.borrow_mut();
            if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
                reg.register(kick);
            } else {
                log::warn!(
                    "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                    kick.effect,
                    kick.operation_id
                );
            }
        }
        reply_for_channel
    }
}

/// Writes one control reply to the given transport, echoing the
/// correlation id. Mirrors the legacy `deliver_deferred_reply` write but
/// takes an explicit channel (used by the state-machine-routed paths).
fn write_reply_to_channel(
    ctx: &DeviceContext<'_>,
    channel: Channel,
    reply: &Reply,
    id: Option<&str>,
) {
    match channel {
        Channel::Usb => crate::usb_console::write_reply(reply, id),
        Channel::Ble => {
            if let Some(ble) = ctx.ble_control.as_ref() {
                ble.write_reply(reply, id);
            }
        }
    }
}

/// Single `AlarmHost` adapter over the RTC executor + shared runner.
/// Borrows `&mut DeviceContext` once so `AlarmPoll::poll` runs the exact
/// orchestration the host harness drives; RTC facts come from the executor
/// client (the sole owner), never from a direct `board.rtc` access.
struct CtxAlarmHost<'a, 'ctx> {
    ctx: &'a mut DeviceContext<'ctx>,
    runner: std::rc::Rc<std::cell::RefCell<crate::app_runner::AppRunner>>,
    pending_renders: std::rc::Rc<std::cell::RefCell<inkwash_logic::epd_registry::RenderRegistry>>,
}

impl inkwash_logic::alarm_flow::AlarmHost for CtxAlarmHost<'_, '_> {
    fn alarm_flag(&mut self) -> Result<bool, String> {
        self.ctx
            .rtc
            .alarm_status()
            .map(|status| status.alarm_flag)
            .map_err(|e| format!("{e:#}"))
    }
    fn read_snapshot(&mut self) -> Result<inkwash_logic::app::RtcAlarmSnapshot, String> {
        // The executor returns a consistent (time, AF, AIE) snapshot and
        // honors its own duplicate latch while AF stays asserted.
        self.ctx.rtc.snapshot().map_err(|e| format!("{e:#}"))
    }
    fn snapshot_ready(&mut self, snapshot: inkwash_logic::app::RtcAlarmSnapshot) -> bool {
        let last_clock = self.runner.borrow().last_clock();
        let mut executor = crate::app_runner::EffectRunner::new(self.ctx, last_clock);
        let mut runtime = self.runner.borrow_mut();
        runtime.push(inkwash_logic::app::Event::RtcAlarmSnapshotReady(snapshot));
        let ok = runtime.pump(&mut executor);
        drop(runtime);
        ok.is_ok()
            && matches!(
                self.runner.borrow().state().screen,
                inkwash_logic::app::Screen::AlarmRinging
            )
    }
    fn dismiss(&mut self) {
        let last_clock = self.runner.borrow().last_clock();
        let mut executor = crate::app_runner::EffectRunner::new(self.ctx, last_clock);
        let mut runtime = self.runner.borrow_mut();
        runtime.push(inkwash_logic::app::Event::Button(
            inkwash_logic::button_event::ButtonEvent::Pressed(
                inkwash_logic::button_event::ButtonId::Enter,
            ),
        ));
        let _ = runtime.pump(&mut executor);
    }
    fn drain_kicks(&mut self) {
        for kick in self.runner.borrow_mut().take_kicks() {
            let mut reg = self.pending_renders.borrow_mut();
            if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
                reg.register(kick);
            } else {
                log::warn!(
                    "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                    kick.effect,
                    kick.operation_id
                );
            }
        }
    }
}
