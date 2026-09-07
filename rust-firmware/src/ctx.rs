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
    /// True once a successful full sync has fetched the server state in
    /// response to an urgent poll. The server keeps answering `urgent:
    /// true` until the message is read on-device, so after a full sync
    /// that fetched the urgent message, the device checks its local
    /// unread_urgent set: if non-empty, the same message was already
    /// fetched — skip further syncs. If empty, a *new* urgent message has
    /// arrived since the last sync — clear the flag and sync again.
    ///
    /// This fixes P1-4: the old `urgent_synced` boolean was not bound to
    /// any inbox version, so a newly-arrived high-priority message was
    /// silently skipped until the next scheduled full sync interval.
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

    /// Returns true if the device already fetched the current urgent message
    /// and a further urgent-triggered full sync would be redundant. Checks
    /// the local unread_urgent set: if non-empty, the last full sync already
    /// fetched the message — the server is just re-reporting `urgent: true`
    /// until the user reads it.
    fn already_synced_urgent(&self, inbox_store: &crate::inbox::InboxStore) -> bool {
        self.urgent_synced
            && matches!(inbox_store.unread_urgent(), Ok(items) if !items.is_empty())
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
    pub ble_control: &'a mut BleControl,
    /// Handle to the audio task (the sole owner of the ES8311 codec). `None`
    /// when the codec failed to initialise at boot (audio is a degraded-run
    /// category, never a boot blocker). Every tone - alarm ring, reminder -
    /// goes through this handle; nobody touches the codec directly.
    pub audio_task: Option<&'a crate::audio_task::AudioTask>,
    pub sync_scheduler: SyncScheduler,
    /// One long-running Wi-Fi operation at a time (the sync task
    /// serializes them anyway); `None` when idle. The receipt is polled by
    /// [`DeviceContext::poll_wifi_ops`].
    pub pending_wifi_op: Option<PendingWifiOp>,
    /// Session currently arbitrating the shared radio.  The ID is carried
    /// through every completion so a stale worker result cannot resume Wi-Fi
    /// for a newer pairing session.
    pub ble_session_id: Option<u64>,
    pub ble_wifi_suspended: bool,
    pub ble_set_wifi_after_resume: Option<WifiCreds>,
    pub ble_start_failure: Option<(u64, String)>,
    pub ble_start_cancelled: bool,
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
    /// Dispatches one non-command event (reminder facts, etc.) into the
    /// shared state-machine runner and routes any render kicks into the
    /// shared EPD registry, so the SM's render effects reach the panel the
    /// same way every other dispatch does. Non-blocking: the pump only runs
    /// the pure update + synchronous effects.
    pub fn dispatch_event(&mut self, event: inkwash_logic::app::Event) -> anyhow::Result<()> {
        let runner = self.app_runner.clone();
        let last_clock = runner.borrow().last_clock();
        {
            let mut executor = crate::app_runner::EffectRunner::new(self, last_clock);
            let mut runtime = runner.borrow_mut();
            runtime.push(event);
            runtime
                .pump(&mut executor)
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        for kick in runner.borrow_mut().take_kicks() {
            let mut reg = self.pending_renders.borrow_mut();
            if kick.is_render() {
                reg.register(kick);
            } else {
                log::warn!(
                    "AppRunner async kick {:?} (op {:?}) not wired; dropped",
                    kick.effect,
                    kick.operation_id
                );
            }
        }
        Ok(())
    }

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
        if let Command::SetRtc { epoch_secs } = cmd.clone() {
            let reply = if self.pending_wifi_op.is_some() || self.ble_wifi_suspended {
                Reply::Busy
            } else {
                self.set_rtc_from_epoch(epoch_secs)
            };
            if !matches!(reply, Reply::Busy) {
                if let Some(id_value) = id.as_deref() {
                    self.last_command = Some((id_value.to_string(), cmd, reply.clone()));
                }
            }
            crate::usb_console::write_reply(&reply, id.as_deref());
            let changed = matches!(reply, Reply::Ok);
            return (changes_visible_state && changed, true);
        }
        if matches!(cmd, Command::SyncNow) && self.pending_wifi_op.is_some() {
            crate::usb_console::write_reply(&Reply::Busy, id.as_deref());
            return (false, true);
        }
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
        // Firing is now a pure state-machine affair: the SM entered
        // Screen::AlarmRinging, the executor started the alarm tone through
        // the audio task (non-blocking) and rendered the ring frame; ENTER
        // and the ring-deadline Tick dismiss through the SM's shared
        // `dismiss_ringing`. Nothing blocks here - the unified loop keeps
        // serving Button/Tick/USB/BLE/EPD events while the alarm rings.
        firing
    }

    /// Services scheduled sync and due-todo reminders. AppRunner owns
    /// the alarm business (boot/home/RTC alarm) so alarm handling lives
    /// outside this path - any AF-driven ringing flows through
    /// `Event::RtcAlarmSnapshotReady` and the state machine. This
    /// function only schedules network + reminder work, mirroring what
    /// it did before AppRunner existed; the legacy alarm branch is
    /// gone so the two paths cannot race for the same AF.
    pub fn poll_runtime(&mut self, now: &DateTime) {
        // Stage 5/6: reminders are now a state-machine overlay - the fact
        // layer dispatches ReminderDue (Screen::Reminder + its render), so
        // nothing visible happens here. This only advances the sync
        // scheduler (boundary dispatch) and raises reminder facts; the
        // SM's own render effects are the only refreshes.
        self.poll_reminders(now);
        self.poll_scheduled_sync(now);
    }

    fn poll_reminders(&mut self, now: &DateTime) {
        crate::reminders::poll(self, now);
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
        if self.pending_wifi_op.is_some()
            || self.ble_wifi_suspended
            || self.ble_session_id.is_some()
        {
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
        if self.pending_wifi_op.is_some() || self.ble_wifi_suspended {
            return Ok(false);
        }
        let reply = self.sync.sync_now(now)?;
        self.pending_wifi_op = Some(PendingWifiOp::Sync { reply });
        Ok(true)
    }

    /// Sets the PCF8563 from a host-provided Unix timestamp. This stays on
    /// the firmware main loop because the RTC executor is the sole I2C owner;
    /// the host timestamp path is useful when NTP UDP is unavailable.
    pub fn set_rtc_from_epoch(&mut self, epoch_secs: u64) -> Reply {
        const MIN_EPOCH: u64 = 946_684_800; // 2000-01-01
        const MAX_EPOCH: u64 = 4_102_444_800; // 2100-01-01
        if !(MIN_EPOCH..MAX_EPOCH).contains(&epoch_secs) {
            return Reply::Error {
                message: "RTC timestamp must be between 2000-01-01 and 2100-01-01".into(),
            };
        }
        let timezone_offset = self.counters.timezone_offset_minutes().unwrap_or(0);
        let local_time = DateTime::from_unix(epoch_secs).shifted_minutes(timezone_offset as i32);
        match self.rtc.write_time(&local_time) {
            Ok(()) => {
                if let Err(err) = self.counters.clear_rtc_align_epoch() {
                    log::warn!("Failed to clear RTC alignment marker after host set: {err}");
                }
                log::info!(
                    "Host RTC sync applied: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    local_time.year,
                    local_time.month,
                    local_time.day,
                    local_time.hour,
                    local_time.minute,
                    local_time.second
                );
                if let Err(err) = self.dispatch_event(inkwash_logic::app::Event::Tick(local_time)) {
                    log::warn!("Failed to refresh clock fact after host RTC sync: {err}");
                }
                // The wall clock moved — the PCF8563 hardware alarm slot may
                // now point at the wrong instant. Re-derive it from the new
                // RTC time so a host-set timestamp never leaves the hardware
                // alarm lagging (P1-5 fix).
                match self.alarm_store.load().and_then(|list| {
                    crate::alarms::program_hardware_alarm_via(self.rtc, &list, &local_time)
                }) {
                    Ok(()) => {}
                    Err(err) => {
                        log::warn!("Failed to re-arm hardware alarm after host RTC sync: {err}")
                    }
                }
                Reply::Ok
            }
            Err(err) => Reply::Error {
                message: format!("failed to write RTC: {err}"),
            },
        }
    }

    /// Dispatches a Wi-Fi verification + save to the sync task.
    pub fn start_set_wifi(&mut self, creds: WifiCreds) -> Result<bool> {
        if self.pending_wifi_op.is_some() || self.ble_wifi_suspended {
            return Ok(false);
        }
        let reply = self.sync.set_wifi(creds)?;
        self.pending_wifi_op = Some(PendingWifiOp::SetWifi { reply });
        Ok(true)
    }

    /// Accepts a BLE-sent SetWifi while the BLE pairing screen owns the
    /// radio. The BLE transport gets its ack before StopBlePairing is queued;
    /// once BLE is torn down and Wi-Fi is restored, the saved credentials are
    /// verified by the sync task in the background.
    pub fn begin_ble_set_wifi_handoff(&mut self, creds: WifiCreds) -> bool {
        if self.ble_session_id.is_none()
            || !self.ble_wifi_suspended
            || self.pending_wifi_op.is_some()
            || self.ble_set_wifi_after_resume.is_some()
        {
            return false;
        }
        log::info!(
            "BLE SetWifi accepted for '{}'; stopping BLE before Wi-Fi verification",
            creds.ssid
        );
        self.ble_set_wifi_after_resume = Some(creds);
        true
    }

    fn start_ble_set_wifi_after_resume(&mut self, creds: WifiCreds) {
        let ssid = creds.ssid.clone();
        let has_password = !creds.password.is_empty();
        match self.sync.set_wifi(creds) {
            Ok(reply) => {
                log::info!("BLE SetWifi handoff: verifying saved credentials for '{ssid}'");
                self.pending_wifi_op = Some(PendingWifiOp::PostBleSetWifi {
                    reply,
                    ssid,
                    has_password,
                });
            }
            Err(err) => {
                log::error!("BLE SetWifi handoff: failed to queue Wi-Fi verification: {err:#}");
            }
        }
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
            Err(err) => log::error!("Failed to queue Wi-Fi resume after BLE: {err:#}"),
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
                    self.apply_sync_side_effects(&result);
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
            PendingWifiOp::PostBleSetWifi {
                reply,
                ssid,
                has_password,
            } => match reply.try_recv() {
                Ok(Ok(())) => {
                    log::info!("BLE SetWifi handoff: Wi-Fi credentials verified and saved");
                    if let Err(err) =
                        self.dispatch_event(inkwash_logic::app::Event::WifiConfigApplied(
                            inkwash_logic::app::WifiConfigApplied { ssid, has_password },
                        ))
                    {
                        log::warn!("Wi-Fi config fact dispatch failed after BLE handoff: {err}");
                    }
                }
                Ok(Err(err)) => {
                    log::warn!("BLE SetWifi handoff: Wi-Fi verification failed: {err}");
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::PostBleSetWifi {
                        reply,
                        ssid,
                        has_password,
                    });
                }
                Err(_) => log::warn!("BLE SetWifi handoff: Wi-Fi verification task disconnected"),
            },
            PendingWifiOp::UrgentPoll { reply } => match reply.try_recv() {
                Ok(Ok(true)) => {
                    // The server keeps answering `urgent: true` until the
                    // message is read on-device. After a full sync that
                    // fetched the message, the local unread_urgent set is
                    // non-empty — so this is the same message already
                    // fetched; skip the redundant full sync. If the local set
                    // is empty, a *new* urgent message arrived since the last
                    // sync, so fall through and sync again. (P1-4 fix:
                    // replaces the old `urgent_synced` boolean which was not
                    // bound to any inbox version.)
                    if self.sync_scheduler.already_synced_urgent(&self.inbox_store) {
                        log::info!("Urgent flag already fetched and unread locally; skipping full sync");
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
                        self.start_ble_set_wifi_after_resume(creds);
                    }
                }
                Ok(Err(err)) => {
                    self.ble_set_wifi_after_resume = None;
                    log::error!("Wi-Fi resume after BLE failed: {err:#}");
                }
                Err(TryRecvError::Empty) => {
                    self.pending_wifi_op = Some(PendingWifiOp::ResumeAfterBle { reply });
                }
                Err(_) => log::error!("Wi-Fi resume task disconnected after BLE"),
            },
        }
    }

    /// The RTC/NVS-maintenance half of a completed sync: re-point the
    /// PCF8563 alarm slot at the newly-synced alarm list (the executor's
    /// `ApplySyncedData` already wrote the list to NVS), apply the daily
    /// NTP alignment, and clear the fresh-device/urgent-gate flags. The
    /// merged data apply + transport reply are owned by the state machine;
    /// this only does what the machine cannot (I2C RTC work on the main
    /// thread).
    fn apply_sync_side_effects(&mut self, result: &sync::SyncResult) {
        let ok = result.outcome.is_ok();
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
            // Daily NTP alignment: the sync task captured the NTP time
            // while Wi-Fi was up; apply it here (the task never touches
            // the I2C bus).
            if let Some(epoch) = result.ntp_epoch {
                let tz = self.counters.timezone_offset_minutes().unwrap_or(0);
                let dt = DateTime::from_unix(epoch).shifted_minutes(tz as i32);
                match self.rtc.write_time(&dt) {
                    Ok(()) => {
                        log::info!("NTP alignment applied to RTC");
                        if let Err(err) = self.counters.set_rtc_align_epoch(epoch) {
                            log::warn!("Failed to record RTC alignment time: {err}");
                        }
                        // NTP just shifted the wall clock — re-point the PCF8563
                        // hardware alarm at the new instant. The alarm above
                        // was computed from the pre-NTP RTC read, so without
                        // this re-arm the slot lags the true time. (P1-6 fix.)
                        match self.alarm_store.load().and_then(|list| {
                            crate::alarms::program_hardware_alarm_via(self.rtc, &list, &dt)
                        }) {
                            Ok(()) => {}
                            Err(err) => {
                                log::warn!("Failed to re-arm hardware alarm after NTP alignment: {err}")
                            }
                        }
                    }
                    Err(err) => log::warn!("Failed to write NTP time to RTC: {err}"),
                }
            }
            self.sync_scheduler.never_synced = false;
            // A successful full sync fetched the current server state; if the
            // urgent flag is set, check whether we already have the message
            // locally — the server keeps answering `urgent: true` until the
            // device marks it read. If the local unread_urgent set is
            // non-empty, we already have it; if empty, a new urgent message
            // has arrived since this sync (detected on the next poll).
            // Fixes P1-4: the old behavior set the flag unconditionally,
            // silently skipping newly-arrived urgent messages.
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
                uploaded_alarm_ids,
                uploaded_todo_ids,
            }) => inkwash_logic::app::SyncResult::Ok {
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
            } else if matches!(
                kick.effect,
                inkwash_logic::app::Effect::StartBlePairing(_)
                    | inkwash_logic::app::Effect::StopBlePairing
            ) {
                // BLE worker results are delivered through its dedicated
                // result channel and correlated by session id, not by an
                // AppRunner async kick.
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
            ctx.ble_control.write_reply(reply, id);
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
