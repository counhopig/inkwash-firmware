//! Application state machine: the single owner of business state for the
//! firmware (see `docs/firmware-architecture.md`).
//!
//! This is migration step 1 of that plan: the `AppState` / `Event` /
//! `Effect` / `EffectBatch` skeleton and a single `update` entry point. The
//! architecture requires core state transitions to be unit-testable without
//! ESP-IDF (acceptance criterion #9), so every type here is pure data and
//! `update` touches no hardware - it mutates state and returns effects that
//! a separate side-effect execution layer (in the firmware crate) performs.
//!
//! Scope: the shapes and the transitions already fully specified in the
//! design (`Boot`, `Tick`, the RTC alarm runtime state machine,
//! effect-completion plumbing, and the transport busy/reply protocol).
//! Later steps wire the remaining pages, sync scheduling, display refresh
//! and BLE pairing through this same `update`.
//!
//! `update` MUST NOT call hardware, NVS or IO. A transition that cannot
//! decide on pure data alone leaves the existing state intact and lets the
//! executor report the outcome back as a new `Event`.
//!
//! Confirmable operations (ACK, alarm persistence, RTC programming) each
//! carry their own `OperationId` in the batch *and* in the matching
//! `CommitState::InFlight`; a completion or failure event only mutates the
//! commit state whose `operation_id` matches exactly, so a stale/duplicate
//! completion can never corrupt a newer state.

use crate::alarm_regs::AlarmRegs;
use crate::alarm_schedule::{next_due, Repeat, StoredAlarm};
use crate::button_event::ButtonEvent;
use crate::datetime::DateTime;
use crate::device_config::DeviceConfig;
use crate::inbox_item::InboxItem;
use crate::protocol::{Channel, ControlReply, ControlRequest, Reply};
use crate::todo::Todo;
use crate::wake_cause::WakeCause;

/// Monotonic version number of the *visible* state. Only incremented when a
/// new `ViewModel` is generated, so out-of-date `Render` completions can be
/// discarded without overwriting the current page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RenderGeneration(pub u64);

impl RenderGeneration {
    pub fn next(self) -> Self {
        RenderGeneration(self.0.wrapping_add(1))
    }
}

/// Identifies one in-flight business operation (sync, BLE, persistence, RTC
/// commit, protocol reply). A completion event matches against it to find
/// the corresponding in-flight state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct OperationId(pub u64);

/// Identifies one `EffectBatch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EffectBatchId(pub u64);

/// Identifies one `Effect` within a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EffectId(pub u64);

/// A page is state, not a function owning a blocking loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Navigation { selected: usize },
    Settings { selected: usize },
    Calendar(CalendarState),
    AlarmList { selected: usize },
    AlarmEdit(AlarmEditState),
    TodoList { selected: usize },
    Inbox { selected: usize },
    AlarmRinging,
    Reminder(ReminderState),
    BlePairing(BlePairingState),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarState {
    pub year: u16,
    pub month: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmEditState {
    pub editing: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderState {
    pub item: ReminderItem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReminderItem {
    Todo { id: u8, text: String },
    Inbox { seq: u64, title: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingState {
    pub phase: BlePairingPhase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlePairingPhase {
    Waiting,
    Pairing,
    Success,
    Failure(String),
}

/// A pending transport reply slot. Each transport (USB/BLE) has at most one
/// in-flight command at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReply {
    pub operation_id: OperationId,
    pub request: ControlRequest,
    pub channel: Channel,
    /// The final reply, filled once the command's confirmable effects have
    /// all completed (`None` while the command is in flight).
    pub reply: Option<ControlReply>,
    /// The confirmable operation ids the in-flight command still awaits.
    /// Each matching `Persisted`/`RtcProgrammed`/`AckDone` completion
    /// removes its id; the final reply fires when the set is empty. An
    /// immediate/busy reply never occupies a slot.
    pub awaiting_ops: Vec<OperationId>,
}

impl PendingReply {
    /// A new in-flight command awaiting the given confirmable completions.
    fn awaiting(
        operation_id: OperationId,
        request: ControlRequest,
        channel: Channel,
        awaiting_ops: Vec<OperationId>,
    ) -> Self {
        Self {
            operation_id,
            request,
            channel,
            reply: None,
            awaiting_ops,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ClockState {
    pub now: Option<DateTime>,
}

/// The stored alarm list. NVS is the authoritative source; the PCF8563
/// register is rebuildable derived state.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AlarmState {
    pub alarms: Vec<StoredAlarm>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TodoState {
    pub todos: Vec<Todo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct InboxState {
    pub items: Vec<InboxItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConnectivityState {
    pub wifi_configured: bool,
    pub server_configured: bool,
    pub wifi_connected: bool,
}

/// Device configuration facts the state machine can act on. Secrets (the
/// server auth token, the Wi-Fi password) never enter `AppState`: the
/// firmware persists them in NVS and only reports *presence* flags and the
/// non-secret SSID/server URL here.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConfigState {
    /// Wi-Fi SSID (non-secret) + whether a password is set.
    pub wifi_ssid: Option<String>,
    pub wifi_has_password: bool,
    /// Server URL (non-secret) + whether an auth token is set.
    pub server_url: Option<String>,
    pub server_has_token: bool,
    /// UTC offset in minutes (non-secret).
    pub timezone_offset_minutes: i16,
}

impl ConfigState {
    pub fn wifi_configured(&self) -> bool {
        self.wifi_ssid.is_some()
    }
    pub fn server_configured(&self) -> bool {
        self.server_url.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PowerState {
    /// Next idle-deadline target (unix seconds) computed by the business
    /// layer; the event-collection layer only compares current time against
    /// it, never interprets its meaning.
    pub next_idle_deadline: Option<u64>,
}

/// Sync state: at most one network operation at a time.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SyncState {
    #[default]
    Idle,
    Running {
        request_id: OperationId,
    },
    Applying {
        request_id: OperationId,
    },
}

/// How a batch's effects behave on failure. `AbortBatch` stops effects that
/// have not yet run but never rolls back writes that already succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailurePolicy {
    Continue,
    AbortBatch,
}

/// An ordered group of effects, executed sequentially by the side-effect
/// execution task. Only render batches carry a `render_generation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectBatch {
    pub id: EffectBatchId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effects: Vec<Effect>,
    pub failure_policy: FailurePolicy,
}

/// An effect is a request for the side-effect execution layer. It is the
/// ONLY way any layer may touch a driver/repository: new capabilities must
/// be modelled as an `Effect` first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    PersistAlarms(Vec<StoredAlarm>),
    PersistTodos(Vec<Todo>),
    PersistInbox(Vec<InboxItem>),
    PersistConfig(DeviceConfig),
    PersistSyncMetadata(SyncMetadata),
    /// Persist the UTC-offset minutes (SetTimezone's NVS write).
    PersistTimezone(i16),
    /// Write the RTC clock to an absolute time (SetTimezone's hardware
    /// write; confirmable through the RTC executor).
    WriteRtcTime(DateTime),
    ProgramRtcAlarm(AlarmRegs),
    DisableRtcAlarm,
    AcknowledgeRtcAlarm,
    StartSync(SyncRequest),
    StartTone,
    StopTone,
    Render(RenderRequest),
    /// Send a control reply to exactly one transport. `channel` is which
    /// transport receives it: USB and BLE each have an independent pending
    /// reply slot, and a reply must never be written to the other channel
    /// (two simultaneous commands - one per transport - would otherwise get
    /// each other's response).
    Reply {
        channel: Channel,
        reply: ControlReply,
    },
    StartBlePairing(BlePairingRequest),
    StopBlePairing,
    EnterLightSleep(LightSleepPlan),
    EnterDeepSleep(WakeupPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub generation: RenderGeneration,
    pub intent: RenderIntent,
}

/// Refresh mode requested by a state transition. The render intent is kept
/// in the host-testable request so the firmware executor can preserve cheap
/// clock updates while making alarm-dismiss transitions atomic on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RenderIntent {
    #[default]
    Partial,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRequest {
    pub now: DateTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncMetadata {
    pub etag: Option<String>,
    pub last_sync_epoch: Option<u64>,
    pub rtc_align_epoch: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingRequest {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeupPlan {
    pub maintenance: Option<std::time::Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightSleepPlan {
    pub wake_after_ms: u64,
}

/// Which repository a persistence effect targeted, so a `Persisted`
/// completion can be attributed precisely (the alarm flow only persists
/// alarms, but the same model is reused by sync/command flows in later
/// steps).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistTarget {
    Alarms,
    Todos,
    Inbox,
    Config,
    SyncMetadata,
    /// SetTimezone's timezone-offset NVS write.
    Timezone,
}

/// Outcome of a completed effect, fed back to `update` via
/// `Event::EffectCompleted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutput {
    RenderDone,
    SyncDone(SyncResult),
    Persisted(PersistTarget),
    RtcProgrammed,
    /// A `WriteRtcTime` (SetTimezone clock write) confirmed.
    RtcTimeWritten,
    AckDone,
    ToneDone,
    BlePairingDone,
    LightSleepEntered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncResult {
    Ok {
        alarm_count: usize,
        todo_count: usize,
        inbox_count: usize,
        inbox_truncated: bool,
        etag: Option<String>,
    },
    Failed(String),
}

/// Failure of an effect, fed back via `Event::EffectFailed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectError {
    Render(String),
    Persist(String),
    Rtc(String),
    Sync(String),
    Tone(String),
    Ble(String),
    Sleep(String),
    Ack(String),
}

/// A side-effect completion report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectFailure {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub error: EffectError,
}

/// A consistent RTC read (time, AF, AIE) taken after a GPIO5 edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcAlarmSnapshot {
    pub now: DateTime,
    pub alarm_flag: bool,
    pub alarm_interrupt_enabled: bool,
}

/// Facts gathered before boot. Initialization must NOT clear AF/AIE/alarm
/// register - the state machine decides that out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootSnapshot {
    pub wake_cause: WakeCause,
    pub now: Option<DateTime>,
    pub rtc_alarm_flag: bool,
    pub rtc_alarm_interrupt_enabled: bool,
    pub alarms: Vec<StoredAlarm>,
    pub todos: Vec<Todo>,
    pub inbox: Vec<InboxItem>,
    pub config: DeviceConfig,
    /// Status-visible device facts (wi-fi presence, timezone) gathered at
    /// boot. Secrets are never included.
    pub status: DeviceStatus,
}

/// Status-visible device configuration facts gathered at boot for
/// `GetStatus`. Mirrors `ConfigState`'s fields; carried on `BootSnapshot`
/// so the state machine's `ConfigState` is populated from a fact, never
/// from a driver read.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DeviceStatus {
    pub wifi_ssid: Option<String>,
    pub wifi_has_password: bool,
    pub timezone_offset_minutes: i16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingResult {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingFailure {
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayResult {
    pub generation: RenderGeneration,
    pub ok: bool,
    pub recovered: bool,
}

/// All events flow through one entry. Every variant is a fact collected by
/// the event sources; `update` alone decides what it means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Boot(BootSnapshot),
    Tick(DateTime),
    Button(ButtonEvent),
    RtcAlarmSnapshotReady(RtcAlarmSnapshot),
    UsbCommand(ControlRequest),
    BleCommand(ControlRequest),
    BlePairingStarted,
    BlePairingSucceeded(BlePairingResult),
    BlePairingFailed(BlePairingFailure),
    BleDisconnected,
    SyncCompleted(SyncResult),
    DisplayCompleted(DisplayResult),
    EffectCompleted(EffectCompletion),
    EffectFailed(EffectFailure),
    IdleDeadlineReached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectCompletion {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub output: EffectOutput,
}

/// RTC alarm runtime stage. Only `Firing` owns the currently-ringing
/// `alarm_id`; `Screen::AlarmRinging` never carries it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum AlarmRuntimeState {
    #[default]
    Disarmed,
    Armed {
        alarm_id: u8,
    },
    Firing {
        alarm_id: u8,
        fired_minute: u64,
        ack: CommitState,
        persistence: CommitState,
    },
    WaitingForRearm {
        fired_alarm_id: u8,
        fired_minute: u64,
        ack: CommitState,
        persistence: CommitState,
        minute_advanced: bool,
    },
    Degraded {
        desired_alarm_id: Option<u8>,
        error: AlarmHardwareError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlarmHardwareError {
    RtcWriteFailed,
    RtcReadFailed,
}

/// An in-flight RTC programming operation and what it should leave the
/// hardware in once it succeeds: armed to a specific alarm, or disabled.
/// The target alarm id is what lets `RtcProgrammed` enter `Armed { alarm_id }`
/// (an `Armed` state is how the app expresses which alarm the RTC register
/// currently holds - the precondition for deep-sleep verification).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingRtc {
    Program {
        operation_id: OperationId,
        alarm_id: u8,
    },
    Disable {
        operation_id: OperationId,
    },
}

impl PendingRtc {
    fn operation_id(&self) -> OperationId {
        match self {
            PendingRtc::Program { operation_id, .. } | PendingRtc::Disable { operation_id } => {
                *operation_id
            }
        }
    }
}

/// A failed confirmable operation scheduled for a retry, with a single
/// backoff window. The retry re-issues the same effect with a fresh
/// `OperationId` once `due_minute` has passed. The design says failures
/// must be retried, not silently left failed; NVS stays authoritative so a
/// retried RTC program never rolls back a committed alarm list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPending {
    pub action: RetryAction,
    pub due_minute: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryAction {
    Ack,
    PersistAlarms,
    ProgramRtc,
    DisableRtc,
}

/// Progress of one confirmable commit (ACK / persistence / RTC program).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum CommitState {
    #[default]
    Pending,
    InFlight {
        operation_id: OperationId,
    },
    Succeeded,
    Failed {
        error: EffectError,
    },
}

impl CommitState {
    /// True when this commit is in flight and carrying `op`. This is the
    /// exact-match gate used by completion/failure transitions so a stale
    /// event cannot advance a newer state.
    fn inflight_is(&self, op: OperationId) -> bool {
        matches!(self, CommitState::InFlight { operation_id } if *operation_id == op)
    }
}

/// The whole business state, owned exclusively by `App`. Hardware drivers,
/// repositories and executors never hold it. Private trailing fields are
/// bookkeeping (id counters, the pre-ring screen, the in-flight RTC program)
/// rather than business state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppState {
    pub render_generation: RenderGeneration,
    pub screen: Screen,
    pub clock: ClockState,
    pub alarms: AlarmState,
    pub alarm_runtime: AlarmRuntimeState,
    pub todos: TodoState,
    pub inbox: InboxState,
    pub sync: SyncState,
    pub connectivity: ConnectivityState,
    /// Device configuration (server endpoint + Wi-Fi presence flags). Holds
    /// only what the UI/status layer needs; the secrets (auth token,
    /// Wi-Fi password) are never stored in `AppState` - the firmware keeps
    /// them in NVS and only reports booleans.
    pub config: ConfigState,
    pub power: PowerState,
    pub pending_usb_reply: Option<PendingReply>,
    pub pending_ble_reply: Option<PendingReply>,
    op_counter: u64,
    batch_counter: u64,
    screen_before_ring: Screen,
    pending_rtc: Option<PendingRtc>,
    /// An in-flight ACK that clears a *residue* AF (no valid alarm fired):
    /// the RTC register must not be reprogrammed until this ACK succeeds,
    /// or a stale AF could keep RTC_INT low after AIE is re-enabled.
    pending_residue_ack: Option<OperationId>,
    /// Wall-clock time at which the pending residue ACK was issued, used to
    /// reprogram the RTC register on the matching `AckDone`. Set when
    /// `pending_residue_ack` is set; cleared when the pending ACK clears.
    pending_residue_time: Option<DateTime>,
    retries: Vec<RetryPending>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            render_generation: RenderGeneration(0),
            screen: Screen::Home,
            clock: ClockState::default(),
            alarms: AlarmState::default(),
            alarm_runtime: AlarmRuntimeState::Disarmed,
            todos: TodoState::default(),
            inbox: InboxState::default(),
            sync: SyncState::Idle,
            connectivity: ConnectivityState::default(),
            config: ConfigState::default(),
            power: PowerState::default(),
            pending_usb_reply: None,
            pending_ble_reply: None,
            pending_residue_ack: None,
            pending_residue_time: None,
            retries: Vec::new(),
            op_counter: 0,
            batch_counter: 0,
            screen_before_ring: Screen::Home,
            pending_rtc: None,
        }
    }
}

/// Backoff window (in minutes) before a failed confirmable operation is
/// retried after it is scheduled.
const RETRY_BACKOFF_MINUTES: u64 = 1;

impl AppState {
    fn next_operation_id(&mut self) -> OperationId {
        self.op_counter = self.op_counter.wrapping_add(1);
        OperationId(self.op_counter)
    }

    fn next_batch_id(&mut self) -> EffectBatchId {
        self.batch_counter = self.batch_counter.wrapping_add(1);
        EffectBatchId(self.batch_counter)
    }

    /// Schedules a retry for `action` after a one-minute backoff window from
    /// `now_minute`. A later Tick passes the due window and re-issues the
    /// effect with a fresh `OperationId` (see [`maybe_retry`]).
    ///
    /// Retries are keyed by action so concurrent failures (e.g. ACK and
    /// persistence both failing) never overwrite each other: a new failure
    /// of the *same* action refreshes its deadline, but a different action
    /// is appended / kept independently.
    fn schedule_retry(&mut self, action: RetryAction, now_minute: u64) {
        let due_minute = now_minute + RETRY_BACKOFF_MINUTES;
        if let Some(existing) = self.retries.iter_mut().find(|r| r.action == action) {
            existing.due_minute = due_minute;
        } else {
            self.retries.push(RetryPending { action, due_minute });
        }
    }

    /// Returns the in-flight commit state whose `operation_id` equals `op`,
    /// for either the ack or the persistence slot, in `Firing` or
    /// `WaitingForRearm`. Exact-match only.
    fn commit_for_operation(&mut self, op: OperationId) -> Option<&mut CommitState> {
        match &mut self.alarm_runtime {
            AlarmRuntimeState::Firing {
                ack, persistence, ..
            }
            | AlarmRuntimeState::WaitingForRearm {
                ack, persistence, ..
            } => {
                if ack.inflight_is(op) {
                    Some(ack)
                } else if persistence.inflight_is(op) {
                    Some(persistence)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// True when the RTC alarm plan is fully confirmed and the device may
    /// deep-sleep on the strength of the hardware alarm register plus a
    /// maintenance wake (migration stage 2 "confirmable power pre-state").
    ///
    /// False while any of the following holds, because a deep sleep then
    /// could strand an unconfirmed RTC write or a ringing alarm:
    ///
    /// - an RTC program/disable/ack is in flight (`pending_rtc` /
    ///   `pending_residue_ack`);
    /// - a confirmable alarm commit (ACK or persistence) has failed and is
    ///   awaiting a retry (`retries` non-empty), or is in flight;
    /// - the alarm runtime is still `Firing` or `WaitingForRearm` (an alarm
    ///   the user has not fully dismissed, or whose re-arm has not been
    ///   confirmed);
    /// - the alarm runtime is `Degraded` (an RTC write/read failure means
    ///   the hardware slot may not match the stored list).
    ///
    /// In the settled, confirmed states (`Armed { .. }` / `Disarmed`) with
    /// no retries or pending RTC writes the register matches the stored
    /// alarm list, so the only wake the sleep must still guarantee is the
    /// maintenance timer - the caller computes that plan separately.
    pub fn rtc_alarm_plan_confirmed(&self) -> bool {
        if self.pending_rtc.is_some() || self.pending_residue_ack.is_some() {
            return false;
        }
        if !self.retries.is_empty() {
            return false;
        }
        match &self.alarm_runtime {
            AlarmRuntimeState::Armed { .. } | AlarmRuntimeState::Disarmed => true,
            AlarmRuntimeState::Firing { .. }
            | AlarmRuntimeState::WaitingForRearm { .. }
            | AlarmRuntimeState::Degraded { .. } => false,
        }
    }
}

/// The single state transition entry point. Mutates `state` and returns the
/// effects the side-effect execution layer must run.
pub fn update(state: &mut AppState, event: Event) -> Vec<EffectBatch> {
    match event {
        Event::Boot(snapshot) => transition_boot(state, snapshot),
        Event::RtcAlarmSnapshotReady(snapshot) => transition_rtc_snapshot(state, snapshot),
        Event::Tick(now) => transition_tick(state, now),
        Event::Button(button) => transition_button(state, button),
        Event::EffectCompleted(completion) => transition_effect_completed(state, completion),
        Event::EffectFailed(failure) => transition_effect_failed(state, failure),
        Event::UsbCommand(request) => transition_command(state, Channel::Usb, request),
        Event::BleCommand(request) => transition_command(state, Channel::Ble, request),
        // Remaining events are wired in later migration steps. They leave
        // state unchanged rather than guessing.
        _ => vec![],
    }
}

fn transition_boot(state: &mut AppState, snapshot: BootSnapshot) -> Vec<EffectBatch> {
    state.clock.now = snapshot.now;
    state.alarms.alarms = snapshot.alarms;
    state.todos.todos = snapshot.todos;
    state.inbox.items = snapshot.inbox;
    // Status-visible config facts (no secrets). The server URL presence
    // comes from the persisted config; wifi/timezone come from the boot
    // status fact.
    state.config.server_url = Some(snapshot.config.server_url).filter(|s| !s.is_empty());
    state.config.server_has_token = !snapshot.config.auth_token.is_empty();
    state.config.wifi_ssid = snapshot.status.wifi_ssid;
    state.config.wifi_has_password = snapshot.status.wifi_has_password;
    state.config.timezone_offset_minutes = snapshot.status.timezone_offset_minutes;
    state.connectivity.wifi_configured = state.config.wifi_configured();
    state.connectivity.server_configured = state.config.server_configured();

    let mut batches = Vec::new();
    // An asserted AF at boot resolves through the same path as a runtime
    // GPIO5 edge, so boot and run share one entry (acceptance #4). AIE is
    // part of the fact: AF set but interrupt disabled is residue, not a
    // real trigger.
    if snapshot.rtc_alarm_flag {
        if let Some(now) = snapshot.now {
            batches.extend(resolve_rtc_alarm(
                state,
                now,
                snapshot.rtc_alarm_interrupt_enabled,
            ));
        }
    } else if state.alarm_runtime == AlarmRuntimeState::Disarmed {
        // Otherwise keep the single hardware slot pointed at the nearest
        // stored alarm, or disable it if the list is empty.
        if let Some(now) = state.clock.now {
            batches.extend(program_alarm_for(state, &now));
        }
    }

    // A firing alarm path already rendered the ringing page (its render
    // generation is on the visible alarm state, not a home screen). Only
    // add boot's own render when the alarm path did not - a firing boot
    // must not double-render.
    if !matches!(state.alarm_runtime, AlarmRuntimeState::Firing { .. })
        && !batches
            .iter()
            .any(|b: &EffectBatch| b.render_generation.is_some())
    {
        state.render_generation = state.render_generation.next();
        batches.push(render_batch(state));
    }
    batches
}

/// Whether `now` (date + minute) is an actual occurrence of `alarm`.
/// Triggers must match the repeat rule, not just the wall-clock time.
fn is_due_now(alarm: &StoredAlarm, now: &DateTime) -> bool {
    alarm.enabled
        && alarm.hour == now.hour
        && alarm.minute == now.minute
        && alarm
            .repeat
            .fires_on(now.year, now.month, now.day, now.weekday)
}

/// Resolves an asserted AF flag against the local enabled-alarm list (the
/// "RTC signal / local data / current time are three independent facts"
/// flow). `aie` is the interrupt-enabled fact: a set AF with interrupts
/// disabled is stale residue, not a ringable trigger.
fn resolve_rtc_alarm(state: &mut AppState, now: DateTime, aie: bool) -> Vec<EffectBatch> {
    let fired_minute = now.to_unix() / 60;

    // Idempotency: while we're already ringing (or waiting to rearm), a
    // duplicate snapshot for the same asserted AF must not re-trigger,
    // overwrite `screen_before_ring`, or regenerate the in-flight op ids.
    if matches!(
        state.alarm_runtime,
        AlarmRuntimeState::Firing { .. } | AlarmRuntimeState::WaitingForRearm { .. }
    ) {
        return vec![];
    }

    // A residue ACK is in flight (AF was set but no valid alarm fired): a
    // repeated GPIO5 snapshot while that ACK is pending must not be
    // re-interpreted. It could otherwise (a) overwrite the pending
    // `pending_residue_ack` with a fresh op id, stranding the original
    // ACK's completion, or (b) once the wall-clock crosses into a stored
    // alarm's matching minute, re-interpret the *same* uncleared AF as a
    // real trigger and ring. The barrier is held until the matching
    // `AckDone` clears `pending_residue_ack` (and the RTC is reprogrammed)
    // or the ACK fails and its retry re-establishes a fresh barrier.
    if state.pending_residue_ack.is_some() {
        return vec![];
    }

    // A GPIO5 signal with the interrupt disabled has no runtime meaning -
    // clear the residue. The RTC register is only reprogrammed after this
    // ACK confirms (see `transition_effect_completed`), so a failed ACK
    // cannot leave AIE enabled over a still-set AF.
    if !aie {
        state.alarm_runtime = AlarmRuntimeState::Disarmed;
        return clear_residue_alarm(state, now);
    }

    // Deterministic choice when several alarms are due in the same minute:
    // the smallest `id`.
    let matched = state
        .alarms
        .alarms
        .iter()
        .filter(|a| is_due_now(a, &now))
        .min_by_key(|a| a.id)
        .cloned();

    match matched {
        Some(alarm) => {
            let alarm_id = alarm.id;
            let ack_id = state.next_operation_id();
            let persist_id = state.next_operation_id();
            state.screen_before_ring = state.screen.clone();
            state.alarm_runtime = AlarmRuntimeState::Firing {
                alarm_id,
                fired_minute,
                ack: CommitState::InFlight {
                    operation_id: ack_id,
                },
                persistence: CommitState::InFlight {
                    operation_id: persist_id,
                },
            };
            state.screen = Screen::AlarmRinging;

            // Expire/clean the fired alarm and persist; acknowledge the RTC
            // line. ACK, persistence and tone are independent enough to be
            // separate batches - ACK/persistence are confirmable (carry
            // their own op id), tone is fire-and-forget.
            let cleaned = remove_expired_once(state, alarm_id);
            let mut batches = vec![
                batch(
                    state,
                    ack_id,
                    FailurePolicy::AbortBatch,
                    vec![Effect::AcknowledgeRtcAlarm],
                ),
                batch(
                    state,
                    persist_id,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistAlarms(cleaned)],
                ),
                batch(
                    state,
                    OperationId(0),
                    FailurePolicy::Continue,
                    vec![Effect::StartTone],
                ),
            ];
            // The ringing page is a visible state: render it. A runtime
            // `RtcAlarmSnapshotReady` (not just Boot) must refresh the
            // screen, so resolution is responsible for this render.
            state.render_generation = state.render_generation.next();
            batches.push(render_batch(state));
            batches
        }
        // AF set but no alarm is due at this minute: clear residue. The RTC
        // register is reprogrammed only after this ACK confirms, so a failed
        // ACK cannot leave a stale AF with AIE re-enabled.
        None => {
            state.alarm_runtime = AlarmRuntimeState::Disarmed;
            clear_residue_alarm(state, now)
        }
    }
}

/// Emits the confirmable ACK that clears a *residue* AF (no valid alarm
/// fired); the RTC register is reprogrammed once `AckDone` arrives (see
/// `transition_effect_completed`). The ACK carries its own `OperationId`,
/// recorded in `pending_residue_ack` so its completion/failure is matched.
fn clear_residue_alarm(state: &mut AppState, at: DateTime) -> Vec<EffectBatch> {
    // Defensive: never overwrite an already-pending residue ACK with a
    // different op id - that would strand the original ACK's completion.
    // The `pending_residue_ack` barrier in `resolve_rtc_alarm` prevents
    // reaching here while one is pending, but keep the guard so a call from
    // any path cannot clobber it.
    if state.pending_residue_ack.is_some() {
        return vec![];
    }
    let op = state.next_operation_id();
    state.pending_residue_ack = Some(op);
    state.pending_residue_time = Some(at);
    vec![batch(
        state,
        op,
        FailurePolicy::AbortBatch,
        vec![Effect::AcknowledgeRtcAlarm],
    )]
}

fn transition_rtc_snapshot(state: &mut AppState, snapshot: RtcAlarmSnapshot) -> Vec<EffectBatch> {
    // The snapshot is the most recent time fact - record it so subsequent
    // `AckDone` / completion handlers can use it for re-programming and
    // rearm. Boot also sets `clock.now`; this keeps both paths consistent.
    state.clock.now = Some(snapshot.now);
    // A GPIO5 edge only requested a snapshot; the fact that AF is set does
    // not by itself mean a valid alarm is ringing - resolve it.
    if snapshot.alarm_flag {
        resolve_rtc_alarm(state, snapshot.now, snapshot.alarm_interrupt_enabled)
    } else {
        vec![]
    }
}

/// ENTER on the ringing screen dismisses the alarm (as long as it is still
/// firing), returning to the pre-ring page and copying the in-flight commit
/// state into `WaitingForRearm` so the background ack/persistence still
/// advance and the rearm happens on a later minute.
fn transition_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    if button == ButtonEvent::Pressed
        && state.screen == Screen::AlarmRinging
        && matches!(&state.alarm_runtime, AlarmRuntimeState::Firing { .. })
    {
        // Keep the current commit states; the background completions
        // continue to advance them while waiting for the minute to roll.
        let AlarmRuntimeState::Firing {
            alarm_id,
            fired_minute,
            ack,
            persistence,
        } = state.alarm_runtime.clone()
        else {
            return vec![];
        };
        state.alarm_runtime = AlarmRuntimeState::WaitingForRearm {
            fired_alarm_id: alarm_id,
            fired_minute,
            ack,
            persistence,
            minute_advanced: false,
        };
        state.screen = state.screen_before_ring.clone();
        state.render_generation = state.render_generation.next();
        vec![
            batch(
                state,
                OperationId(0),
                FailurePolicy::Continue,
                vec![Effect::StopTone],
            ),
            render_batch_with_intent(state, RenderIntent::Full),
        ]
    } else {
        vec![]
    }
}

fn transition_tick(state: &mut AppState, now: DateTime) -> Vec<EffectBatch> {
    let mut batches = Vec::new();
    let changed = state
        .clock
        .now
        .map(|prev| {
            prev.minute != now.minute
                || prev.hour != now.hour
                || prev.day != now.day
                || prev.month != now.month
                || prev.year != now.year
        })
        .unwrap_or(true);
    state.clock.now = Some(now);
    let current_minute = now.to_unix() / 60;

    // A failed ACK/persist/RTC-program operation whose backoff window has
    // elapsed is retried first (a fresh op id re-arms the in-flight state).
    batches.extend(maybe_retry(state, current_minute, &now));

    // After a ring, rearm happens once the minute has advanced AND both
    // ack/persistence commits have succeeded. `maybe_rearm` also runs from
    // the commit-completion path below, so a commit finishing after the
    // cross-minute tick re-arms immediately instead of waiting for the next
    // tick.
    if maybe_rearm(state, current_minute, &now) {
        batches.extend(program_alarm_for(state, &now));
    }

    if changed {
        state.render_generation = state.render_generation.next();
        batches.push(render_batch(state));
    }
    batches
}

/// Arms only the rearm logic (marks the minute advanced and checks whether
/// ack + persistence both succeeded), returning `true` when the caller
/// should program the next alarm. Does not itself program, so the caller
/// can decide whether to do so now (Tick) or as part of a larger batch.
/// Idempotent: once the conditions are met, the runtime state moves to
/// `Disarmed`.
fn maybe_rearm(state: &mut AppState, current_minute: u64, _now: &DateTime) -> bool {
    let AlarmRuntimeState::WaitingForRearm {
        fired_minute,
        ack,
        persistence,
        minute_advanced,
        ..
    } = &mut state.alarm_runtime
    else {
        return false;
    };
    if current_minute > *fired_minute {
        *minute_advanced = true;
    }
    let all_done = *ack == CommitState::Succeeded
        && *persistence == CommitState::Succeeded
        && *minute_advanced;
    if all_done {
        state.alarm_runtime = AlarmRuntimeState::Disarmed;
        true
    } else {
        false
    }
}

/// Re-issues a failed confirmable operation once its backoff window has
/// elapsed, using a fresh `OperationId`. Returns the batches to run.
fn maybe_retry(state: &mut AppState, current_minute: u64, now: &DateTime) -> Vec<EffectBatch> {
    // Process every retry whose window has elapsed, in a stable order
    // (Ack, PersistAlarms, then the RTC program/disable). Due retries are
    // removed; not-yet-due ones stay. Separate actions never overwrite each
    // other because each entry carries its own action.
    let mut due: Vec<RetryAction> = state
        .retries
        .iter()
        .filter(|r| current_minute >= r.due_minute)
        .map(|r| r.action.clone())
        .collect();
    // Order Ack -> Persist -> RTC so commit retries are re-issued before a
    // derived-register rebuild.
    due.sort_by_key(action_priority);

    state.retries.retain(|r| current_minute < r.due_minute);

    if due.is_empty() {
        return vec![];
    }

    let mut batches = Vec::new();
    for action in due {
        batches.extend(issue_retry(state, action, now));
    }
    batches
}

/// Stable partial order for retry dispatch: commits first, then the RTC
/// program/disable rebuild.
fn action_priority(action: &RetryAction) -> u8 {
    match action {
        RetryAction::Ack => 0,
        RetryAction::PersistAlarms => 1,
        RetryAction::ProgramRtc | RetryAction::DisableRtc => 2,
    }
}

fn issue_retry(state: &mut AppState, action: RetryAction, now: &DateTime) -> Vec<EffectBatch> {
    match action {
        RetryAction::Ack => {
            let op = state.next_operation_id();
            if let AlarmRuntimeState::Firing { ack, .. }
            | AlarmRuntimeState::WaitingForRearm { ack, .. } = &mut state.alarm_runtime
            {
                *ack = CommitState::InFlight { operation_id: op };
            } else {
                // No ring commit to re-arm: this is the residue-clear ACK,
                // so re-record it and keep the RTC register un-programmed
                // until it succeeds.
                state.pending_residue_ack = Some(op);
            }
            vec![batch(
                state,
                op,
                FailurePolicy::AbortBatch,
                vec![Effect::AcknowledgeRtcAlarm],
            )]
        }
        RetryAction::PersistAlarms => {
            let op = state.next_operation_id();
            if let AlarmRuntimeState::Firing { persistence, .. }
            | AlarmRuntimeState::WaitingForRearm { persistence, .. } = &mut state.alarm_runtime
            {
                *persistence = CommitState::InFlight { operation_id: op };
            }
            vec![batch(
                state,
                op,
                FailurePolicy::AbortBatch,
                vec![Effect::PersistAlarms(state.alarms.alarms.clone())],
            )]
        }
        RetryAction::ProgramRtc | RetryAction::DisableRtc => {
            // Re-run the RTC programming decision from the authoritative list
            // so the derived register is rebuilt (NVS is never rolled back).
            program_alarm_for(state, now)
        }
    }
}

fn transition_effect_completed(
    state: &mut AppState,
    completion: EffectCompletion,
) -> Vec<EffectBatch> {
    // Exact-match gate: only touch the commit state whose op id matches.
    let mut batches = Vec::new();
    match completion.output {
        EffectOutput::Persisted(PersistTarget::Alarms) => {
            if let Some(commit) = state.commit_for_operation(completion.operation_id) {
                *commit = CommitState::Succeeded;
            }
        }
        EffectOutput::AckDone => {
            if let Some(commit) = state.commit_for_operation(completion.operation_id) {
                *commit = CommitState::Succeeded;
            }
            // A residue ACK confirmed: now that the stale AF is cleared it
            // is safe to re-program the derived RTC register (which is what
            // the "AF set but no valid alarm" paths deferred until now).
            if state.pending_residue_ack == Some(completion.operation_id) {
                state.pending_residue_ack = None;
                if let Some(now) = state.clock.now {
                    batches.extend(program_alarm_for(state, &now));
                }
            }
        }
        EffectOutput::RtcProgrammed
            if state
                .pending_rtc
                .as_ref()
                .is_some_and(|p| p.operation_id() == completion.operation_id) =>
        {
            match state.pending_rtc.take() {
                Some(PendingRtc::Program { alarm_id, .. }) => {
                    state.alarm_runtime = AlarmRuntimeState::Armed { alarm_id };
                }
                Some(PendingRtc::Disable { .. }) => {
                    state.alarm_runtime = AlarmRuntimeState::Disarmed;
                }
                None => {}
            }
        }
        EffectOutput::Persisted(PersistTarget::Timezone) => {
            // A SetTimezone timezone-offset persist confirmed: apply the
            // target offset to the status-visible config now (NVS holds it
            // authoritatively; the generic resolver below sends the reply).
            for channel in [Channel::Usb, Channel::Ble] {
                apply_confirmed_timezone(state, completion.operation_id, channel);
            }
        }
        _ => {}
    }
    if matches!(
        completion.output,
        EffectOutput::Persisted(_)
            | EffectOutput::RtcProgrammed
            | EffectOutput::RtcTimeWritten
            | EffectOutput::AckDone
    ) {
        for channel in [Channel::Usb, Channel::Ble] {
            if let Some((found_channel, reply)) =
                resolve_command_confirmation(state, completion.operation_id, channel)
            {
                batches.push(reply_batch(state, found_channel, reply));
            }
        }
    }
    // A commit that completes after the cross-minute tick has already set
    // `minute_advanced` must re-arm right away, not wait for the next tick.
    if let Some(now) = state.clock.now {
        let current_minute = now.to_unix() / 60;
        if maybe_rearm(state, current_minute, &now) {
            batches.extend(program_alarm_for(state, &now));
        }
    }
    batches
}

fn transition_effect_failed(state: &mut AppState, failure: EffectFailure) -> Vec<EffectBatch> {
    // A failed confirmable operation is retried after a backoff; the
    // current minute comes from the clock so the due window is sane.
    let now_minute = state.clock.now.map(|t| t.to_unix() / 60).unwrap_or(0);
    match failure.error {
        // ACK / persistence failure: mark that exact commit failed and
        // schedule a retry (it stays in the alarm runtime so a later
        // re-issue re-arms the same in-flight op).
        EffectError::Ack(_) => {
            if let Some(commit) = state.commit_for_operation(failure.operation_id) {
                *commit = CommitState::Failed {
                    error: failure.error.clone(),
                };
                state.schedule_retry(RetryAction::Ack, now_minute);
            } else if state.pending_residue_ack == Some(failure.operation_id) {
                // A residue ACK failed: do NOT re-open AIE (the RTC is only
                // reprogrammed after a successful ACK). Retry the ACK so the
                // stale AF is eventually cleared.
                state.pending_residue_ack = None;
                state.schedule_retry(RetryAction::Ack, now_minute);
            }
        }
        EffectError::Persist(_) => {
            if let Some(commit) = state.commit_for_operation(failure.operation_id) {
                *commit = CommitState::Failed {
                    error: failure.error.clone(),
                };
                state.schedule_retry(RetryAction::PersistAlarms, now_minute);
            }
        }
        // RTC program/disable failure (or a startup re-arm failure): NVS
        // stays authoritative, the derived register degrades, and a later
        // retry rebuilds it.
        EffectError::Rtc(_)
            if state
                .pending_rtc
                .as_ref()
                .is_some_and(|p| p.operation_id() == failure.operation_id) =>
        {
            // On a failed program record what we wanted to disarm/arm so a
            // later retry knows the target; NVS stays authoritative.
            let degraded_action = match state.pending_rtc.take() {
                Some(PendingRtc::Program { alarm_id, .. }) => {
                    state.alarm_runtime = AlarmRuntimeState::Degraded {
                        desired_alarm_id: Some(alarm_id),
                        error: AlarmHardwareError::RtcWriteFailed,
                    };
                    RetryAction::ProgramRtc
                }
                Some(PendingRtc::Disable { .. }) => {
                    state.alarm_runtime = AlarmRuntimeState::Degraded {
                        desired_alarm_id: None,
                        error: AlarmHardwareError::RtcWriteFailed,
                    };
                    RetryAction::DisableRtc
                }
                None => return vec![],
            };
            state.schedule_retry(degraded_action, now_minute);
        }
        // All other failure kinds (render, sync, tone, ble, sleep) are
        // surfaced but do not alter the alarm commit state in step 1.
        _ => {}
    }
    // A command's confirmable effect failed: error the client on the
    // transport that sent the command and clear the slot. This is separate
    // from the alarm-internal retry logic above - a control command is not
    // auto-retried (the client resends).
    let mut batches = Vec::new();
    let error_message = match &failure.error {
        EffectError::Persist(msg)
        | EffectError::Rtc(msg)
        | EffectError::Ack(msg)
        | EffectError::Sync(msg) => msg.clone(),
        _ => String::new(),
    };
    if !error_message.is_empty() {
        for channel in [Channel::Usb, Channel::Ble] {
            if let Some(reply) =
                fail_command_reply(state, channel, failure.operation_id, error_message.clone())
            {
                batches.push(reply_batch(state, channel, reply));
            }
        }
    }
    batches
}

fn transition_command(
    state: &mut AppState,
    channel: Channel,
    request: ControlRequest,
) -> Vec<EffectBatch> {
    // One in-flight command per transport. If the slot is busy, reply busy
    // WITHOUT overwriting the pending reply - overwriting would strand the
    // first request and block the deep-sleep precondition. The busy reply
    // goes only to the channel that sent the new command.
    let slot_occupied = match channel {
        Channel::Usb => state.pending_usb_reply.is_some(),
        Channel::Ble => state.pending_ble_reply.is_some(),
    };
    if slot_occupied {
        return vec![reply_batch(state, channel, Reply::Busy)];
    }

    match request {
        // SyncNow's async result flow (SyncCompleted -> merge/persist/RTC ->
        // final reply) is wired in the next Stage-3 increment. Until then
        // the firmware handles SyncNow on its legacy path and never routes
        // it here; keep the transition a no-op rather than strand a reply
        // slot that nothing will resolve.
        ControlRequest::SyncNow => vec![],
        // SetServer's stale-ETag-clear side effect (NVS sync-metadata
        // bookkeeping) migrates with the sync-metadata state; the firmware
        // keeps routing it on the legacy path until then. If it ever does
        // reach the runner, refuse loudly rather than hang the transport.
        ControlRequest::SetServer { .. } => vec![reply_batch(
            state,
            channel,
            Reply::Error {
                message: "command not yet migrated to the state machine".into(),
            },
        )],
        ControlRequest::ClearAlarms => {
            // Empty the local list (authoritative NVS source) and disarm the
            // RTC slot. Two confirmable ops; the reply waits for both, and a
            // failure of either errors the command and releases the slot.
            let persist_op = state.next_operation_id();
            let rtc_op = state.next_operation_id();
            state.alarms.alarms.clear();
            // The stored list is now empty, so the runtime's armed target is
            // gone; the RTC disable below confirms the hardware follows.
            state.alarm_runtime = AlarmRuntimeState::Disarmed;
            set_pending_reply(
                state,
                channel,
                persist_op,
                ControlRequest::ClearAlarms,
                vec![persist_op, rtc_op],
            );
            vec![
                batch(
                    state,
                    persist_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistAlarms(vec![])],
                ),
                batch(
                    state,
                    rtc_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::DisableRtcAlarm],
                ),
            ]
        }
        ControlRequest::GetStatus => {
            // A pure read: build the status reply from the facts in state
            // (no side effects, no pending slot). Same result regardless of
            // transport - the channel only selects where the reply goes.
            let status = Reply::Status {
                wifi_configured: state.config.wifi_configured(),
                server_configured: state.config.server_configured(),
                wifi_connected: state.connectivity.wifi_connected,
                wifi_ssid: state.config.wifi_ssid.clone(),
                wifi_has_password: state.config.wifi_has_password,
                server_url: state.config.server_url.clone(),
                server_has_token: state.config.server_has_token,
                timezone_offset_minutes: state.config.timezone_offset_minutes,
            };
            vec![reply_batch(state, channel, status)]
        }
        ControlRequest::SetTimezone { offset_minutes } => {
            // Validate the offset is a sane UTC window first. The hardware
            // clock is shifted by the delta (new - old) so the *instant* is
            // preserved; both the RTC write and the timezone persist are
            // confirmable, and the reply waits for both. On failure the
            // in-state offset is rolled back by the failure handler.
            if !(-720..=840).contains(&offset_minutes) {
                return vec![reply_batch(
                    state,
                    channel,
                    Reply::Error {
                        message: "Timezone offset must be between -720 and 840 minutes".into(),
                    },
                )];
            }
            let Some(now) = state.clock.now else {
                return vec![reply_batch(
                    state,
                    channel,
                    Reply::Error {
                        message: "System time not available".into(),
                    },
                )];
            };
            let old_offset = state.config.timezone_offset_minutes;
            let delta = (offset_minutes - old_offset) as i32;
            let shifted = now.shifted_minutes(delta);
            let time_op = state.next_operation_id();
            let tz_op = state.next_operation_id();
            // The offset is NOT updated optimistically: NVS is authoritative
            // and only the confirmed persist applies it (see
            // transition_effect_completed's Persisted(Timezone) branch). A
            // failed command therefore never leaves the in-state offset
            // diverged from NVS.
            set_pending_reply(
                state,
                channel,
                time_op,
                ControlRequest::SetTimezone { offset_minutes },
                vec![time_op, tz_op],
            );
            vec![
                batch(
                    state,
                    time_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::WriteRtcTime(shifted)],
                ),
                batch(
                    state,
                    tz_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistTimezone(offset_minutes)],
                ),
            ]
        }
        // Other commands need their storage/network side effects, wired in
        // migration step 3. Refuse rather than guess.
        _ => vec![reply_batch(
            state,
            channel,
            Reply::Error {
                message: "command not yet supported".into(),
            },
        )],
    }
}

// ---- helpers -------------------------------------------------------------

/// Records an in-flight command on `channel`'s pending-reply slot. The
/// slot is taken (at most one command per transport); the caller has
/// already checked it is free.
fn set_pending_reply(
    state: &mut AppState,
    channel: Channel,
    op: OperationId,
    request: ControlRequest,
    awaiting_ops: Vec<OperationId>,
) {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    *slot = Some(PendingReply::awaiting(op, request, channel, awaiting_ops));
}

/// Applies a confirmed SetTimezone persist to the status-visible offset.
/// The target comes from the pending command's request (the slot holds the
/// `SetTimezone { offset_minutes }` that initiated the persist), so the
/// offset field only ever reflects a value NVS now holds.
fn apply_confirmed_timezone(state: &mut AppState, completed_op: OperationId, channel: Channel) {
    let slot = match channel {
        Channel::Usb => &state.pending_usb_reply,
        Channel::Ble => &state.pending_ble_reply,
    };
    let Some(pending) = slot else { return };
    if !pending.awaiting_ops.contains(&completed_op) {
        return;
    }
    if let ControlRequest::SetTimezone { offset_minutes } = &pending.request {
        state.config.timezone_offset_minutes = *offset_minutes;
    }
}

/// Resolves `channel`'s pending-reply slot after one of its awaited
/// confirmable ops completed. Removes the op from `awaiting_ops`; when the
/// set empties, takes the slot and returns the final reply to send on that
/// channel. Returns `None` while the command still awaits another op, or if
/// the slot does not await this op.
fn resolve_command_confirmation(
    state: &mut AppState,
    completed_op: OperationId,
    channel: Channel,
) -> Option<(Channel, ControlReply)> {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    let pending = slot.as_mut()?;
    let before = pending.awaiting_ops.len();
    pending.awaiting_ops.retain(|op| *op != completed_op);
    if pending.awaiting_ops.len() == before {
        // This slot was not awaiting this op.
        return None;
    }
    if pending.awaiting_ops.is_empty() {
        let reply = pending.reply.take().unwrap_or(ControlReply::Ok);
        *slot = None;
        Some((channel, reply))
    } else {
        None
    }
}

/// Fails an in-flight command on `channel` when `failed_op` is one of its
/// awaited confirmable ops: the client gets an error reply and the slot
/// clears. The request is not auto-retried (the client resends).
fn fail_command_reply(
    state: &mut AppState,
    channel: Channel,
    failed_op: OperationId,
    message: String,
) -> Option<ControlReply> {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    let pending = slot.as_ref()?;
    if !pending.awaiting_ops.contains(&failed_op) {
        return None;
    }
    let reply = ControlReply::Error { message };
    *slot = None;
    Some(reply)
}

/// Removes a fired one-shot from the stored list (its date has passed);
/// recurrences stay. Returns the cleaned list to persist.
fn remove_expired_once(state: &mut AppState, alarm_id: u8) -> Vec<StoredAlarm> {
    let preserved: Vec<StoredAlarm> = state
        .alarms
        .alarms
        .iter()
        .filter(|a| a.id != alarm_id || !matches!(a.repeat, Repeat::Once { .. }))
        .cloned()
        .collect();
    state.alarms.alarms = preserved.clone();
    preserved
}

fn batch(
    state: &mut AppState,
    operation_id: OperationId,
    failure_policy: FailurePolicy,
    effects: Vec<Effect>,
) -> EffectBatch {
    EffectBatch {
        id: state.next_batch_id(),
        operation_id,
        render_generation: None,
        effects,
        failure_policy,
    }
}

/// A single-reply batch to one transport, carrying no business operation
/// id (a busy / immediate-error reply is not a tracked in-flight op).
fn reply_batch(state: &mut AppState, channel: Channel, reply: ControlReply) -> EffectBatch {
    batch(
        state,
        OperationId(0),
        FailurePolicy::Continue,
        vec![Effect::Reply { channel, reply }],
    )
}

fn render_batch(state: &mut AppState) -> EffectBatch {
    render_batch_with_intent(state, RenderIntent::Partial)
}

fn render_batch_with_intent(state: &mut AppState, intent: RenderIntent) -> EffectBatch {
    EffectBatch {
        id: state.next_batch_id(),
        operation_id: OperationId(0),
        render_generation: Some(state.render_generation),
        effects: vec![Effect::Render(RenderRequest {
            generation: state.render_generation,
            intent,
        })],
        failure_policy: FailurePolicy::Continue,
    }
}

/// Programs the RTC alarm slot to the nearest enabled alarm (using the
/// shared repeat-aware `alarm_regs_for` mapping), or disables it when the
/// list is empty. The programming is confirmable: it carries an op id that
/// the executor reports back on completion/failure.
fn program_alarm_for(state: &mut AppState, now: &DateTime) -> Vec<EffectBatch> {
    let op = state.next_operation_id();
    match next_due(&state.alarms.alarms, now) {
        Some(alarm) => {
            let regs = match crate::alarm_schedule::alarm_regs_for(alarm, now) {
                Some(regs) => regs,
                // A Once alarm outside its target month must stay unarmed.
                None => {
                    state.pending_rtc = Some(PendingRtc::Disable { operation_id: op });
                    return vec![batch(
                        state,
                        op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::DisableRtcAlarm],
                    )];
                }
            };
            state.pending_rtc = Some(PendingRtc::Program {
                operation_id: op,
                alarm_id: alarm.id,
            });
            vec![batch(
                state,
                op,
                FailurePolicy::AbortBatch,
                vec![Effect::ProgramRtcAlarm(regs)],
            )]
        }
        None => {
            state.pending_rtc = Some(PendingRtc::Disable { operation_id: op });
            vec![batch(
                state,
                op,
                FailurePolicy::AbortBatch,
                vec![Effect::DisableRtcAlarm],
            )]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(hour: u8, minute: u8) -> DateTime {
        DateTime {
            year: 2026,
            month: 8,
            day: 31,
            weekday: 0,
            hour,
            minute,
            second: 0,
            voltage_low: false,
        }
    }

    /// `2026-08-31` is a Monday (weekday 1). Helper to build a date with an
    /// explicit weekday.
    fn dt_full(hour: u8, minute: u8, weekday: u8, day: u8) -> DateTime {
        DateTime {
            year: 2026,
            month: 8,
            day,
            weekday,
            hour,
            minute,
            second: 0,
            voltage_low: false,
        }
    }

    fn alarm(id: u8, hour: u8, minute: u8) -> StoredAlarm {
        StoredAlarm {
            id,
            hour,
            minute,
            repeat: Repeat::Daily,
            enabled: true,
            label: String::new(),
        }
    }

    fn boot_snapshot(
        alarms: Vec<StoredAlarm>,
        now: Option<DateTime>,
        af: bool,
        aie: bool,
    ) -> BootSnapshot {
        BootSnapshot {
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
        }
    }

    #[test]
    fn boot_with_no_alarm_flag_arms_the_nearest_alarm() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
        assert!(matches!(state.alarm_runtime, AlarmRuntimeState::Disarmed));
        assert!(batches.iter().any(|b| {
            b.effects.contains(&Effect::ProgramRtcAlarm(AlarmRegs {
                minute: 0,
                hour: 9,
                day: None,
                weekday: None,
            }))
        }));
    }

    #[test]
    fn default_state_rtc_plan_is_confirmed() {
        // A fresh default AppState: Disarmed, no retries, no pending RTC.
        let state = AppState::default();
        assert!(state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn rtc_plan_not_confirmed_while_program_in_flight() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        // A program was requested and is in flight (pending_rtc set); the
        // plan is not confirmed until the RtcProgrammed completion lands.
        assert!(
            batches.iter().any(|b| b
                .effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))),
            "boot must request the program"
        );
        assert!(!state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn rtc_plan_confirmed_after_program_success() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        let prog = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            })
            .map(|b| b.operation_id)
            .unwrap();
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: prog,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Armed { alarm_id: 1 }
        ));
        assert!(state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn rtc_plan_not_confirmed_while_firing() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let _ = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));
        assert!(!state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn rtc_plan_not_confirmed_with_pending_retry() {
        let mut state = AppState::default();
        // A degraded runtime (failed program) leaves a retry scheduled;
        // the register may not match the list, so deep sleep is unsafe.
        state.retries.push(RetryPending {
            action: RetryAction::ProgramRtc,
            due_minute: 10,
        });
        state.alarm_runtime = AlarmRuntimeState::Degraded {
            desired_alarm_id: Some(1),
            error: AlarmHardwareError::RtcWriteFailed,
        };
        assert!(!state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn boot_with_af_and_matching_daily_alarm_enters_firing() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { alarm_id: 1, .. }
        ));
    }

    #[test]
    fn af_with_no_matching_alarm_disarms_and_reprograms() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
    }

    #[test]
    fn empty_alarm_list_produces_disable_not_ring() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![], Some(dt(9, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::DisableRtcAlarm)));
    }

    #[test]
    fn af_set_but_interrupt_disabled_is_residue_not_a_trigger() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, false);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
    }

    #[test]
    fn weekly_alarm_does_not_ring_on_a_different_weekday() {
        let mut state = AppState::default();
        // weekly on Sunday (weekday 0); 2026-08-31 is Monday (weekday 1).
        let weekly = StoredAlarm {
            id: 1,
            hour: 9,
            minute: 0,
            repeat: Repeat::Weekly { days: vec![0] },
            enabled: true,
            label: String::new(),
        };
        let snapshot = boot_snapshot(vec![weekly], Some(dt_full(9, 0, 1, 31)), true, true);
        let _batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
    }

    #[test]
    fn weekly_alarm_rings_on_matching_weekday() {
        let mut state = AppState::default();
        // weekly on Monday (weekday 1); 2026-08-31 is Monday (weekday 1).
        let weekly = StoredAlarm {
            id: 1,
            hour: 9,
            minute: 0,
            repeat: Repeat::Weekly { days: vec![1] },
            enabled: true,
            label: String::new(),
        };
        let snapshot = boot_snapshot(vec![weekly], Some(dt_full(9, 0, 1, 31)), true, true);
        let _batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
    }

    #[test]
    fn once_alarm_does_not_ring_outside_its_month() {
        let mut state = AppState::default();
        // Once on a date in a different month (September); now is August.
        let once = StoredAlarm {
            id: 1,
            hour: 9,
            minute: 0,
            repeat: Repeat::Once {
                year: 2026,
                month: 9,
                day: 5,
            },
            enabled: true,
            label: String::new(),
        };
        let snapshot = boot_snapshot(vec![once], Some(dt(9, 0)), true, true);
        let _batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::Home);
    }

    #[test]
    fn empty_list_with_aie_on_produces_disable() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![], Some(dt(9, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::DisableRtcAlarm)));
    }

    #[test]
    fn dismiss_transitions_to_waiting_for_rearm_and_stops_tone() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let batches = update(&mut state, Event::Button(ButtonEvent::Pressed));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::StopTone)));
    }

    #[test]
    fn rearm_only_after_minute_and_commits_complete() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        update(&mut state, Event::Boot(snapshot));
        update(&mut state, Event::Button(ButtonEvent::Pressed));

        // Grab the two in-flight op ids from the WaitingForRearm state.
        let (ack_op, persist_op) = match &state.alarm_runtime {
            AlarmRuntimeState::WaitingForRearm {
                ack, persistence, ..
            } => (
                match ack {
                    CommitState::InFlight { operation_id } => *operation_id,
                    _ => OperationId(0),
                },
                match persistence {
                    CommitState::InFlight { operation_id } => *operation_id,
                    _ => OperationId(0),
                },
            ),
            _ => (OperationId(0), OperationId(0)),
        };

        // Cross-minute tick while commits are still pending: must not rearm.
        let batches = update(&mut state, Event::Tick(dt(9, 1)));
        assert!(!batches.iter().any(|b| b
            .effects
            .iter()
            .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));

        // The ack completes first (minute already advanced, so it sits
        // waiting for the persistence commit too).
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: ack_op,
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));

        // The persist commit completes last, after the minute has already
        // advanced -> rearm *immediately*, not on the next tick.
        let batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
                || b.effects.contains(&Effect::DisableRtcAlarm)
        }));
        assert!(matches!(state.alarm_runtime, AlarmRuntimeState::Disarmed));
    }

    #[test]
    fn stale_completion_with_wrong_opid_does_not_advance() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        update(&mut state, Event::Boot(snapshot));
        // A stale Persisted completion with an unknown op id must not set
        // the persistence commit to Succeeded.
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: OperationId(999),
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                persistence: CommitState::InFlight { .. },
                ..
            }
        ));
    }

    #[test]
    fn rtc_program_failure_degrades_not_rolls_back() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        // Find the RTC program op id from the batch being returned.
        let rtc_op = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            })
            .map(|b| b.operation_id)
            .unwrap();
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                error: EffectError::Rtc("write failed".into()),
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Degraded { .. }
        ));
    }

    #[test]
    fn busy_transport_replies_busy_without_overwriting_pending() {
        let mut state = AppState::default();
        let op = state.next_operation_id();
        set_pending_reply(
            &mut state,
            Channel::Usb,
            op,
            ControlRequest::SyncNow,
            vec![op],
        );
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
        assert!(state.pending_usb_reply.is_some());
        assert!(batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Busy
                    }
                )
            })
        }));
    }

    #[test]
    fn busy_reply_goes_to_the_busy_transport_only() {
        let mut state = AppState::default();
        let op = state.next_operation_id();
        // USB has an in-flight SyncNow; BLE is free.
        set_pending_reply(
            &mut state,
            Channel::Usb,
            op,
            ControlRequest::SyncNow,
            vec![op],
        );
        // A second USB command is busy on USB.
        let usb_batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
        assert!(usb_batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Busy
                    }
                )
            })
        }));
        // A BLE command while USB is busy is NOT busy: the transports have
        // independent slots, so it is processed - GetStatus is a pure read
        // and replies Status to BLE only.
        let ble_batches = update(&mut state, Event::BleCommand(ControlRequest::GetStatus));
        assert!(!ble_batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        reply: Reply::Busy
                    }
                )
            })
        }));
        assert!(ble_batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        reply: Reply::Status { .. }
                    }
                )
            })
        }));
        // The USB pending reply is untouched.
        assert!(state.pending_usb_reply.is_some());
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn same_command_on_usb_and_ble_replies_on_its_own_channel() {
        // "USB/BLE 同一命令产生相同业务结果": GetStatus must produce the same
        // Status payload on both transports, each delivered only to its own
        // channel.
        let mut state = AppState::default();
        let usb_batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
        let ble_batches = update(&mut state, Event::BleCommand(ControlRequest::GetStatus));
        let usb_status = usb_batches.iter().find_map(|b| {
            b.effects.iter().find_map(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Status { .. },
                } => Some(()),
                _ => None,
            })
        });
        let ble_status = ble_batches.iter().find_map(|b| {
            b.effects.iter().find_map(|e| match e {
                Effect::Reply {
                    channel: Channel::Ble,
                    reply: Reply::Status { .. },
                } => Some(()),
                _ => None,
            })
        });
        assert!(usb_status.is_some(), "USB GetStatus replies Status");
        assert!(ble_status.is_some(), "BLE GetStatus replies Status");
        // No cross-channel leakage.
        assert!(!usb_batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        ..
                    }
                )
            })
        }));
        assert!(!ble_batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        ..
                    }
                )
            })
        }));
    }

    #[test]
    fn set_server_refuses_until_migrated_with_sync_metadata() {
        // SetServer is not yet routed to the runner: its stale-ETag-clear
        // side effect depends on sync-metadata state that migrates with the
        // SyncCompleted increment. Until then the state machine refuses it
        // loudly (the firmware keeps it on the legacy path, so this branch
        // only guards against a future mis-routing that would otherwise
        // hang the transport).
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetServer {
                url: "https://sync.example".into(),
                token: "secret".into(),
            }),
        );
        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::PersistConfig(_)))
        }));
        assert!(batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Error { .. }
                    }
                )
            })
        }));
        assert!(
            state.pending_usb_reply.is_none(),
            "no slot taken on refusal"
        );
    }

    #[test]
    fn boot_populates_status_visible_config_facts() {
        let mut state = AppState::default();
        let snapshot = BootSnapshot {
            wake_cause: WakeCause::Other,
            now: Some(dt(8, 0)),
            rtc_alarm_flag: false,
            rtc_alarm_interrupt_enabled: true,
            alarms: vec![],
            todos: vec![],
            inbox: vec![],
            config: DeviceConfig {
                server_url: "https://server.example".into(),
                auth_token: "sekrit".into(),
            },
            status: DeviceStatus {
                wifi_ssid: Some("home-wifi".into()),
                wifi_has_password: true,
                timezone_offset_minutes: 480,
            },
        };
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(
            state.config.server_url.as_deref(),
            Some("https://server.example")
        );
        assert!(state.config.server_has_token);
        assert_eq!(state.config.wifi_ssid.as_deref(), Some("home-wifi"));
        assert!(state.config.wifi_has_password);
        assert_eq!(state.config.timezone_offset_minutes, 480);
        assert!(state.connectivity.wifi_configured);
        assert!(state.connectivity.server_configured);
    }

    #[test]
    fn get_status_replies_status_from_state_facts() {
        let mut state = AppState::default();
        state.config.wifi_ssid = Some("home-wifi".into());
        state.config.wifi_has_password = true;
        state.config.server_url = Some("https://server.example".into());
        state.config.server_has_token = true;
        state.config.timezone_offset_minutes = -300;
        state.connectivity.wifi_connected = true;
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
        let status_reply = batches.iter().find_map(|b| {
            b.effects.iter().find_map(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply:
                        Reply::Status {
                            wifi_configured,
                            server_configured,
                            wifi_connected,
                            wifi_ssid,
                            wifi_has_password,
                            server_url,
                            server_has_token,
                            timezone_offset_minutes,
                        },
                } => Some((
                    *wifi_configured,
                    *server_configured,
                    *wifi_connected,
                    wifi_ssid.clone(),
                    *wifi_has_password,
                    server_url.clone(),
                    *server_has_token,
                    *timezone_offset_minutes,
                )),
                _ => None,
            })
        });
        let (wifi_cfg, server_cfg, wifi_conn, ssid, has_pw, url, has_tok, tz) =
            status_reply.expect("GetStatus must reply Status");
        assert!(wifi_cfg && server_cfg && wifi_conn);
        assert_eq!(ssid.as_deref(), Some("home-wifi"));
        assert!(has_pw);
        assert_eq!(url.as_deref(), Some("https://server.example"));
        assert!(has_tok);
        assert_eq!(tz, -300);
        assert!(state.pending_usb_reply.is_none(), "GetStatus takes no slot");
    }

    #[test]
    fn set_timezone_writes_rtc_and_persists_then_replies_ok() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0)); // 10:00 local under old offset
        state.config.timezone_offset_minutes = 0; // old UTC+0
                                                  // New offset UTC+8: delta +480 -> RTC becomes 18:00.
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetTimezone {
                offset_minutes: 480,
            }),
        );
        assert!(state.pending_usb_reply.is_some());
        let rtc_batch = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::WriteRtcTime(_)))
            })
            .expect("SetTimezone must write the RTC clock");
        assert!(matches!(
            rtc_batch
                .effects
                .iter()
                .find(|e| matches!(e, Effect::WriteRtcTime(_))),
            Some(Effect::WriteRtcTime(dt)) if dt.hour == 18 && dt.minute == 0
        ));
        let rtc_op = rtc_batch.operation_id;
        let tz_batch = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistTimezone(_)))
            })
            .expect("SetTimezone must persist the offset");
        let tz_op = tz_batch.operation_id;
        // RTC write alone confirms: no reply yet, offset not yet applied.
        let mid = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                output: EffectOutput::RtcTimeWritten,
            }),
        );
        assert!(
            !mid.iter()
                .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))),
            "reply must wait for the timezone persist"
        );
        assert_eq!(
            state.config.timezone_offset_minutes, 0,
            "offset applies on persist confirm"
        );
        // Timezone persist confirms -> offset applied + Ok on USB.
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: tz_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Timezone),
            }),
        );
        assert_eq!(state.config.timezone_offset_minutes, 480);
        assert!(done.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Ok
                    }
                )
            })
        }));
        assert!(state.pending_usb_reply.is_none());
    }

    #[test]
    fn set_timezone_rtc_failure_replies_error_and_leaves_offset_unchanged() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 0;
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetTimezone {
                offset_minutes: 480,
            }),
        );
        let rtc_op = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::WriteRtcTime(_)))
            })
            .unwrap()
            .operation_id;
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                error: EffectError::Rtc("rtc bus error".into()),
            }),
        );
        let error_seen = failed.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("rtc bus error"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(state.pending_usb_reply.is_none(), "slot released");
        assert_eq!(
            state.config.timezone_offset_minutes, 0,
            "offset must not change on a failed SetTimezone"
        );
    }

    #[test]
    fn set_timezone_rejects_out_of_range_offset() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::BleCommand(ControlRequest::SetTimezone {
                offset_minutes: 2000,
            }),
        );
        assert!(batches.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        reply: Reply::Error { .. }
                    }
                )
            })
        }));
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn clear_alarms_persists_empty_and_disables_rtc_then_replies_ok() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0), alarm(2, 10, 30)];
        state.alarm_runtime = AlarmRuntimeState::Armed { alarm_id: 1 };
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::ClearAlarms));
        assert!(state.alarms.alarms.is_empty());
        // Two confirmable batches: PersistAlarms(empty) + DisableRtcAlarm.
        let persist_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::PersistAlarms(vec![])))
            .expect("ClearAlarms must persist an empty list")
            .operation_id;
        let rtc_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::DisableRtcAlarm))
            .expect("ClearAlarms must disable the RTC alarm")
            .operation_id;
        assert!(state.pending_usb_reply.is_some());
        // Only the persist confirms: no reply yet (rtc still awaited).
        let mid = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        assert!(
            !mid.iter()
                .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))),
            "reply must wait for the RTC disable"
        );
        // RTC disable confirms -> final Ok on USB, slot released.
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        assert!(done.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Ok
                    }
                )
            })
        }));
        assert!(state.pending_usb_reply.is_none());
        assert_eq!(state.alarm_runtime, AlarmRuntimeState::Disarmed);
    }

    #[test]
    fn clear_alarms_rtc_failure_replies_error_and_releases_slot() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::ClearAlarms));
        let persist_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::PersistAlarms(vec![])))
            .unwrap()
            .operation_id;
        let rtc_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::DisableRtcAlarm))
            .unwrap()
            .operation_id;
        // Persist succeeds, RTC disable fails -> error reply + slot release.
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                error: EffectError::Rtc("rtc bus error".into()),
            }),
        );
        let error_seen = failed.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("rtc bus error"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(
            state.pending_usb_reply.is_none(),
            "slot released after failure"
        );
    }

    // ---- review round 2 findings -----------------------------------------

    #[test]
    fn runtime_snapshot_renders_the_ringing_page() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(batches.iter().any(|b| {
            b.render_generation.is_some()
                && b.effects.iter().any(|e| matches!(e, Effect::Render(_)))
        }));
        // A runtime snapshot must also start the tone.
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::StartTone)));
    }

    #[test]
    fn boot_with_firing_alarm_renders_once_not_twice() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let renders = batches
            .iter()
            .filter(|b| b.render_generation.is_some())
            .count();
        assert_eq!(
            renders, 1,
            "boot + alarm path must produce exactly one render"
        );
    }

    #[test]
    fn program_success_enters_armed() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        let prog = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            })
            .map(|b| b.operation_id)
            .unwrap();
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: prog,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Armed { alarm_id: 1 }
        ));
    }

    #[test]
    fn disable_success_leaves_disarmed() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![], Some(dt(9, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        let dis = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::DisableRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: dis,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        assert_eq!(state.alarm_runtime, AlarmRuntimeState::Disarmed);
    }

    #[test]
    fn duplicate_snapshot_during_firing_is_idempotent() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        // First snapshot enters Firing and captures the ack op id.
        let _ = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        let first_ack_op = match &state.alarm_runtime {
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { operation_id },
                ..
            } => *operation_id,
            _ => OperationId(0),
        };
        // A duplicate snapshot for the same asserted AF must not overwrite
        // the in-flight commit or re-enter anything.
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert!(batches.is_empty());
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { operation_id },
                ..
            } if operation_id == first_ack_op
        ));
        assert_eq!(state.screen, Screen::AlarmRinging);
    }

    #[test]
    fn ack_failure_retries_after_backoff() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        update(&mut state, Event::Boot(snapshot));
        let ack_op = match &state.alarm_runtime {
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { operation_id },
                ..
            } => *operation_id,
            _ => OperationId(0),
        };
        // ACK fails -> retry scheduled.
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: ack_op,
                render_generation: None,
                error: EffectError::Ack("i2c".into()),
            }),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                ack: CommitState::Failed { .. },
                ..
            }
        ));
        // After the backoff window passes, a Tick re-issues ACK with a fresh
        // op id and resets the commit to InFlight.
        let now = dt(9, 2); // fired at minute 540, +1 backoff -> due by 541
        let batches = update(&mut state, Event::Tick(now));
        // Still Firing; a fresh in-flight ack is generated.
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { .. },
                ..
            }
        ));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
    }

    // ---- round 3 findings: residue-ACK ordering + concurrent retry -------

    /// AF set with no matching alarm (residue): the first response must be
    /// ONLY a confirmable ACK (never a simultaneous RTC program), and the
    /// register is reprogrammed after `AckDone`.
    #[test]
    fn residue_ack_before_any_rtc_program() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true);
        let batches = update(&mut state, Event::Boot(snapshot));

        // Only an ACK is emitted - no ProgramRtcAlarm / DisableRtcAlarm yet.
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));

        // The ACK carries a real op id (no OperationId(0) residue ACK).
        let ack_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();
        assert_ne!(ack_op, OperationId(0));

        // After AckDone, the RTC register is reprogrammed.
        let batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: ack_op,
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));
    }

    /// A failed residue ACK must not open the alarm interrupt (no RTC
    /// program), and is retried after the backoff window.
    #[test]
    fn residue_ack_failure_retries_then_programs() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true);
        let batches = update(&mut state, Event::Boot(snapshot));
        let ack_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();

        // ACK fails: no RTC program is generated.
        let batches = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: ack_op,
                render_generation: None,
                error: EffectError::Ack("i2c".into()),
            }),
        );
        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));

        // After the backoff window, a Tick re-issues the ACK.
        let batches = update(&mut state, Event::Tick(dt(9, 2)));
        let retry_ack = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id);
        assert!(retry_ack.is_some());
        assert_ne!(retry_ack.unwrap(), ack_op);
        // No RTC program yet - it waits for the ACK to succeed.
        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));
    }

    /// ACK + persistence both failing must keep BOTH retries (the single
    /// retry-slot bug) so a later rearm can still complete.
    #[test]
    fn concurrent_ack_and_persist_failure_both_retained() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        update(&mut state, Event::Boot(snapshot));
        let (ack_op, persist_op) = match &state.alarm_runtime {
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { operation_id: a },
                persistence: CommitState::InFlight { operation_id: p },
                ..
            } => (*a, *p),
            _ => (OperationId(0), OperationId(0)),
        };

        // Both fail independently.
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: ack_op,
                render_generation: None,
                error: EffectError::Ack("i2c".into()),
            }),
        );
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                error: EffectError::Persist("nvs".into()),
            }),
        );

        // Both retries are retained (two distinct actions), neither is
        // overwritten by the other.
        assert_eq!(state.retries.len(), 2);

        // A single Tick past the backoff processes both.
        let batches = update(&mut state, Event::Tick(dt(9, 2)));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
        // The persist retry re-issues with the current alarm list (the daily
        // alarm is still stored).
        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::PersistAlarms(list) if list == &state.alarms.alarms))
        }));
        // Both commits reset to InFlight.
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { .. },
                persistence: CommitState::InFlight { .. },
                ..
            }
        ));
    }
    // ---- round 4 finding: residue-ACK barrier idempotency -----------------

    /// While a residue ACK is pending, a duplicate `RtcAlarmSnapshotReady`
    /// must be ignored: it must not regenerate an ACK op id, must not ring
    /// even if the wall-clock now crosses an alarm's matching minute, and
    /// the pending op id must remain stable.
    #[test]
    fn residue_ack_pending_blocks_duplicate_snapshot() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];

        let first_batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(8, 59),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        let first_ack_op = first_batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();
        assert_ne!(first_ack_op, OperationId(0));
        assert!(state.pending_residue_ack.is_some());

        // Duplicate snapshot at the SAME minute: empty, no op id change.
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(8, 59),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert!(batches.is_empty());
        assert_eq!(state.pending_residue_ack, Some(first_ack_op));
        assert_eq!(state.alarm_runtime, AlarmRuntimeState::Disarmed);

        // Snapshot at the alarm's MATCHING minute (9:00): the same AF is
        // still asserted, but the barrier holds.
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert!(batches.is_empty());
        assert_eq!(state.screen, Screen::Home);
        assert!(matches!(state.alarm_runtime, AlarmRuntimeState::Disarmed));
        assert_eq!(state.pending_residue_ack, Some(first_ack_op));
    }

    /// A stale `AckDone` with a different op id must NOT clear the pending
    /// residue ACK.
    #[test]
    fn stale_ackdone_does_not_clear_pending_residue() {
        let mut state = AppState::default();
        let first_batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(8, 59),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        let first_ack_op = first_batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();

        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: OperationId(999),
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        assert_eq!(state.pending_residue_ack, Some(first_ack_op));
        assert_eq!(state.alarm_runtime, AlarmRuntimeState::Disarmed);

        // The matching AckDone clears it and re-programs the RTC.
        let batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: first_ack_op,
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        assert_eq!(state.pending_residue_ack, None);
        // After AckDone, the RTC is reprogrammed (ProgramRtcAlarm) or
        // disabled (DisableRtcAlarm when no alarms are stored).
        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm))
        }));
    }

    /// A residue ACK retry re-records the barrier: while the retry ACK is
    /// in flight, a duplicate snapshot must still be ignored.
    #[test]
    fn residue_ack_retry_reestablishes_barrier() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];

        let first_batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(8, 59),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        let first_ack_op = first_batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();

        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: first_ack_op,
                render_generation: None,
                error: EffectError::Ack("i2c".into()),
            }),
        );
        assert_eq!(state.pending_residue_ack, None);

        // Retry re-issues the ACK and re-records pending_residue_ack.
        let batches = update(&mut state, Event::Tick(dt(9, 1)));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
        assert!(state.pending_residue_ack.is_some());

        // While the retry ACK is in flight, a duplicate snapshot is ignored.
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert!(batches.is_empty());
        assert_eq!(state.alarm_runtime, AlarmRuntimeState::Disarmed);
    }

    /// Round-8 host integration test: the full alarm lifecycle through the
    /// state machine - boot into Firing, dismiss to WaitingForRearm, let
    /// ACK + persistence complete, then a cross-minute Tick re-arms the
    /// next alarm. Asserts ACK / persist / rearm each happen exactly once
    /// and no stage is skipped or doubled. This is the logic that the
    /// firmware AppRunner + ring_screen handoff relies on; it is testable
    /// on the host because `update` is pure.
    #[test]
    fn alarm_lifecycle_ring_dismiss_rearm_each_once() {
        let mut state = AppState::default();
        // Boot at 9:00 with a Daily 9:00 alarm, AF set + AIE enabled.
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let boot_batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        // ACK and persist each occur exactly once across the whole batch.
        let ack_count = boot_batches
            .iter()
            .flat_map(|b| b.effects.iter())
            .filter(|e| **e == Effect::AcknowledgeRtcAlarm)
            .count();
        let persist_count = boot_batches
            .iter()
            .flat_map(|b| b.effects.iter())
            .filter(|e| matches!(e, Effect::PersistAlarms(_)))
            .count();
        assert_eq!(ack_count, 1, "ACK must fire exactly once on ring");
        assert_eq!(persist_count, 1, "persist must fire exactly once on ring");

        // Dismiss via ENTER.
        let dismiss_batches = update(&mut state, Event::Button(ButtonEvent::Pressed));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        assert_eq!(state.screen, Screen::Home);
        let stop_count = dismiss_batches
            .iter()
            .flat_map(|b| b.effects.iter())
            .filter(|e| **e == Effect::StopTone)
            .count();
        assert_eq!(stop_count, 1, "StopTone must fire exactly once on dismiss");

        // Grab the in-flight ACK + persist op ids.
        let (ack_op, persist_op) = match &state.alarm_runtime {
            AlarmRuntimeState::WaitingForRearm {
                ack, persistence, ..
            } => (
                match ack {
                    CommitState::InFlight { operation_id } => *operation_id,
                    _ => OperationId(0),
                },
                match persistence {
                    CommitState::InFlight { operation_id } => *operation_id,
                    _ => OperationId(0),
                },
            ),
            _ => panic!("expected WaitingForRearm"),
        };

        // Persist completes.
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(1),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        // ACK completes.
        let _rearm_batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(2),
                operation_id: ack_op,
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        // Still in WaitingForRearm until the minute advances.
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));

        // Cross-minute Tick re-arms: the Daily alarm is still enabled, so
        // the state machine must emit exactly one ProgramRtcAlarm. The
        // runtime goes Disarmed (program in flight via pending_rtc) and
        // only becomes Armed after the RtcProgrammed completion arrives.
        let tick_batches = update(&mut state, Event::Tick(dt(9, 1)));
        let program_count = tick_batches
            .iter()
            .flat_map(|b| b.effects.iter())
            .filter(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            .count();
        assert_eq!(
            program_count, 1,
            "rearm must fire exactly one ProgramRtcAlarm, got {tick_batches:?} state={:?}",
            state.alarm_runtime
        );
        let program_op = tick_batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            })
            .map(|b| b.operation_id)
            .expect("program batch present");

        // RTC program completes -> Armed with the Daily alarm id.
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(3),
                operation_id: program_op,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        assert!(
            matches!(
                state.alarm_runtime,
                AlarmRuntimeState::Armed { alarm_id: 1 }
            ),
            "Daily alarm still enabled: must be Armed {{ alarm_id: 1 }}, got {:?}",
            state.alarm_runtime
        );
    }
}
