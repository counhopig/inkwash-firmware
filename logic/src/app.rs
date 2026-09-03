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
use crate::alarm_schedule::{maintenance_wakeup_delay, next_due, Repeat, StoredAlarm};
use crate::button_event::{ButtonEvent, ButtonId};
use crate::datetime::{days_in_month, DateTime};
use crate::device_config::{DeviceConfig, WifiCreds};
use crate::inbox_item::InboxItem;
use crate::protocol::{Channel, ControlReply, ControlRequest, Reply};
use crate::todo::{Importance, Todo};
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
    Navigation {
        selected: usize,
        /// Which screen the drawer was opened from: cancelling the drawer
        /// (long ENTER) restores it, and selecting a still-legacy
        /// destination returns to it after the executor's page wedge.
        origin: NavOrigin,
    },
    Settings {
        selected: usize,
    },
    /// The SYNC INTERVAL picker (Stage 4): choose from the five fixed sync
    /// intervals (1/5/10/30/60 min). The SM owns the row cursor; ENTER
    /// confirms (emits Effect::SetSyncInterval, fire-and-forget NVS write)
    /// and returns to Settings; long ENTER cancels.
    SyncIntervalPick {
        selected: usize,
    },
    Calendar(CalendarState),
    /// The read-only week view (Stage 4): the Sun-Sat week containing the
    /// opened day, one column per day with each day's open todos. ENTER on a
    /// Calendar day opens it; any button closes it back to the Calendar
    /// screen (the cursor stays on the opened day).
    WeekView {
        year: u16,
        month: u8,
        day: u8,
    },
    AlarmList {
        selected: usize,
    },
    /// The ADD-ALARM editor (Stage 4): a two-stage number picker (hour,
    /// then minute) driven by the state machine - UP/DOWN step the value
    /// (wrapping within the stage's range), ENTER advances the stage,
    /// long ENTER cancels back to the AlarmList ADD row. Completing the
    /// minute stage appends the new Daily alarm (confirmable persist).
    AlarmAdd(AlarmAddState),
    TodoList {
        selected: usize,
    },
    Inbox {
        selected: usize,
    },
    /// The inbox item detail (Stage 4): title + body of the selected item,
    /// drawn by the executor from the store (carries only the index). ENTER
    /// on an Inbox row opens it (and marks it read); any button closes it
    /// back to the list.
    InboxItem {
        index: usize,
    },
    AlarmRinging,
    BlePairing(BlePairingState),
}

/// The screen a navigation drawer was opened from. Every drawer origin is
/// itself a state-machine screen (content pages are still legacy wedges and
/// cannot host a drawer yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavOrigin {
    Home,
    Settings,
    AlarmList,
    TodoList,
    Inbox,
    Calendar,
}

impl Screen {
    /// Which render surface this screen is drawn on.
    ///
    /// Stage-4/5 transitional projection: a `RenderRequest` must tell the
    /// executor *what* to draw (and which panel rect to refresh), because
    /// the executor draws from state, not from call-site rectangles. Until
    /// the remaining content pages migrate to `Screen` states (Stage 4) and
    /// the ViewModel diff replaces intents (Stage 5), every screen that
    /// still renders through a legacy blocking page maps to the Home
    /// surface, and only screens the executor can actually draw get their
    /// own view:
    ///
    /// - `Home` - the idle home canvas (clock + cards).
    /// - `Navigation` - Home canvas with the GO TO drawer overlay.
    /// - `Settings` - the settings list (rows browsed in the state
    ///   machine; row actions are deferred executor wedges).
    /// - `AlarmList` - the alarm list (rows browsed in the state machine;
    ///   ENTER toggles an alarm's enabled flag through a confirmable edit;
    ///   the "+ ADD ALARM" row is a deferred executor wedge).
    ///
    /// `AlarmRinging` maps to Home: the ringing frame is owned by the
    /// legacy `ring_screen`, which draws over whatever the SM render
    /// produced before the EPD request lands.
    pub fn render_view(&self) -> RenderView {
        match self {
            Screen::Navigation { selected, .. } => RenderView::Navigation {
                selected: *selected,
            },
            Screen::Settings { selected } => RenderView::Settings {
                selected: *selected,
            },
            Screen::SyncIntervalPick { selected } => RenderView::SyncInterval {
                selected: *selected,
            },
            Screen::AlarmList { selected } => RenderView::AlarmList {
                selected: *selected,
            },
            Screen::TodoList { selected } => RenderView::TodoList {
                selected: *selected,
            },
            Screen::Inbox { selected } => RenderView::Inbox {
                selected: *selected,
            },
            Screen::InboxItem { index } => RenderView::InboxItem { index: *index },
            Screen::Calendar(CalendarState {
                year,
                month,
                selected_day,
            }) => RenderView::Calendar {
                year: *year,
                month: *month,
                selected_day: *selected_day,
            },
            Screen::WeekView { year, month, day } => RenderView::WeekView {
                year: *year,
                month: *month,
                day: *day,
            },
            Screen::AlarmAdd(AlarmAddState { stage, value, .. }) => RenderView::NumberPick {
                stage: *stage,
                value: *value,
            },
            Screen::BlePairing(_) => RenderView::BlePairing,
            Screen::Home | Screen::AlarmRinging => RenderView::Home,
        }
    }
}

/// Number of global navigation destinations (HOME/CALENDAR/INBOX/ALARMS/
/// TODOS/SETTINGS - see the firmware's `NAV_DESTINATIONS`).
pub const NAV_DESTINATION_COUNT: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarState {
    /// Displayed year/month: always the *current* month (the grid has no
    /// month navigation in v1 - it tracks the clock, like the legacy
    /// browse_page). Refreshed on each cross-month Tick.
    pub year: u16,
    pub month: u8,
    /// The cursor day (1..=days_in_month(year, month)); ENTER opens that
    /// day's week view.
    pub selected_day: u8,
}

/// The ADD-ALARM editor's two picker stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddStage {
    Hour,
    Minute,
}

/// The ADD-ALARM editor cursor (Stage 4): which stage is active, the
/// currently stepped value (0-23 for Hour, 0-59 for Minute), and the hour
/// locked in when the Hour stage completed (so the Minute stage can finish
/// with both).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmAddState {
    pub stage: AddStage,
    pub hour: u8,
    pub value: u8,
}

impl Default for AlarmAddState {
    fn default() -> Self {
        Self {
            stage: AddStage::Hour,
            hour: 0,
            value: 0,
        }
    }
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
    /// Apply merged sync data (alarms/todos/inbox + read acks) in one
    /// confirmable write. The executor saves each store, clears the dirty
    /// sets, acks inbox reads and persists the ETag; the machine replies to
    /// the requesting transport only after the apply confirms.
    ApplySyncedData(SyncedData),
    /// Invalidate the stale sync ETag after a server change (SetServer).
    /// Separate from `PersistSyncMetadata` because that effect's `etag:
    /// None` means "don't touch"; clearing needs its own confirmable op.
    ClearSyncEtag,
    /// Persist the UTC-offset minutes (SetTimezone's NVS write).
    PersistTimezone(i16),
    /// Write the RTC clock to an absolute time (SetTimezone's hardware
    /// write; confirmable through the RTC executor).
    WriteRtcTime(DateTime),
    ProgramRtcAlarm(AlarmRegs),
    DisableRtcAlarm,
    AcknowledgeRtcAlarm,
    /// Confirmable single-alarm enabled-toggle from the AlarmList screen
    /// (Stage 4): save the new list and mark `toggled_id` dirty (two-way
    /// sync contract). Completes as `Persisted(Alarms)`; the machine's
    /// pending-alarm-list-edit gate matches the op id exactly, so a stale
    /// completion cannot release a newer edit.
    PersistAlarmToggle {
        alarms: Vec<StoredAlarm>,
        toggled_id: u8,
    },
    /// Confirmable single-todo edit from the TodoList screen (Stage 4):
    /// save the new list and mark `edited_id` dirty (two-way sync
    /// contract). Completions feed back as `Persisted(Todos)`; the
    /// machine's pending-todo-list-edit gate matches the op id exactly, so
    /// a stale completion cannot release a newer edit.
    PersistTodoEdit {
        todos: Vec<Todo>,
        edited_id: u8,
    },
    StartSync(SyncRequest),
    /// Ask the network executor to verify + save Wi-Fi credentials (it owns
    /// the Wi-Fi driver). Completes via `Event::SetWifiCompleted`.
    StartSetWifi(WifiCreds),
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
    /// The user pressed ENTER on the Settings BLE PAIRING row (index 2) -
    /// the last row still behind a legacy blocking radio screen. The state
    /// machine stays on `Screen::Settings` while the executor runs the wedge
    /// (deferred post-pump); a Full Settings render is re-issued when the
    /// wedge returns.
    OpenBlePairingScreen,
    /// The user opened an inbox item (ENTER on an Inbox row). Fire-and-forget
    /// local persistence of the read mark: the executor marks `seq` read and
    /// adds it to the pending-read set (two-way sync uploads it later). The
    /// state machine optimistically set the item's `read` flag when the
    /// detail screen opened; there is no confirmable completion to track.
    MarkInboxRead {
        /// The inbox item `seq` to mark read (from `state.inbox.items`).
        seq: u64,
    },
    /// The user confirmed a SYNC INTERVAL choice on the SM picker (Stage 4).
    /// Fire-and-forget persistence of the sync interval (minutes); the
    /// executor writes it to NVS. The state machine does not track the
    /// value itself - the firmware's sync scheduler owns the cadence.
    SetSyncInterval {
        /// Interval in minutes (one of the fixed five: 1/5/10/30/60).
        minutes: u16,
    },
}

/// Which visible surface a render request targets. The renderer draws this
/// surface (not a call-site rectangle) and refreshes the panel rect the
/// surface maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderView {
    /// The idle home canvas (clock + cards), with no overlay.
    Home,
    /// Home canvas plus the GO TO drawer overlay (Stage 4, slice 1 - the
    /// only SM-drawn overlay today). Carries the drawer's selected row so
    /// the executor can draw the overlay from state.
    Navigation { selected: usize },
    /// The settings list (Stage 4): rows browsed in the state machine,
    /// row actions deferred to executor wedges. Carries the selected row.
    Settings { selected: usize },
    /// The alarm list (Stage 4): rows browsed in the state machine, ENTER
    /// toggles a row's enabled flag (confirmable SM edit); the trailing
    /// "+ ADD ALARM" row is a deferred executor wedge. Carries the selected
    /// row; the executor loads the rows from the store.
    AlarmList { selected: usize },
    /// The todo list (Stage 4): rows browsed in the state machine; ENTER
    /// toggles a row's done flag and long ENTER cycles its importance
    /// (both confirmable SM edits). Carries the selected row.
    TodoList { selected: usize },
    /// The inbox list (Stage 4): rows browsed in the state machine, ENTER
    /// opens an item's detail (an SM screen). Carries the selected row; the
    /// executor loads the rows from the store.
    Inbox { selected: usize },
    /// The inbox item detail (Stage 4): title + body of the selected item,
    /// drawn by the executor from the store (carries only the index). ENTER
    /// on an Inbox row opens it (and marks it read); any button closes it
    /// back to the list.
    InboxItem { index: usize },
    /// The calendar month grid (Stage 4): a read-only grid of the current
    /// month with a day cursor the state machine moves; ENTER opens the
    /// day's week view through a deferred legacy wedge. Carries the year /
    /// month and selected day so the executor can draw the grid from state
    /// (day marks are read from the todo store by the executor).
    Calendar {
        year: u16,
        month: u8,
        selected_day: u8,
    },
    /// The week view (Stage 4): the Sun-Sat week containing the opened day,
    /// one column per day with each day's open todos read from the todo
    /// store by the executor. Full refresh on open/close.
    WeekView { year: u16, month: u8, day: u8 },
    /// The ADD-ALARM number picker (Stage 4): a big digit stepped by
    /// UP/DOWN (wrapping). Carries the active stage (the executor maps it
    /// to the title + range) and the current value.
    NumberPick { stage: AddStage, value: u8 },
    /// The SYNC INTERVAL picker list (Stage 4): five fixed options, row
    /// cursor moved by the SM, ENTER confirms. Carries the selected row.
    SyncInterval { selected: usize },
    /// The BLE pairing session (Stage 4): the pairing-instructions screen.
    /// The SM owns the lifecycle (events drive the phase; any button exits
    /// back to Settings); the executor draws the instructions canvas. The
    /// phase-specific visuals stay with the radio layer until the NimBLE
    /// event wiring lands (the SM phase transitions are locked regardless).
    BlePairing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub generation: RenderGeneration,
    pub intent: RenderIntent,
    /// The surface to draw. Stage-5 will replace `intent` with a diffed
    /// RenderPlan; `view` stays as the renderer's "what screen am I
    /// drawing" input until every screen is SM-drawn.
    pub view: RenderView,
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
    /// A confirmed `ApplySyncedData` (merged writes landed).
    SyncApply,
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
        /// Merged server data the state machine applies (persist is
        /// ordered by the machine; the network task never writes NVS).
        data: SyncedData,
    },
    Failed(String),
}

/// Merged server state returned by a successful sync, applied by the state
/// machine through `Effect::ApplySyncedData` (the network task only
/// transports; it never writes NVS, RTC, UI or the scheduler).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedData {
    pub alarms: Vec<StoredAlarm>,
    pub todos: Vec<Todo>,
    pub inbox: Vec<InboxItem>,
    /// Inbox sequence numbers the server confirmed as read (executor acks
    /// them in the same apply).
    pub inbox_read_acked: Vec<u64>,
    pub inbox_truncated: bool,
    pub etag: Option<String>,
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
    /// A `StartSetWifi` verification+save finished (Ok when the credentials
    /// were verified and persisted; Err carries the failure message).
    SetWifiCompleted(Result<(), String>),
    /// The wall-clock full-sync boundary is due (the event-collection layer
    /// detected the minute crossed the interval boundary; it only reports
    /// the fact - the state machine decides whether a sync may start).
    SyncBoundaryDue,
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
    /// An in-flight AlarmList enabled-toggle (Stage 4). The screen
    /// optimistically flips the row's `enabled`; the matching
    /// `Persisted(Alarms)` completion confirms it (and the executor has
    /// marked it dirty). A failure rolls the row back to its previous
    /// value. One edit at a time: while this is Some, another row ENTER is
    /// ignored.
    pending_alarm_list_edit: Option<PendingAlarmListEdit>,
    /// An in-flight ADD-ALARM append (Stage 4): the new alarm is appended
    /// optimistically and the list persists confirmably (one add in flight
    /// at a time). A failed persist pops the appended alarm back.
    pending_alarm_add: Option<OperationId>,
    /// An in-flight TodoList row edit (Stage 4): ENTER toggled `done` or
    /// long ENTER cycled `importance` optimistically. The matching
    /// `Persisted(Todos)` completion confirms it (executor marked it
    /// dirty); a failure rolls the row back to the captured previous
    /// state. One edit at a time.
    pending_todo_list_edit: Option<PendingTodoListEdit>,
    retries: Vec<RetryPending>,
}

/// An in-flight single-row edit from the TodoList screen. Captures the
/// row's `done` + `importance` before the optimistic change so a failed
/// persist can roll both back (the two edit kinds touch different fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingTodoListEdit {
    operation_id: OperationId,
    index: usize,
    previous_done: bool,
    previous_importance: Importance,
}

/// An in-flight single-row enabled-toggle from the AlarmList screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingAlarmListEdit {
    operation_id: OperationId,
    /// Row index in `alarms.alarms` whose `enabled` the toggle flipped.
    index: usize,
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
            pending_alarm_list_edit: None,
            pending_alarm_add: None,
            pending_todo_list_edit: None,
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
        Event::SyncCompleted(result) => transition_sync_completed(state, result),
        Event::SetWifiCompleted(result) => transition_set_wifi_completed(state, result),
        Event::SyncBoundaryDue => transition_sync_boundary_due(state),
        Event::BlePairingStarted => transition_ble_pairing_started(state),
        Event::BlePairingSucceeded(result) => transition_ble_pairing_succeeded(state, result),
        Event::BlePairingFailed(failure) => transition_ble_pairing_failed(state, failure),
        Event::BleDisconnected => transition_ble_disconnected(state),
        // Remaining events are wired in later migration steps. They leave
        // state unchanged rather than guessing.
        _ => vec![],
    }
}

/// The wall-clock full-sync boundary is due. The event-collection layer
/// reported the fact; the machine decides whether a sync may start: it
/// must be idle (single network op) and wifi + server must be configured.
/// A scheduled sync has no transport pending reply - its completion feeds
/// back through `SyncCompleted` and applies the merged data with no reply
/// to send.
fn transition_sync_boundary_due(state: &mut AppState) -> Vec<EffectBatch> {
    if state.sync != SyncState::Idle {
        return vec![];
    }
    if !state.connectivity.wifi_configured || !state.connectivity.server_configured {
        return vec![];
    }
    let Some(now) = state.clock.now else {
        return vec![];
    };
    let op = state.next_operation_id();
    state.sync = SyncState::Running { request_id: op };
    vec![batch(
        state,
        op,
        FailurePolicy::Continue,
        vec![Effect::StartSync(SyncRequest { now })],
    )]
}

/// BLE pairing phase transitions (Stage 4 wiring of the pre-existing model):
/// the firmware's radio layer reports pairing lifecycle facts through these
/// events while a `Screen::BlePairing` session is current. Each updates the
/// session phase and re-renders; reaching a terminal phase also stops the
/// radio (the executor owns the BLE driver teardown).
fn transition_ble_pairing_started(state: &mut AppState) -> Vec<EffectBatch> {
    if !matches!(state.screen, Screen::BlePairing(_)) {
        return vec![];
    }
    if let Screen::BlePairing(st) = &mut state.screen {
        st.phase = BlePairingPhase::Pairing;
    }
    state.render_generation = state.render_generation.next();
    vec![render_batch_with_intent(state, RenderIntent::Full)]
}

fn transition_ble_pairing_succeeded(
    state: &mut AppState,
    result: BlePairingResult,
) -> Vec<EffectBatch> {
    if !matches!(state.screen, Screen::BlePairing(_)) {
        return vec![];
    }
    if let Screen::BlePairing(st) = &mut state.screen {
        st.phase = BlePairingPhase::Success;
    }
    let _ = result;
    state.render_generation = state.render_generation.next();
    vec![
        batch(
            state,
            OperationId(0),
            FailurePolicy::Continue,
            vec![Effect::StopBlePairing],
        ),
        render_batch_with_intent(state, RenderIntent::Full),
    ]
}

fn transition_ble_pairing_failed(
    state: &mut AppState,
    failure: BlePairingFailure,
) -> Vec<EffectBatch> {
    if !matches!(state.screen, Screen::BlePairing(_)) {
        return vec![];
    }
    if let Screen::BlePairing(st) = &mut state.screen {
        st.phase = BlePairingPhase::Failure(failure.message);
    }
    state.render_generation = state.render_generation.next();
    vec![
        batch(
            state,
            OperationId(0),
            FailurePolicy::Continue,
            vec![Effect::StopBlePairing],
        ),
        render_batch_with_intent(state, RenderIntent::Full),
    ]
}

fn transition_ble_disconnected(state: &mut AppState) -> Vec<EffectBatch> {
    if !matches!(state.screen, Screen::BlePairing(_)) {
        return vec![];
    }
    if let Screen::BlePairing(st) = &mut state.screen {
        st.phase = BlePairingPhase::Waiting;
    }
    state.render_generation = state.render_generation.next();
    vec![render_batch_with_intent(state, RenderIntent::Full)]
}

/// BlePairing screen buttons (Stage 4): any press closes the pairing
/// session back to the Settings BLE PAIRING row (mirroring the legacy
/// modal's "HOLD ENTER BACK"). Releases are ignored.
fn transition_ble_pairing_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    match button {
        ButtonEvent::Pressed(ButtonId::Enter)
        | ButtonEvent::LongPressed(ButtonId::Enter)
        | ButtonEvent::Pressed(ButtonId::Up)
        | ButtonEvent::Pressed(ButtonId::Down)
        | ButtonEvent::LongPressed(ButtonId::Up)
        | ButtonEvent::LongPressed(ButtonId::Down) => {
            state.screen = Screen::Settings {
                selected: SETTINGS_BLE_PAIRING_ROW,
            };
            state.render_generation = state.render_generation.next();
            vec![
                batch(
                    state,
                    OperationId(0),
                    FailurePolicy::Continue,
                    vec![Effect::StopBlePairing],
                ),
                render_batch_with_intent(state, RenderIntent::Full),
            ]
        }
        _ => vec![],
    }
}

/// A completed network sync resolves the running sync (single-flight
/// arbitration) and drives the SyncNow pending reply. On success the state
/// machine adopts the merged data and emits a confirmable
/// `ApplySyncedData`; the transport's Ok reply is sent only when the apply
/// confirms (persist failure must not reply success early). On failure it
/// replies Error immediately.
fn transition_sync_completed(state: &mut AppState, result: SyncResult) -> Vec<EffectBatch> {
    let mut batches = Vec::new();
    match result {
        SyncResult::Ok { data } => {
            let apply_op = state.next_operation_id();
            // Adopt the merged data into state (the state machine is the
            // single owner of the alarm/todo/inbox lists) and persist it via
            // one confirmable apply - for BOTH on-demand and scheduled
            // syncs, so the network task never writes NVS itself.
            state.alarms.alarms = data.alarms.clone();
            state.todos.todos = data.todos.clone();
            state.inbox.items = data.inbox.clone();
            // A successful sync proves the network was up; record the
            // last-known connectivity fact (GetStatus reports it).
            state.connectivity.wifi_connected = true;
            // Release the single-flight lock; the sync itself is done, only
            // its apply (below) remains confirmable.
            state.sync = SyncState::Idle;
            // If a transport's SyncNow slot awaits this sync, re-point it at
            // the apply op so the final Ok waits for the apply confirm.
            let _ = [Channel::Usb, Channel::Ble]
                .iter()
                .copied()
                .any(|channel| repoint_sync_to_apply(state, channel, apply_op));
            // Always apply the merged data (a scheduled sync has no pending
            // transport; its `Persisted(SyncApply)` completion is observed
            // by the executor path with no reply to send).
            batches.push(batch(
                state,
                apply_op,
                FailurePolicy::AbortBatch,
                vec![Effect::ApplySyncedData(data)],
            ));
        }
        SyncResult::Failed(message) => {
            state.sync = SyncState::Idle;
            let reply = Reply::Error { message };
            for channel in [Channel::Usb, Channel::Ble] {
                if let Some((found_channel, resolved)) =
                    resolve_sync_completion(state, channel, &reply)
                {
                    batches.push(reply_batch(state, found_channel, resolved));
                }
            }
        }
    }
    batches
}

fn transition_set_wifi_completed(
    state: &mut AppState,
    result: Result<(), String>,
) -> Vec<EffectBatch> {
    let mut batches = Vec::new();
    for channel in [Channel::Usb, Channel::Ble] {
        let Some((found_channel, creds)) = take_set_wifi_pending(state, channel) else {
            continue;
        };
        match &result {
            Ok(()) => {
                // The network executor verified + persisted the
                // credentials: apply the status-visible wifi facts (SSID +
                // has-password; the password value never enters AppState).
                state.config.wifi_ssid = Some(creds.ssid);
                state.config.wifi_has_password = !creds.password.is_empty();
                state.connectivity.wifi_configured = state.config.wifi_configured();
                batches.push(reply_batch(state, found_channel, Reply::Ok));
            }
            Err(message) => {
                let msg = format!("Connection verification failed: {message}");
                batches.push(reply_batch(
                    state,
                    found_channel,
                    Reply::Error { message: msg },
                ));
            }
        }
    }
    batches
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
    if matches!(button, ButtonEvent::Pressed(ButtonId::Enter))
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
    } else if matches!(
        state.screen,
        Screen::Home
            | Screen::Navigation { .. }
            | Screen::Settings { .. }
            | Screen::SyncIntervalPick { .. }
            | Screen::AlarmList { .. }
            | Screen::AlarmAdd(_)
            | Screen::TodoList { .. }
            | Screen::Inbox { .. }
            | Screen::InboxItem { .. }
            | Screen::Calendar(_)
            | Screen::WeekView { .. }
            | Screen::BlePairing(_)
    ) {
        transition_nav_button(state, button)
    } else {
        vec![]
    }
}

/// Returns the drawer's home-destination index for `origin` (the row
/// highlighted when the drawer opens = "you are here", matching the legacy
/// pick_navigation pre-selection).
fn nav_home_index(origin: NavOrigin) -> usize {
    match origin {
        NavOrigin::Home => 0,
        // The drawer over Settings highlights SETTINGS (5), so a no-move
        // ENTER stays on Settings (legacy pick_navigation behavior).
        NavOrigin::Settings => NAV_DESTINATION_COUNT - 1,
        // The drawer over the alarm list highlights ALARMS (3).
        NavOrigin::AlarmList => 3,
        // The drawer over the todo list highlights TODOS (4).
        NavOrigin::TodoList => 4,
        // The drawer over the inbox list highlights INBOX (2).
        NavOrigin::Inbox => 2,
        // The drawer over the calendar grid highlights CALENDAR (1).
        NavOrigin::Calendar => 1,
    }
}

/// The screen a closed drawer leaves behind when the selection does not
/// open a sub-screen destination (cancel, HOME, or the origin's own row).
/// With every origin a state-machine screen, closing the drawer returns to
/// the plain root of the navigation tree: Home (Settings keeps its own
/// back-to-Home path).
fn nav_origin_screen(_origin: NavOrigin) -> Screen {
    // Every origin is a state-machine screen; closing the drawer (cancel /
    // HOME / the origin's own row) returns to the plain root Home. Settings,
    // AlarmList, TodoList and Inbox keep their own back / drawer flows.
    Screen::Home
}

/// Navigation-drawer state machine (Stage 4): a long UP/DOWN opens the
/// drawer from a state-machine screen (Home, or Settings once that screen
/// exists); inside the drawer, UP/DOWN move the selection (wrapping), long
/// UP/DOWN jump to the first/last destination, long ENTER cancels back to
/// the origin, and ENTER selects a destination. Every destination
/// (HOME/CALENDAR/INBOX/ALARMS/TODOS/SETTINGS) is a state-machine screen
/// now; a drawer over a screen highlights that screen's row, so a no-move
/// ENTER stays there. The Settings BLE Pairing row emits
/// `OpenBlePairingScreen`; the
/// Settings screen itself stays current while the executor wedge runs.
fn transition_nav_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    match &state.screen {
        Screen::Navigation { selected, origin } => {
            let cur = *selected;
            let origin = *origin;
            let next_screen = match button {
                ButtonEvent::Pressed(ButtonId::Up) => Some(Screen::Navigation {
                    selected: if cur == 0 {
                        NAV_DESTINATION_COUNT - 1
                    } else {
                        cur - 1
                    },
                    origin,
                }),
                ButtonEvent::Pressed(ButtonId::Down) => Some(Screen::Navigation {
                    selected: (cur + 1) % NAV_DESTINATION_COUNT,
                    origin,
                }),
                ButtonEvent::LongPressed(ButtonId::Up) => Some(Screen::Navigation {
                    selected: 0,
                    origin,
                }),
                ButtonEvent::LongPressed(ButtonId::Down) => Some(Screen::Navigation {
                    selected: NAV_DESTINATION_COUNT - 1,
                    origin,
                }),
                ButtonEvent::LongPressed(ButtonId::Enter) => Some(nav_origin_screen(origin)),
                ButtonEvent::Pressed(ButtonId::Enter) => {
                    // Selecting a destination closes the drawer. Every
                    // destination is an SM screen now: HOME (0), CALENDAR
                    // (1), INBOX (2), ALARMS (3), TODOS (4), SETTINGS (5).
                    return match cur {
                        0 => {
                            state.screen = nav_origin_screen(origin);
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        1 => {
                            // Open the calendar grid: always the current
                            // month, cursor on today (matching the legacy
                            // browse_page entry state).
                            let (year, month, day) = state
                                .clock
                                .now
                                .map(|dt| (dt.year, dt.month, dt.day))
                                .unwrap_or((1970, 1, 1));
                            state.screen = Screen::Calendar(CalendarState {
                                year,
                                month,
                                selected_day: day.min(days_in_month(year, month)).max(1),
                            });
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        2 => {
                            state.screen = Screen::Inbox { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        3 => {
                            state.screen = Screen::AlarmList { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        4 => {
                            state.screen = Screen::TodoList { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        5 => {
                            state.screen = Screen::Settings { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch_with_intent(state, RenderIntent::Full)]
                        }
                        _ => unreachable!(),
                    };
                }
                _ => None,
            };
            if let Some(next) = next_screen {
                // A still-drawer move only changes the overlay (cheap
                // NAV_BAR partial refresh); leaving the drawer to a full
                // screen clears the whole overlay, so it is a Full render.
                let still_in_drawer = matches!(next, Screen::Navigation { .. });
                state.screen = next;
                state.render_generation = state.render_generation.next();
                if still_in_drawer {
                    vec![render_batch(state)]
                } else {
                    vec![render_batch_with_intent(state, RenderIntent::Full)]
                }
            } else {
                vec![]
            }
        }
        Screen::Settings { selected } => {
            // Settings is a state-machine list screen: UP/DOWN move the
            // row; ENTER selects the row's action (a deferred executor
            // wedge until that action's screen migrates); long ENTER backs
            // out to Home; long UP/DOWN opens the GO TO drawer.
            //
            // The drawer's overlay canvas is always Home (the executor's
            // Navigation view draws Home + the bar), and the legacy
            // Settings flow closed Settings when the drawer opened (the
            // long-UP/DOWN nav request returned out of open_menu). So
            // opening the drawer from Settings leaves Settings behind a
            // Home-origin drawer: cancel/select-HOME lands on Home, and
            // selecting SETTINGS re-enters Settings.
            let cur = *selected;
            match button {
                ButtonEvent::LongPressed(ButtonId::Up)
                | ButtonEvent::LongPressed(ButtonId::Down) => {
                    state.screen = Screen::Navigation {
                        selected: nav_home_index(NavOrigin::Home),
                        origin: NavOrigin::Home,
                    };
                    state.render_generation = state.render_generation.next();
                    vec![render_batch_with_intent(state, RenderIntent::Full)]
                }
                ButtonEvent::Pressed(ButtonId::Up) => {
                    state.screen = Screen::Settings {
                        selected: cur.saturating_sub(1),
                    };
                    state.render_generation = state.render_generation.next();
                    vec![render_batch(state)]
                }
                ButtonEvent::Pressed(ButtonId::Down) => {
                    state.screen = Screen::Settings {
                        selected: (cur + 1).min(SETTINGS_ROW_COUNT - 1),
                    };
                    state.render_generation = state.render_generation.next();
                    vec![render_batch(state)]
                }
                ButtonEvent::LongPressed(ButtonId::Enter) => {
                    // Back out of Settings to the root Home.
                    state.screen = Screen::Home;
                    state.render_generation = state.render_generation.next();
                    vec![render_batch_with_intent(state, RenderIntent::Full)]
                }
                ButtonEvent::Pressed(ButtonId::Enter) => {
                    if cur == SETTINGS_SYNC_NOW_ROW {
                        // SYNC NOW (row 0): start a background sync through
                        // the SM sync engine (same gate as the scheduled
                        // SyncBoundaryDue - idle + wifi/server configured +
                        // clock). The SM stays on Settings; the completion
                        // adopts + persists data through the existing
                        // SyncCompleted path. A busy / unconfigured / no
                        // clock start is a no-op (nothing to sync).
                        let mut batches = Vec::new();
                        if state.sync == SyncState::Idle
                            && state.connectivity.wifi_configured
                            && state.connectivity.server_configured
                        {
                            if let Some(now) = state.clock.now {
                                let op = state.next_operation_id();
                                state.sync = SyncState::Running { request_id: op };
                                batches.push(batch(
                                    state,
                                    op,
                                    FailurePolicy::Continue,
                                    vec![Effect::StartSync(SyncRequest { now })],
                                ));
                            }
                        }
                        state.render_generation = state.render_generation.next();
                        batches.push(render_batch_with_intent(state, RenderIntent::Full));
                        batches
                    } else if cur == SETTINGS_SYNC_INTERVAL_ROW {
                        // SYNC INTERVAL (row 1): open the SM interval picker.
                        state.screen = Screen::SyncIntervalPick { selected: 0 };
                        state.render_generation = state.render_generation.next();
                        vec![render_batch_with_intent(state, RenderIntent::Full)]
                    } else if cur == SETTINGS_SLEEP_ROW {
                        // SLEEP (row 3): an SM-owned effect - deep sleep with
                        // the maintenance wake needed for a far-future
                        // one-shot alarm (mirrors the idle deep-sleep plan).
                        let maintenance = state
                            .clock
                            .now
                            .and_then(|now| maintenance_wakeup_delay(&state.alarms.alarms, &now));
                        state.render_generation = state.render_generation.next();
                        vec![
                            batch(
                                state,
                                OperationId(0),
                                FailurePolicy::Continue,
                                vec![Effect::EnterDeepSleep(WakeupPlan { maintenance })],
                            ),
                            render_batch_with_intent(state, RenderIntent::Full),
                        ]
                    } else {
                        // BLE pairing (row 2): deferred to the executor
                        // (still a legacy blocking radio screen). The SM
                        // stays on Settings; the executor returns
                        // after the wedge and main re-renders Settings.
                        state.render_generation = state.render_generation.next();
                        vec![
                            batch(
                                state,
                                OperationId(0),
                                FailurePolicy::Continue,
                                vec![Effect::OpenBlePairingScreen],
                            ),
                            render_batch_with_intent(state, RenderIntent::Full),
                        ]
                    }
                }
                _ => vec![],
            }
        }
        Screen::SyncIntervalPick { selected } => {
            transition_sync_interval_pick_button(state, *selected, button)
        }
        Screen::AlarmList { selected } => transition_alarm_list_button(state, *selected, button),
        Screen::AlarmAdd(_) => transition_alarm_add_button(state, button),
        Screen::TodoList { selected } => transition_todo_list_button(state, *selected, button),
        Screen::Inbox { selected } => transition_inbox_list_button(state, *selected, button),
        Screen::InboxItem { .. } => transition_inbox_item_button(state, button),
        Screen::Calendar(_) => transition_calendar_button(state, button),
        Screen::WeekView { .. } => transition_week_view_button(state, button),
        Screen::BlePairing(_) => transition_ble_pairing_button(state, button),
        Screen::Home => {
            // Long UP/DOWN opens the global navigation drawer from Home
            // (origin Home, highlighting HOME).
            if matches!(
                button,
                ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down)
            ) {
                state.screen = Screen::Navigation {
                    selected: nav_home_index(NavOrigin::Home),
                    origin: NavOrigin::Home,
                };
                state.render_generation = state.render_generation.next();
                vec![render_batch_with_intent(state, RenderIntent::Full)]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Number of rows in the Settings list (SYNC NOW / SYNC INTERVAL / BLE
/// PAIRING / SLEEP) - mirrors the firmware's `open_menu` items.
pub const SETTINGS_ROW_COUNT: usize = 4;
/// Index of the SLEEP row in the Settings list (0=Sync Now, 1=Sync
/// Interval, 2=BLE Pairing, 3=Sleep).
pub const SETTINGS_SLEEP_ROW: usize = 3;
/// Index of the BLE PAIRING row in the Settings list.
pub const SETTINGS_BLE_PAIRING_ROW: usize = 2;
/// Index of the SYNC INTERVAL row in the Settings list.
pub const SETTINGS_SYNC_INTERVAL_ROW: usize = 1;
/// Index of the SYNC NOW row in the Settings list.
pub const SETTINGS_SYNC_NOW_ROW: usize = 0;

/// SyncIntervalPick screen (Stage 4): a five-option list picker owned by
/// the state machine (rows 0..=4 = 1/5/10/30/60 minutes). UP/DOWN move the
/// cursor (wrapping); ENTER confirms (fire-and-forget SetSyncInterval to
/// the executor) and returns to Settings; long ENTER cancels back.
fn transition_sync_interval_pick_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    match button {
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Cancel back to the Settings SYNC INTERVAL row.
            state.screen = Screen::Settings {
                selected: SETTINGS_SYNC_INTERVAL_ROW,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            state.screen = Screen::SyncIntervalPick {
                selected: if selected == 0 {
                    SYNC_INTERVAL_MINUTES.len() - 1
                } else {
                    selected - 1
                },
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            state.screen = Screen::SyncIntervalPick {
                selected: (selected + 1) % SYNC_INTERVAL_MINUTES.len(),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            // Confirm: fire-and-forget the interval write, return to
            // Settings (the executor persists to NVS; the firmware sync
            // scheduler picks the new cadence up on its next poll).
            let minutes = SYNC_INTERVAL_MINUTES[selected];
            state.screen = Screen::Settings {
                selected: SETTINGS_SYNC_INTERVAL_ROW,
            };
            state.render_generation = state.render_generation.next();
            vec![
                batch(
                    state,
                    OperationId(0),
                    FailurePolicy::Continue,
                    vec![Effect::SetSyncInterval { minutes }],
                ),
                render_batch_with_intent(state, RenderIntent::Full),
            ]
        }
        _ => vec![],
    }
}
/// The fixed SYNC INTERVAL choices (row order = the firmware's
/// sync_interval_screen OPTIONS). Indexed by Screen::SyncIntervalPick.
pub const SYNC_INTERVAL_MINUTES: [u16; 5] = [1, 5, 10, 30, 60];

/// The "+ ADD ALARM" row is the list length (rows 0..len are stored alarms).
/// AlarmList screen (Stage 4): browse the stored alarm rows (toggle
/// enabled on ENTER, an SM-owned confirmable edit) plus the trailing
/// "+ ADD ALARM" row, which opens the SM two-stage picker
/// (`Screen::AlarmAdd`).
fn transition_alarm_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let alarm_count = state.alarms.alarms.len();
    // Rows: 0..alarm_count are alarms; alarm_count is "+ ADD ALARM".
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            // Open the GO TO drawer over the alarm list (origin AlarmList).
            state.screen = Screen::Navigation {
                selected: nav_home_index(NavOrigin::AlarmList),
                origin: NavOrigin::AlarmList,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the alarm list to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            state.screen = Screen::AlarmList {
                selected: selected.saturating_sub(1),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            // The selection moves through the ADD row (index == alarm_count)
            // and clamps there.
            let max = alarm_count;
            state.screen = Screen::AlarmList {
                selected: (selected + 1).min(max),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            if selected == alarm_count {
                // "+ ADD ALARM": open the SM two-stage picker (hour, then
                // minute). The list's ADD-row stays selected underneath so a
                // cancel returns to it.
                state.screen = Screen::AlarmAdd(AlarmAddState::default());
                state.render_generation = state.render_generation.next();
                vec![render_batch_with_intent(state, RenderIntent::Full)]
            } else if state.pending_alarm_list_edit.is_some() {
                // One toggle at a time: while a persist is in flight a
                // second row ENTER is ignored (its completion/rollback
                // releases the gate).
                vec![]
            } else {
                // Toggle the alarm's enabled flag: optimistic flip + a
                // confirmable persist (the executor also marks it dirty).
                // On failure the row rolls back to its previous value.
                let op = state.next_operation_id();
                let Some(alarm) = state.alarms.alarms.get_mut(selected) else {
                    return vec![];
                };
                alarm.enabled = !alarm.enabled;
                let toggled_id = alarm.id;
                state.pending_alarm_list_edit = Some(PendingAlarmListEdit {
                    operation_id: op,
                    index: selected,
                });
                state.render_generation = state.render_generation.next();
                let list = state.alarms.alarms.clone();
                vec![
                    batch(
                        state,
                        op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::PersistAlarmToggle {
                            alarms: list,
                            toggled_id,
                        }],
                    ),
                    render_batch_with_intent(state, RenderIntent::Full),
                ]
            }
        }
        _ => vec![],
    }
}

/// AlarmAdd editor (Stage 4): a two-stage number picker owned by the state
/// machine. UP/DOWN step `value` (wrapping within the stage range); ENTER
/// advances Hour -> Minute -> complete; long ENTER cancels back to the
/// AlarmList ADD row. Completing appends a new Daily alarm (id via
/// `next_id`) optimistically and persists the list through a confirmable
/// `PersistAlarms` (one add in flight at a time); a failed persist rolls
/// the appended alarm back.
fn transition_alarm_add_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::AlarmAdd(AlarmAddState { stage, hour, .. }) = &mut state.screen else {
        return vec![];
    };
    let (stage, hour) = (*stage, *hour);
    match button {
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Cancel back to the AlarmList (the ADD row stays selected).
            state.screen = Screen::AlarmList {
                selected: state.alarms.alarms.len(),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            let (min, max) = range_for_stage(stage);
            let Screen::AlarmAdd(add) = &mut state.screen else {
                return vec![];
            };
            add.value = if add.value == min { max } else { add.value - 1 };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            let (min, max) = range_for_stage(stage);
            let Screen::AlarmAdd(add) = &mut state.screen else {
                return vec![];
            };
            add.value = if add.value == max { min } else { add.value + 1 };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => match stage {
            AddStage::Hour => {
                // Lock the hour and advance to the minute stage.
                let Screen::AlarmAdd(add) = &mut state.screen else {
                    return vec![];
                };
                add.hour = add.value;
                add.stage = AddStage::Minute;
                add.value = 0;
                state.render_generation = state.render_generation.next();
                vec![render_batch(state)]
            }
            AddStage::Minute => {
                // Complete: append a new Daily alarm (id via next_id) and
                // persist confirmably. One add in flight at a time.
                if state.pending_alarm_add.is_some() {
                    return vec![];
                }
                let minute = {
                    let Screen::AlarmAdd(add) = &state.screen else {
                        return vec![];
                    };
                    add.value
                };
                // All 256 ids in use: leave the list unchanged (no id to
                // allocate) - extremely unlikely, but degenerate safely.
                let Some(id) = crate::alarm_schedule::next_id(&state.alarms.alarms) else {
                    return vec![];
                };
                let new_alarm = StoredAlarm {
                    id,
                    hour,
                    minute,
                    repeat: Repeat::Daily,
                    enabled: true,
                    label: String::new(),
                };
                state.alarms.alarms.push(new_alarm);
                let op = state.next_operation_id();
                state.pending_alarm_add = Some(op);
                state.render_generation = state.render_generation.next();
                let list = state.alarms.alarms.clone();
                vec![
                    batch(
                        state,
                        op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::PersistAlarms(list)],
                    ),
                    render_batch_with_intent(state, RenderIntent::Full),
                ]
            }
        },
        _ => vec![],
    }
}

/// The picker range for an ADD-ALARM stage: hour 0-23, minute 0-59.
fn range_for_stage(stage: AddStage) -> (u8, u8) {
    match stage {
        AddStage::Hour => (0, 23),
        AddStage::Minute => (0, 59),
    }
}

/// row's done flag, long ENTER cycles its importance (Low -> Medium ->
/// High) - matching the legacy todos page, where long ENTER cycled
/// importance and the only way out was the long-UP/DOWN drawer. Both row
/// actions are optimistic confirmable SM edits (the executor persists the
/// list + marks the row dirty); a failure rolls the row back. Long UP/DOWN
/// opens the GO TO drawer (origin TodoList). An empty list has no rows to
/// act on (moves clamp at 0).
fn transition_todo_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let count = state.todos.todos.len();
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            // Open the GO TO drawer over the todo list (origin TodoList).
            state.screen = Screen::Navigation {
                selected: nav_home_index(NavOrigin::TodoList),
                origin: NavOrigin::TodoList,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            state.screen = Screen::TodoList {
                selected: selected.saturating_sub(1),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            // Clamp to the last row; an empty list stays at 0.
            let max = count.saturating_sub(1);
            state.screen = Screen::TodoList {
                selected: (selected + 1).min(max),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        // ENTER toggles done; long ENTER cycles importance. Both go
        // through the same confirmable-edit path (one at a time).
        ButtonEvent::Pressed(ButtonId::Enter) | ButtonEvent::LongPressed(ButtonId::Enter) => {
            if state.pending_todo_list_edit.is_some() {
                // One edit at a time: while a persist is in flight another
                // row action is ignored (completion/rollback releases it).
                return vec![];
            }
            // Capture what the edit will change before borrowing the row.
            let toggles_done = button == ButtonEvent::Pressed(ButtonId::Enter);
            let op = state.next_operation_id();
            let (previous_done, previous_importance, edited_id) = {
                let Some(todo) = state.todos.todos.get_mut(selected) else {
                    return vec![];
                };
                let previous_done = todo.done;
                let previous_importance = todo.importance;
                let edited_id = todo.id;
                if toggles_done {
                    todo.done = !todo.done;
                } else {
                    todo.importance = match todo.importance {
                        Importance::Low => Importance::Medium,
                        Importance::Medium => Importance::High,
                        Importance::High => Importance::Low,
                    };
                }
                (previous_done, previous_importance, edited_id)
            };
            state.pending_todo_list_edit = Some(PendingTodoListEdit {
                operation_id: op,
                index: selected,
                previous_done,
                previous_importance,
            });
            state.render_generation = state.render_generation.next();
            let list = state.todos.todos.clone();
            vec![
                batch(
                    state,
                    op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistTodoEdit {
                        todos: list,
                        edited_id,
                    }],
                ),
                render_batch_with_intent(state, RenderIntent::Full),
            ]
        }
        _ => vec![],
    }
}

/// InboxList screen (Stage 4): browse the stored inbox items (read/unread
/// markers from the executor's render); ENTER opens the selected item's
/// detail (an SM screen) and optimistically marks it read (fire-and-forget
/// `MarkInboxRead` local persist). Long UP/DOWN opens the GO TO drawer
/// (origin Inbox); long ENTER backs to Home.
fn transition_inbox_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let count = state.inbox.items.len();
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            // Open the GO TO drawer over the inbox list (origin Inbox).
            state.screen = Screen::Navigation {
                selected: nav_home_index(NavOrigin::Inbox),
                origin: NavOrigin::Inbox,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the inbox list to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            state.screen = Screen::Inbox {
                selected: selected.saturating_sub(1),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            // Clamp to the last row; an empty list stays at 0.
            let max = count.saturating_sub(1);
            state.screen = Screen::Inbox {
                selected: (selected + 1).min(max),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            if count == 0 {
                // Nothing to open on an empty list.
                return vec![];
            }
            let index = selected.min(count - 1);
            // Open the item's detail (an SM screen). Optimistically mark it
            // read in state and fire-and-forget the local read-mark persist
            // (two-way sync uploads it later).
            let unread_seq = {
                let Some(item) = state.inbox.items.get_mut(index) else {
                    return vec![];
                };
                if !item.read {
                    item.read = true;
                    Some(item.id)
                } else {
                    None
                }
            };
            let mut batches = Vec::new();
            if let Some(seq) = unread_seq {
                batches.push(batch(
                    state,
                    OperationId(0),
                    FailurePolicy::Continue,
                    vec![Effect::MarkInboxRead { seq }],
                ));
            }
            state.screen = Screen::InboxItem { index };
            state.render_generation = state.render_generation.next();
            batches.push(render_batch_with_intent(state, RenderIntent::Full));
            batches
        }
        _ => vec![],
    }
}

/// InboxItem screen (Stage 4): read-only title + body of the opened item.
/// Any button closes it back to the Inbox list (legacy open_inbox_item
/// semantics: "ENTER / HOLD ENTER CLOSE" - read-only).
fn transition_inbox_item_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::InboxItem { index } = &state.screen else {
        return vec![];
    };
    let index = *index;
    match button {
        ButtonEvent::Pressed(ButtonId::Enter)
        | ButtonEvent::LongPressed(ButtonId::Enter)
        | ButtonEvent::Pressed(ButtonId::Up)
        | ButtonEvent::Pressed(ButtonId::Down)
        | ButtonEvent::LongPressed(ButtonId::Up)
        | ButtonEvent::LongPressed(ButtonId::Down) => {
            state.screen = Screen::Inbox { selected: index };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        _ => vec![],
    }
}

/// Calendar screen (Stage 4): a read-only grid of the current month with a
/// day cursor. UP/DOWN move the cursor within the month; ENTER opens the
/// selected day's week view through a deferred legacy wedge (the SM stays
/// on Calendar and main re-renders the grid when the wedge returns); long
/// ENTER backs to Home; long UP/DOWN opens the GO TO drawer (origin
/// Calendar). The grid tracks the clock (cross-month Ticks refresh it).
fn transition_calendar_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::Calendar(cal) = &mut state.screen else {
        return vec![];
    };
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            // Open the GO TO drawer over the calendar (origin Calendar).
            state.screen = Screen::Navigation {
                selected: nav_home_index(NavOrigin::Calendar),
                origin: NavOrigin::Calendar,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the calendar to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            cal.selected_day = cal.selected_day.saturating_sub(1).max(1);
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            let dim = days_in_month(cal.year, cal.month);
            cal.selected_day = (cal.selected_day + 1).min(dim);
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            // Open the selected day's week view (an SM screen now): the
            // view is read-only and any button closes it back to the grid.
            let day = cal.selected_day;
            state.screen = Screen::WeekView {
                year: cal.year,
                month: cal.month,
                day,
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        _ => vec![],
    }
}

/// WeekView screen (Stage 4): read-only week columns. Any button closes it
/// back to the Calendar grid; the grid keeps the day the view was opened
/// from as its cursor (legacy week_view semantics: read-only, nothing to
/// drill into, "any button closes").
fn transition_week_view_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::WeekView { year, month, day } = &state.screen else {
        return vec![];
    };
    let (year, month, day) = (*year, *month, *day);
    match button {
        // Any button (short/long ENTER / UP / DOWN, releases ignored) closes
        // the read-only view. Releases are ignored (they always follow a
        // press that already closed it).
        ButtonEvent::Pressed(ButtonId::Enter)
        | ButtonEvent::LongPressed(ButtonId::Enter)
        | ButtonEvent::Pressed(ButtonId::Up)
        | ButtonEvent::Pressed(ButtonId::Down)
        | ButtonEvent::LongPressed(ButtonId::Up)
        | ButtonEvent::LongPressed(ButtonId::Down) => {
            state.screen = Screen::Calendar(CalendarState {
                year,
                month,
                selected_day: day,
            });
            state.render_generation = state.render_generation.next();
            vec![render_batch_with_intent(state, RenderIntent::Full)]
        }
        _ => vec![],
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

    // The calendar grid always shows the current month: when the clock rolls
    // into a new month while the Calendar screen is current, adopt the new
    // year/month and clamp the day cursor (matching the legacy browse_page,
    // which re-read the RTC each visible change and re-rendered the grid).
    if let Screen::Calendar(cal) = &mut state.screen {
        let dim = days_in_month(now.year, now.month);
        if cal.year != now.year || cal.month != now.month {
            cal.year = now.year;
            cal.month = now.month;
            cal.selected_day = cal.selected_day.min(dim).max(1);
        }
    }

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

    if changed && tick_refreshes_current_screen(&state.screen) {
        state.render_generation = state.render_generation.next();
        batches.push(render_batch(state));
    }
    batches
}

/// Whether a minute-change Tick should re-render the current screen.
/// Home (clock), the Calendar grid (dates) and the TodoList ("DUE TODAY"
/// markers) are time-sensitive; the list screens may show data a sync just
/// changed. Pure read-only sub-screens (a week view, an item detail, a
/// picker, the pairing session, the ADD-ALARM editor) have no time-driven
/// content, and AlarmRinging is drawn over by the legacy ring screen - a
/// minute tick on any of these would only burn an unnecessary e-ink
/// refresh.
fn tick_refreshes_current_screen(screen: &Screen) -> bool {
    !matches!(
        screen,
        Screen::AlarmRinging
            | Screen::WeekView { .. }
            | Screen::InboxItem { .. }
            | Screen::AlarmAdd(_)
            | Screen::SyncIntervalPick { .. }
            | Screen::BlePairing(_)
    )
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
            // An AlarmList enabled-toggle confirmed: release the gate. The
            // executor already saved the list and marked the row dirty. The
            // stored list changed, so re-point the RTC hardware slot at the
            // new nearest alarm / disable if the list is now empty.
            if state
                .pending_alarm_list_edit
                .as_ref()
                .is_some_and(|edit| edit.operation_id == completion.operation_id)
            {
                state.pending_alarm_list_edit = None;
                if let Some(now) = state.clock.now {
                    batches.extend(program_alarm_for(state, &now));
                }
            }
            // An ADD-ALARM append confirmed: release the one-at-a-time add
            // gate and re-point the RTC slot at the new nearest alarm.
            if state.pending_alarm_add == Some(completion.operation_id) {
                state.pending_alarm_add = None;
                if let Some(now) = state.clock.now {
                    batches.extend(program_alarm_for(state, &now));
                }
            }
        }
        EffectOutput::Persisted(PersistTarget::Todos) => {
            // A TodoList row edit confirmed: release the one-at-a-time gate.
            // The executor already saved the list and marked the row dirty.
            if state
                .pending_todo_list_edit
                .as_ref()
                .is_some_and(|edit| edit.operation_id == completion.operation_id)
            {
                state.pending_todo_list_edit = None;
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
        EffectOutput::Persisted(PersistTarget::Config) => {
            // A SetServer config persist confirmed: apply the status-visible
            // server facts now (NVS holds them authoritatively; the generic
            // resolver below sends the reply).
            for channel in [Channel::Usb, Channel::Ble] {
                apply_confirmed_server_config(state, completion.operation_id, channel);
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
            // An AlarmList enabled-toggle failed to persist: roll the
            // optimistic row back (NVS never changed), release the gate and
            // re-render. No RTC reprogram - the hardware slot still matches
            // the unchanged stored list.
            let rollback_index = match state.pending_alarm_list_edit.take() {
                Some(edit) if edit.operation_id == failure.operation_id => Some(edit.index),
                Some(edit) => {
                    state.pending_alarm_list_edit = Some(edit);
                    None
                }
                None => None,
            };
            if let Some(index) = rollback_index {
                if let Some(alarm) = state.alarms.alarms.get_mut(index) {
                    alarm.enabled = !alarm.enabled;
                }
                state.render_generation = state.render_generation.next();
            }
            // A TodoList row edit failed to persist: roll both the done
            // flag and the importance back (NVS never changed), release the
            // gate and re-render.
            let todo_rollback = match state.pending_todo_list_edit.take() {
                Some(edit) if edit.operation_id == failure.operation_id => Some(edit),
                Some(edit) => {
                    state.pending_todo_list_edit = Some(edit);
                    None
                }
                None => None,
            };
            if let Some(rollback) = todo_rollback {
                if let Some(todo) = state.todos.todos.get_mut(rollback.index) {
                    todo.done = rollback.previous_done;
                    todo.importance = rollback.previous_importance;
                }
                state.render_generation = state.render_generation.next();
            }
            // An ADD-ALARM append failed to persist: pop the optimistically
            // appended alarm back (NVS never changed), release the gate and
            // re-render. The SM screen is AlarmAdd (Minute stage); the
            // appended alarm is the last element.
            if state.pending_alarm_add == Some(failure.operation_id) {
                state.pending_alarm_add = None;
                state.alarms.alarms.pop();
                state.render_generation = state.render_generation.next();
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
    // A failed StartSync must release the single-flight lock too: the
    // completion path (transition_sync_completed) will never run for an
    // effect that failed at dispatch, so without this the next SyncNow
    // would be told Busy forever.
    if matches!(state.sync, SyncState::Running { .. })
        && state.pending_usb_reply.is_none()
        && state.pending_ble_reply.is_none()
    {
        state.sync = SyncState::Idle;
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
        ControlRequest::SyncNow => {
            // Single network operation at a time (SyncState arbitration): if
            // a sync is already running (from USB, BLE or the scheduler),
            // this transport is told Busy and its request is not queued.
            if state.sync != SyncState::Idle {
                return vec![reply_batch(state, channel, Reply::Busy)];
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
            let op = state.next_operation_id();
            state.sync = SyncState::Running { request_id: op };
            // The reply is resolved by Event::SyncCompleted (Ok / Failed)
            // on the transport that requested the sync.
            set_pending_reply(state, channel, op, ControlRequest::SyncNow, vec![op]);
            vec![batch(
                state,
                op,
                FailurePolicy::Continue,
                vec![Effect::StartSync(SyncRequest { now })],
            )]
        }
        ControlRequest::SetWifi { ssid, password } => {
            // Wi-Fi verification + save runs on the network executor (it
            // owns the Wi-Fi driver); the reply is deferred like SyncNow.
            // The command takes the requesting transport's pending slot and
            // emits StartSetWifi; Event::SetWifiCompleted resolves it. The
            // status-visible wifi facts are applied only on success (the
            // executor verified + persisted the credentials), so a failed
            // verification never leaves facts claiming credentials that are
            // not in NVS.
            let op = state.next_operation_id();
            let creds = WifiCreds {
                ssid: ssid.clone(),
                password: password.clone(),
            };
            set_pending_reply(
                state,
                channel,
                op,
                ControlRequest::SetWifi {
                    ssid: creds.ssid.clone(),
                    password: creds.password.clone(),
                },
                vec![op],
            );
            vec![batch(
                state,
                op,
                FailurePolicy::Continue,
                vec![Effect::StartSetWifi(creds)],
            )]
        }
        ControlRequest::SetServer { url, token } => {
            // Save the new endpoint and invalidate the stale sync ETag: the
            // old server's conditional-request state must not survive a
            // server change or the next sync would be suppressed by a
            // now-meaningless If-None-Match. Two confirmable ops (config
            // persist + etag clear); the reply waits for both. The
            // status-visible config facts are applied only when the persist
            // confirms (NVS-authoritative, like SetTimezone) so a failed
            // command never leaves the in-state facts diverged from NVS.
            let cfg_op = state.next_operation_id();
            let clear_op = state.next_operation_id();
            let cfg = DeviceConfig {
                server_url: url.clone(),
                auth_token: token.clone(),
            };
            let pending_request = ControlRequest::SetServer { url, token };
            set_pending_reply(
                state,
                channel,
                cfg_op,
                pending_request,
                vec![cfg_op, clear_op],
            );
            vec![
                batch(
                    state,
                    cfg_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistConfig(cfg)],
                ),
                batch(
                    state,
                    clear_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::ClearSyncEtag],
                ),
            ]
        }
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

/// Applies a confirmed SetServer config persist to the status-visible
/// server facts (URL presence + token presence; the token value never
/// enters AppState). The target comes from the pending command's request,
/// so the facts only ever reflect a value NVS now holds.
fn apply_confirmed_server_config(
    state: &mut AppState,
    completed_op: OperationId,
    channel: Channel,
) {
    let slot = match channel {
        Channel::Usb => &state.pending_usb_reply,
        Channel::Ble => &state.pending_ble_reply,
    };
    let Some(pending) = slot else { return };
    if !pending.awaiting_ops.contains(&completed_op) {
        return;
    }
    if let ControlRequest::SetServer { url, token } = &pending.request {
        state.config.server_url = Some(url.clone());
        state.config.server_has_token = !token.is_empty();
        state.connectivity.server_configured = state.config.server_configured();
    }
}

/// Resolves a completed sync's pending SyncNow reply on `channel` with the
/// result-specific reply (Ok / Failed). Returns `(channel, reply)` when the
/// slot holds a SyncNow request still awaiting its sync, clearing the slot.
fn resolve_sync_completion(
    state: &mut AppState,
    channel: Channel,
    reply: &ControlReply,
) -> Option<(Channel, ControlReply)> {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    let pending = slot.as_ref()?;
    let is_sync = matches!(pending.request, ControlRequest::SyncNow)
        && !pending.awaiting_ops.is_empty()
        && pending.reply.is_none();
    if !is_sync {
        return None;
    }
    let reply = reply.clone();
    *slot = None;
    Some((channel, reply))
}

/// Re-points a pending SyncNow slot (the transport whose SyncNow completed)
/// at the sync-apply op: its final Ok now waits for `Effect::ApplySyncedData`
/// to confirm. Returns `true` when such a slot was found and re-pointed.
fn repoint_sync_to_apply(state: &mut AppState, channel: Channel, apply_op: OperationId) -> bool {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    let Some(pending) = slot else { return false };
    if !matches!(pending.request, ControlRequest::SyncNow) || pending.reply.is_some() {
        return false;
    }
    pending.awaiting_ops = vec![apply_op];
    true
}

/// Resolves a completed SetWifi's pending slot on `channel`. Returns the
/// pending wifi request (SSID + password presence) and clears the slot,
/// or `None` when the slot does not hold a pending SetWifi. The caller
/// applies the status-visible facts and builds the reply: on success the
/// facts reflect the verified-and-persisted credentials; on failure they
/// are left as NVS holds them.
fn take_set_wifi_pending(state: &mut AppState, channel: Channel) -> Option<(Channel, WifiCreds)> {
    let slot = match channel {
        Channel::Usb => &mut state.pending_usb_reply,
        Channel::Ble => &mut state.pending_ble_reply,
    };
    let pending = slot.as_ref()?;
    let ControlRequest::SetWifi { ssid, password } = &pending.request else {
        return None;
    };
    let creds = WifiCreds {
        ssid: ssid.clone(),
        password: password.clone(),
    };
    *slot = None;
    Some((channel, creds))
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
    let view = state.screen.render_view();
    EffectBatch {
        id: state.next_batch_id(),
        operation_id: OperationId(0),
        render_generation: Some(state.render_generation),
        effects: vec![Effect::Render(RenderRequest {
            generation: state.render_generation,
            intent,
            view,
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

    fn synced_data(alarms: Vec<StoredAlarm>) -> SyncedData {
        SyncedData {
            alarms,
            todos: vec![],
            inbox: vec![],
            inbox_read_acked: vec![],
            inbox_truncated: false,
            etag: None,
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
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
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
        update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );

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
    fn set_server_persists_config_and_clears_etag_then_replies_ok() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetServer {
                url: "https://sync.example".into(),
                token: "secret".into(),
            }),
        );
        assert!(state.pending_usb_reply.is_some());
        assert!(state.pending_ble_reply.is_none());
        // Two confirmable batches: PersistConfig + ClearSyncEtag.
        let cfg_batch = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistConfig(_)))
            })
            .expect("SetServer must persist the config");
        let clear_batch = batches
            .iter()
            .find(|b| b.effects.iter().any(|e| matches!(e, Effect::ClearSyncEtag)))
            .expect("SetServer must clear the stale ETag");
        let cfg_op = cfg_batch.operation_id;
        let clear_op = clear_batch.operation_id;
        assert_ne!(cfg_op, clear_op);
        // Status-visible facts not applied until the config persist confirms.
        assert!(state.config.server_url.is_none(), "facts apply on confirm");
        // Config confirms: facts applied, no reply yet (etag clear pending).
        let mid = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: cfg_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Config),
            }),
        );
        assert_eq!(
            state.config.server_url.as_deref(),
            Some("https://sync.example")
        );
        assert!(state.config.server_has_token);
        assert!(!mid
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
        // ETag clear confirms -> final Ok on USB, slot released.
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: clear_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::SyncMetadata),
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
    }

    #[test]
    fn set_wifi_success_applies_facts_and_replies_ok_on_the_channel() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::BleCommand(ControlRequest::SetWifi {
                ssid: "home-wifi".into(),
                password: "sekrit".into(),
            }),
        );
        assert!(state.pending_ble_reply.is_some());
        assert!(state.pending_usb_reply.is_none());
        // Facts not yet applied (verification + NVS save not confirmed).
        assert!(state.config.wifi_ssid.is_none());
        let batches_has_effect = batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::StartSetWifi(_)))
        });
        assert!(batches_has_effect);
        let done = update(&mut state, Event::SetWifiCompleted(Ok(())));
        assert!(done.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        reply: Reply::Ok
                    }
                )
            })
        }));
        // Facts now reflect the verified + persisted credentials (no secret
        // in state - only presence).
        assert_eq!(state.config.wifi_ssid.as_deref(), Some("home-wifi"));
        assert!(state.config.wifi_has_password);
        assert!(state.connectivity.wifi_configured);
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn set_wifi_failure_replies_error_and_leaves_facts_unchanged() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetWifi {
                ssid: "home-wifi".into(),
                password: "wrong".into(),
            }),
        );
        let done = update(
            &mut state,
            Event::SetWifiCompleted(Err("authentication failed".into())),
        );
        let error_seen = done.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("authentication failed"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(state.pending_usb_reply.is_none());
        assert!(
            state.config.wifi_ssid.is_none(),
            "failed verification must not apply wifi facts"
        );
    }

    #[test]
    fn set_wifi_completion_without_pending_request_is_harmless() {
        let mut state = AppState::default();
        let done = update(&mut state, Event::SetWifiCompleted(Ok(())));
        assert!(!done
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
    }

    #[test]
    fn set_server_persist_failure_replies_error_and_leaves_facts_unchanged() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::BleCommand(ControlRequest::SetServer {
                url: "https://sync.example".into(),
                token: "secret".into(),
            }),
        );
        let cfg_op = batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistConfig(_)))
            })
            .unwrap()
            .operation_id;
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: cfg_op,
                render_generation: None,
                error: EffectError::Persist("nvs write failed".into()),
            }),
        );
        let error_seen = failed.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Ble,
                    reply: Reply::Error { message },
                } => message.contains("nvs write failed"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(state.pending_ble_reply.is_none(), "slot released");
        assert!(
            state.config.server_url.is_none(),
            "facts must not apply on a failed SetServer"
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
    fn sync_now_start_failure_releases_lock_and_errors() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        let op = batches
            .iter()
            .find(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_))))
            .unwrap()
            .operation_id;
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Sync("sync task spawn failed".into()),
            }),
        );
        let error_seen = failed.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("sync task spawn failed"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(state.pending_usb_reply.is_none());
        // The single-flight lock is released: the next SyncNow is not Busy.
        assert_eq!(state.sync, SyncState::Idle);
        let retry = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(retry
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::StartSync(_))) }));
        assert!(matches!(state.sync, SyncState::Running { .. }));
    }

    #[test]
    fn sync_now_starts_sync_and_takes_one_reply_slot() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(state.pending_usb_reply.is_some());
        assert!(state.pending_ble_reply.is_none());
        assert!(batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::StartSync(_))) }));
    }

    #[test]
    fn sync_now_while_running_replies_busy_on_either_transport() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        // A second sync from the other transport is busy (single-flight).
        let ble_batches = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(ble_batches.iter().any(|b| {
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
        assert!(state.pending_ble_reply.is_none(), "busy takes no slot");
        assert!(matches!(state.sync, SyncState::Running { .. }));
        // The USB sync's own slot is untouched.
        assert!(state.pending_usb_reply.is_some());
    }

    #[test]
    fn sync_completed_ok_applies_data_then_replies_ok_on_the_requesting_transport() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(matches!(state.sync, SyncState::Running { .. }));
        // SyncCompleted(Ok) adopts the merged data and emits an apply batch
        // - no reply yet (persist must confirm before Ok).
        let applied = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![alarm(1, 9, 0), alarm(2, 10, 30)]),
            }),
        );
        assert_eq!(state.alarms.alarms, vec![alarm(1, 9, 0), alarm(2, 10, 30)]);
        assert_eq!(state.sync, SyncState::Idle);
        assert!(state.pending_ble_reply.is_some(), "slot awaits the apply");
        assert!(!applied
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::Reply { .. })) }));
        let apply_op = applied
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ApplySyncedData(_)))
                    .then_some(b.operation_id)
            })
            .expect("SyncCompleted(Ok) must emit an apply");
        // Apply confirms -> final Ok on BLE, slot released.
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: apply_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::SyncApply),
            }),
        );
        assert!(done.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Ble,
                        reply: Reply::Ok
                    }
                )
            })
        }));
        assert!(state.pending_ble_reply.is_none());
        assert!(state.pending_usb_reply.is_none());
    }

    #[test]
    fn sync_completed_apply_failure_replies_error_and_releases_slot() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        let applied = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![alarm(1, 9, 0)]),
            }),
        );
        let apply_op = applied
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ApplySyncedData(_)))
                    .then_some(b.operation_id)
            })
            .unwrap();
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: apply_op,
                render_generation: None,
                error: EffectError::Persist("apply failed".into()),
            }),
        );
        assert!(failed.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("apply failed"),
                _ => false,
            })
        }));
        assert!(
            state.pending_usb_reply.is_none(),
            "slot released on apply failure"
        );
    }

    #[test]
    fn sync_completed_failed_replies_error_on_the_requesting_transport() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        let done = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Failed("connection refused".into())),
        );
        let error_seen = done.iter().any(|b| {
            b.effects.iter().any(|e| match e {
                Effect::Reply {
                    channel: Channel::Usb,
                    reply: Reply::Error { message },
                } => message.contains("connection refused"),
                _ => false,
            })
        });
        assert!(error_seen);
        assert!(state.pending_usb_reply.is_none());
        assert_eq!(state.sync, SyncState::Idle);
    }

    #[test]
    fn sync_completed_without_running_sync_is_harmless() {
        // A stale / unsolicited completion (e.g. after a deep-sleep restart
        // replayed the sync receipt) must not fabricate a reply.
        let mut state = AppState::default();
        let done = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![]),
            }),
        );
        assert!(!done
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
        assert_eq!(state.sync, SyncState::Idle);
        assert!(state.pending_usb_reply.is_none());
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn sync_boundary_due_starts_when_idle_and_configured() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.wifi_ssid = Some("home-wifi".into());
        state.config.server_url = Some("https://server.example".into());
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let batches = update(&mut state, Event::SyncBoundaryDue);
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::StartSync(_))) }));
        // No transport slot is taken for a scheduled sync.
        assert!(state.pending_usb_reply.is_none());
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn sync_boundary_due_is_noop_while_a_sync_is_running() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.wifi_ssid = Some("home-wifi".into());
        state.config.server_url = Some("https://server.example".into());
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let _ = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(matches!(state.sync, SyncState::Running { .. }));
        let batches = update(&mut state, Event::SyncBoundaryDue);
        assert!(batches.is_empty(), "single-flight: no second sync");
        assert!(matches!(state.sync, SyncState::Running { .. }));
    }

    #[test]
    fn sync_boundary_due_is_noop_when_not_configured() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        // No wifi / server configured.
        let batches = update(&mut state, Event::SyncBoundaryDue);
        assert!(batches.is_empty());
        assert_eq!(state.sync, SyncState::Idle);
    }

    #[test]
    fn long_press_opens_navigation_from_home() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home
            }
        );
        // A full render is requested for the drawer overlay.
        assert!(batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::Render(_))) }));
        // The request names the Navigation surface (executor draws the
        // drawer overlay, not a call-site rectangle).
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Navigation { selected: 0 },
                ..
            })
        )));
    }

    #[test]
    fn navigation_down_up_move_wrap_and_jump() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        // Down from 0 -> 1.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 1,
                origin: NavOrigin::Home
            }
        );
        // Up from 1 -> 0.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home
            }
        );
        // Up wraps from 0 -> last (5).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: NAV_DESTINATION_COUNT - 1,
                origin: NavOrigin::Home
            }
        );
        // Long DOWN jumps to last; long UP jumps to first.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home
            }
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: NAV_DESTINATION_COUNT - 1,
                origin: NavOrigin::Home
            }
        );
    }

    #[test]
    fn navigation_select_home_closes_drawer() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        assert!(matches!(state.screen, Screen::Navigation { .. }));
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
    }

    #[test]
    fn navigation_select_calendar_opens_sm_calendar_screen() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(
                vec![],
                Some(dt_full(9, 0, 1, 15)),
                false,
                true,
            )),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        // Move to destination 1 (CALENDAR).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // The calendar opens on the current month with the cursor on today.
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 15,
            })
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Calendar {
                    year: 2026,
                    month: 8,
                    selected_day: 15,
                },
                intent: RenderIntent::Full,
                ..
            })
        )));
    }

    #[test]
    fn navigation_cancel_with_long_enter_returns_home() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
    }

    #[test]
    fn render_request_view_tracks_screen_surface() {
        // Home render requests draw the Home surface.
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(9, 0)), false, true)),
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Home,
                ..
            })
        )));

        // Opening the drawer requests the Navigation surface.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Navigation { selected: 0 },
                ..
            })
        )));

        // Selecting HOME closes the drawer and renders the Home surface.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Home,
                ..
            })
        )));
    }

    #[test]
    fn drawer_open_is_full_move_is_partial_close_is_full() {
        // Refresh policy for the on-panel overlay: opening the drawer and
        // closing it clear/reveal the whole overlay (Full), while a move
        // inside the drawer only changes the bar (Partial -> the executor
        // refreshes just the NAV_BAR rect). This is the intent contract the
        // executor's view-aware partial relies on.
        let mut state = AppState::default();

        // Open: Full.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert!(
            batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
                e,
                Effect::Render(RenderRequest {
                    intent: RenderIntent::Full,
                    ..
                })
            )),
            "drawer open must be a Full render"
        );

        // Move: Partial.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert!(
            batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
                e,
                Effect::Render(RenderRequest {
                    intent: RenderIntent::Partial,
                    ..
                })
            )),
            "drawer move must be a Partial render (NAV_BAR refresh only)"
        );
        assert!(!batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                ..
            })
        )));

        // Long ENTER cancels back to Home: Full.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert!(
            batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
                e,
                Effect::Render(RenderRequest {
                    intent: RenderIntent::Full,
                    ..
                })
            )),
            "drawer close (cancel) must be a Full render"
        );

        // Reopen, move to a non-Home destination, ENTER selects: the drawer
        // closes to Home with a Full render plus the deferred destination
        // effect.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                ..
            })
        )));
    }

    #[test]
    fn drawer_select_settings_opens_sm_settings_screen() {
        let mut state = AppState::default();
        // Open the drawer from Home.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        // Jump to the last destination (SETTINGS, index 5).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: NAV_DESTINATION_COUNT - 1,
                origin: NavOrigin::Home
            }
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        // No destination effect: Settings is an SM screen now.
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Settings { selected: 0 },
                intent: RenderIntent::Full,
                ..
            })
        )));
    }

    #[test]
    fn settings_rows_move_and_wrap_clamped() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

        // Down to the last row (3 = SLEEP).
        for _ in 0..3 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        assert_eq!(state.screen, Screen::Settings { selected: 3 });
        // Down past the end clamps (no wrap).
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 3 });
        // Up back to row 2.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 2 });
        // Up past the top clamps at 0.
        for _ in 0..5 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
            );
        }
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        assert!(
            batches.iter().all(|b| {
                !b.effects.iter().any(|e| {
                    matches!(
                        e,
                        Effect::Render(RenderRequest {
                            intent: RenderIntent::Full,
                            ..
                        })
                    )
                })
            }),
            "Settings row moves are Partial (list region refresh)"
        );
    }

    #[test]
    fn settings_enter_row_emits_open_settings_item_and_stays() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Move to row 2 (BLE Pairing).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::OpenBlePairingScreen)));
        // The SM stays on Settings (the executor runs the wedge and returns;
        // main re-renders Settings).
        assert_eq!(state.screen, Screen::Settings { selected: 2 });
    }

    #[test]
    fn settings_sleep_row_emits_deep_sleep_effect() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Move to row 3 (SLEEP).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 3 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Sleep is an SM effect now (deep sleep with the maintenance wake);
        // no legacy wedge for the Sleep row.
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::EnterDeepSleep(WakeupPlan { .. }))));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::OpenBlePairingScreen)));
    }

    #[test]
    fn settings_sleep_with_future_once_alarm_plans_maintenance_wake() {
        let mut state = AppState::default();
        // A future-month one-shot alarm needs a maintenance wake before the
        // target month (PCF8563 cannot compare month/year).
        let mut list = vec![alarm(1, 8, 0)];
        list[0].repeat = Repeat::Once {
            year: 2026,
            month: 10,
            day: 1,
        };
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(list, Some(dt(8, 1)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Row 3 (SLEEP).
        for _ in 0..3 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::EnterDeepSleep(WakeupPlan {
                maintenance: Some(_),
                ..
            })
        )));
    }

    #[test]
    fn settings_sync_interval_row_opens_sm_picker() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Move to row 1 (SYNC INTERVAL) and ENTER: opens the SM picker, not
        // a legacy wedge.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 0 });
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::OpenBlePairingScreen)));
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::SyncInterval { selected: 0 },
                ..
            })
        )));
    }

    #[test]
    fn sync_interval_picker_moves_and_confirms() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 0 });
        // Down twice: 0 -> 1 (5 MIN) -> 2 (10 MIN).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 2 });
        // ENTER confirms 10 MIN: emits SetSyncInterval and returns to
        // Settings row 1.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::SetSyncInterval { minutes: 10 })));
        assert_eq!(
            state.screen,
            Screen::Settings {
                selected: SETTINGS_SYNC_INTERVAL_ROW,
            }
        );
    }

    #[test]
    fn sync_interval_picker_up_wraps_and_long_enter_cancels() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // UP from row 0 wraps to row 4 (60 MIN).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 4 });
        // Long ENTER cancels back to the Settings SYNC INTERVAL row.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::Settings {
                selected: SETTINGS_SYNC_INTERVAL_ROW,
            }
        );
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::SetSyncInterval { .. })));
    }

    #[test]
    fn settings_sync_now_row_starts_sm_sync() {
        let mut state = AppState::default();
        // Boot with wifi + server configured so the sync gate passes.
        let mut snap = boot_snapshot(vec![], Some(dt(8, 0)), false, true);
        snap.config = DeviceConfig {
            server_url: "https://example.com".into(),
            auth_token: "t".into(),
        };
        snap.status = DeviceStatus {
            wifi_ssid: Some("net".into()),
            wifi_has_password: true,
            timezone_offset_minutes: 0,
        };
        let _ = update(&mut state, Event::Boot(snap));
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        // ENTER on row 0 (SYNC NOW): starts the SM sync engine (Running) and
        // emits StartSync - no legacy wedge.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartSync(_))));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::OpenBlePairingScreen)));
        // The SM stays on Settings.
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
    }

    #[test]
    fn settings_sync_now_unconfigured_or_busy_is_noop() {
        let mut state = AppState::default();
        // No wifi/server configured.
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // No StartSync; sync stays Idle.
        assert_eq!(state.sync, SyncState::Idle);
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartSync(_))));
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

        // Busy (a scheduled sync is running): row-0 ENTER is a no-op too.
        let mut snap = boot_snapshot(vec![], Some(dt(8, 0)), false, true);
        snap.config = DeviceConfig {
            server_url: "https://example.com".into(),
            auth_token: "t".into(),
        };
        snap.status = DeviceStatus {
            wifi_ssid: Some("net".into()),
            wifi_has_password: true,
            timezone_offset_minutes: 0,
        };
        let _ = update(&mut state, Event::Boot(snap));
        state.sync = SyncState::Running {
            request_id: OperationId(99),
        };
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartSync(_))));
    }

    #[test]
    fn ble_pairing_events_drive_phase_and_stop_effects() {
        // With a BlePairing screen current (after boot), the pairing
        // lifecycle facts the radio reports drive the phase and issue the
        // StopBlePairing teardown on terminal outcomes.
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Waiting,
        });
        // Started -> Pairing.
        let batches = update(&mut state, Event::BlePairingStarted);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
            })
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                ..
            })
        )));
        // Succeeded -> Success + StopBlePairing.
        let batches = update(
            &mut state,
            Event::BlePairingSucceeded(BlePairingResult {
                name: "photon".into(),
            }),
        );
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Success,
            })
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopBlePairing)));
        // Failed -> Failure(message) + StopBlePairing.
        let batches = update(
            &mut state,
            Event::BlePairingFailed(BlePairingFailure {
                message: "timeout".into(),
            }),
        );
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Failure("timeout".into()),
            })
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopBlePairing)));
        // Disconnected -> Waiting.
        let _ = update(&mut state, Event::BleDisconnected);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
            })
        );
    }

    #[test]
    fn ble_pairing_events_are_ignored_off_screen() {
        // When no pairing session is current these facts leave state alone.
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        assert_eq!(state.screen, Screen::Home);
        let batches = update(&mut state, Event::BlePairingStarted);
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().all(|b| b.effects.is_empty()));
    }

    #[test]
    fn ble_pairing_any_button_exits_to_settings() {
        // Any button press closes the session back to the Settings BLE row
        // and tears down the radio.
        let buttons = [
            ButtonEvent::Pressed(ButtonId::Enter),
            ButtonEvent::LongPressed(ButtonId::Up),
            ButtonEvent::Pressed(ButtonId::Down),
            ButtonEvent::LongPressed(ButtonId::Enter),
        ];
        for b in buttons {
            let mut state = AppState::default();
            let _ = update(
                &mut state,
                Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
            );
            state.screen = Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
            });
            let batches = update(&mut state, Event::Button(b));
            assert_eq!(
                state.screen,
                Screen::Settings {
                    selected: SETTINGS_BLE_PAIRING_ROW,
                }
            );
            assert!(batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::StopBlePairing)));
        }
    }

    #[test]
    fn settings_long_enter_backs_to_home() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Home,
                ..
            })
        )));
    }

    #[test]
    fn drawer_over_settings_opens_home_origin_and_reenters_settings() {
        let mut state = AppState::default();
        // Home -> drawer -> SETTINGS.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        // Long UP/DOWN inside Settings opens the GO TO drawer. The drawer
        // overlay canvas is always Home, and the legacy flow closed
        // Settings when its drawer opened: the drawer is Home-origin.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home
            }
        );
        // A no-move ENTER selects HOME: back to Home.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);

        // Re-enter Settings, then long-UP/DOWN -> drawer -> select
        // SETTINGS -> Settings again.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Settings { selected: 0 },
                ..
            })
        )));
    }

    // ---- AlarmList screen (Stage 4) --------------------------------------

    /// Boots with `alarms` loaded (clock known, no AF) so the SM is in a
    /// Home + armed state, then opens the ALARMS drawer destination (index
    /// 3) and selects it.
    fn open_alarm_list(state: &mut AppState, alarms: Vec<StoredAlarm>) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot(alarms, Some(dt(9, 0)), false, true)),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home
            }
        );
        // Down three times: 0 -> 1 -> 2 -> 3 (ALARMS).
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 3,
                origin: NavOrigin::Home
            }
        );
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
    }

    #[test]
    fn drawer_select_alarms_opens_sm_alarm_list() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(9, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        // Move to index 3 (ALARMS): Down three times.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
        // ALARMS is an SM screen: no destination effect.
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::AlarmList { selected: 0 },
                intent: RenderIntent::Full,
                ..
            })
        )));
    }

    #[test]
    fn alarm_list_rows_browse_and_clamp_through_add_row() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0), alarm(2, 22, 30)]);
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
        // Down to row 1 (second alarm).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 1 });
        // Down again to row 2 = "+ ADD ALARM".
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 2 });
        // Down past ADD clamps.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 2 });
        // Up back to row 1.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 1 });
        // Up to top.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
    }

    #[test]
    fn alarm_list_enter_toggles_enabled_confirmable() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0), alarm(2, 22, 30)]);
        // Move to row 0's alarm (id 1, enabled).
        assert!(state.alarms.alarms[0].enabled);
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Optimistic flip in state.
        assert!(!state.alarms.alarms[0].enabled);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::PersistAlarmToggle { alarms, toggled_id: 1 }
                if alarms.first().is_some_and(|a| !a.enabled)
        )));
        // Confirmable op is tracked as in-flight (one at a time gate).
        assert!(state.pending_alarm_list_edit.is_some());
        // Second ENTER while in-flight is ignored.
        let mid = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(
            mid.is_empty()
                || !mid.iter().any(|b| b
                    .effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistAlarmToggle { .. })))
        );
        // Complete the persist.
        let op = state
            .pending_alarm_list_edit
            .map(|e| e.operation_id)
            .expect("one toggle in flight");
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        assert!(
            state.pending_alarm_list_edit.is_none(),
            "persist confirmed: gate released"
        );
        assert!(!state.alarms.alarms[0].enabled);
    }

    #[test]
    fn alarm_list_enter_on_add_row_opens_sm_picker() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        // Move to the ADD row (index 1) and ENTER: the SM two-stage picker
        // opens (no deferred wedge / persist effect yet).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 1 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Hour,
                hour: 0,
                value: 0,
            })
        );
        assert!(!batches.iter().any(|b| b
            .effects
            .iter()
            .any(|e| matches!(e, Effect::PersistAlarmToggle { .. }))));
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::NumberPick {
                    stage: AddStage::Hour,
                    value: 0,
                },
                ..
            })
        )));
    }

    #[test]
    fn alarm_add_hour_minute_step_and_advance() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        // Open the ADD picker (ADD row index 1).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Step UP wraps 0 -> 23 on the hour stage.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Hour,
                hour: 0,
                value: 23,
            })
        );
        // Step DOWN twice: 23 -> 0 -> 1.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Hour,
                hour: 0,
                value: 1,
            })
        );
        // ENTER locks the hour (1) and advances to the minute stage (0).
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Minute,
                hour: 1,
                value: 0,
            })
        );
        // Minute UP wraps 0 -> 59.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Minute,
                hour: 1,
                value: 59,
            })
        );
        assert!(batches.iter().all(|b| b.effects.iter().all(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Partial,
                ..
            })
        ))));
    }

    #[test]
    fn alarm_add_complete_appends_and_confirms() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Hour: 8 (alarm id 1 uses 8:00; next_id picks a fresh id).
        for _ in 0..8 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Minute,
                hour: 8,
                value: 0,
            })
        );
        // Minute: 30.
        for _ in 0..30 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        let before = state.alarms.alarms.len();
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.alarms.alarms.len(), before + 1, "appended");
        let appended = state.alarms.alarms.last().unwrap().clone();
        assert_eq!((appended.hour, appended.minute), (8, 30));
        assert!(appended.enabled);
        // One confirmable PersistAlarms for the full list.
        assert!(batches
            .iter()
            .any(|b| matches!(b.effects.first(), Some(Effect::PersistAlarms(_)))));
        assert!(state.pending_alarm_add.is_some(), "add in flight");
        // Confirm the persist: releases the add gate and re-arms the RTC.
        let op = state.pending_alarm_add.unwrap();
        let confirm = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Alarms),
            }),
        );
        assert!(state.pending_alarm_add.is_none());
        assert!(confirm.iter().any(|b| matches!(
            b.effects.first(),
            Some(Effect::ProgramRtcAlarm(_)) | Some(Effect::DisableRtcAlarm)
        )));
    }

    #[test]
    fn alarm_add_cancel_returns_to_list() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Long ENTER cancels back to the ADD row (no alarm appended).
        let before = state.alarms.alarms.len();
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.alarms.alarms.len(), before);
        assert_eq!(
            state.screen,
            Screen::AlarmList {
                selected: before, // the ADD row index
            }
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::AlarmList { selected },
                ..
            }) if *selected == before
        )));
    }

    #[test]
    fn alarm_add_persist_failure_pops_appended_alarm() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Hour 8.
        for _ in 0..8 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Minute 0 -> ENTER completes (appends).
        let before = state.alarms.alarms.len();
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.alarms.alarms.len(),
            before + 1,
            "optimistically appended"
        );
        let op = state.pending_alarm_add.unwrap();
        // Fail the persist: the appended alarm pops back.
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Persist("nvs full".into()),
            }),
        );
        assert!(state.pending_alarm_add.is_none());
        assert_eq!(state.alarms.alarms.len(), before, "rollback popped");
    }

    #[test]
    fn alarm_list_long_enter_backs_to_home_and_drawer_is_alarm_origin() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![alarm(1, 8, 0)]);
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
        // Long UP/DOWN opens the drawer with ALARMS highlighted (origin
        // AlarmList) - legacy "you are here" on the alarm page.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 3,
                origin: NavOrigin::AlarmList
            }
        );
        // No-move ENTER (dest 3 = ALARMS) closes back to the alarm list.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
        // Long ENTER from AlarmList backs to Home.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Home,
                ..
            })
        )));
    }

    // ---- TodoList screen (Stage 4) ---------------------------------------

    fn todo(id: u8, text: &str, done: bool) -> Todo {
        Todo {
            id,
            text: text.to_string(),
            done,
            importance: Importance::Medium,
            due_date: None,
            repeat: None,
        }
    }

    fn boot_snapshot_todos(todos: Vec<Todo>, now: Option<DateTime>) -> BootSnapshot {
        let mut snap = boot_snapshot(vec![], now, false, true);
        snap.todos = todos;
        snap
    }

    /// Boots with `todos`, then opens the TODOS drawer destination (index 4)
    /// and selects it.
    fn open_todo_list(state: &mut AppState, todos: Vec<Todo>) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot_todos(todos, Some(dt(9, 0)))),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));
        // Down four times: 0 -> 1 -> 2 -> 3 -> 4 (TODOS).
        for _ in 0..4 {
            let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        }
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 4,
                origin: NavOrigin::Home
            }
        );
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
    }

    #[test]
    fn todo_list_browse_clamps_and_empty_stays_at_zero() {
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![todo(1, "a", false), todo(2, "b", true)]);
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
        // Down to row 1 (last row).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 1 });
        // Down past the end clamps.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 1 });
        // Up to top.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });

        // Empty list: opens at 0, Down clamps at 0.
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![]);
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
    }

    #[test]
    fn todo_list_enter_toggles_done_confirmable() {
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![todo(1, "a", false), todo(2, "b", true)]);
        // Row 0 (id 1, not done).
        assert!(!state.todos.todos[0].done);
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // Optimistic flip.
        assert!(state.todos.todos[0].done);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::PersistTodoEdit { todos, edited_id: 1 }
                if todos.first().is_some_and(|t| t.done)
        )));
        // Gate is up; a second edit is ignored.
        assert!(state.pending_todo_list_edit.is_some());
        let mid = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(
            mid.is_empty()
                || !mid.iter().any(|b| b
                    .effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistTodoEdit { .. })))
        );
        // Confirm the persist.
        let op = state
            .pending_todo_list_edit
            .map(|e| e.operation_id)
            .expect("one edit in flight");
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Todos),
            }),
        );
        assert!(state.pending_todo_list_edit.is_none());
        assert!(state.todos.todos[0].done);
    }

    #[test]
    fn todo_list_long_enter_cycles_importance_confirmable() {
        let mut state = AppState::default();
        // Row 0 starts Medium.
        open_todo_list(&mut state, vec![todo(1, "a", false)]);
        assert_eq!(state.todos.todos[0].importance, Importance::Medium);
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        // Medium -> High.
        assert_eq!(state.todos.todos[0].importance, Importance::High);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::PersistTodoEdit { todos, edited_id: 1 }
                if todos.first().is_some_and(|t| t.importance == Importance::High)
        )));
        let op = state
            .pending_todo_list_edit
            .map(|e| e.operation_id)
            .expect("edit in flight");
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Todos),
            }),
        );
        // High -> Low on the next cycle (confirm the persist so the gate
        // releases before the next edit).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.todos.todos[0].importance, Importance::Low);
        let op = state
            .pending_todo_list_edit
            .map(|e| e.operation_id)
            .expect("edit in flight");
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Todos),
            }),
        );
        // And Low -> Medium.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.todos.todos[0].importance, Importance::Medium);
    }

    #[test]
    fn todo_list_edit_persist_failure_rolls_back_done_and_importance() {
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![todo(1, "a", false)]);
        // Toggle done.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(state.todos.todos[0].done);
        let op = state
            .pending_todo_list_edit
            .map(|e| e.operation_id)
            .unwrap();
        // Fail the persist.
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Persist("nvs full".into()),
            }),
        );
        assert!(
            state.pending_todo_list_edit.is_none(),
            "failed persist releases the gate"
        );
        assert!(!state.todos.todos[0].done, "done rolled back");
        // Long-ENTER cycle then fail: importance rolls back too.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.todos.todos[0].importance, Importance::High);
        let op = state
            .pending_todo_list_edit
            .map(|e| e.operation_id)
            .unwrap();
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Persist("nvs full".into()),
            }),
        );
        assert_eq!(
            state.todos.todos[0].importance,
            Importance::Medium,
            "importance rolled back to pre-edit value"
        );
    }

    #[test]
    fn todo_list_drawer_is_todo_origin_and_exits_via_home() {
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![todo(1, "a", false)]);
        // Long UP/DOWN opens the drawer with TODOS highlighted (origin
        // TodoList).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 4,
                origin: NavOrigin::TodoList
            }
        );
        // Select HOME (move Up 4 times: 4 -> 3 -> 2 -> 1 -> 0, or long UP
        // jumps to 0).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Home,
                ..
            })
        )));
        // Re-enter TODOS, then select TODOS again (no-move ENTER) closes
        // back to the list.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        // Long DOWN from Home-origin drawer: 0 -> last(5); up to 4.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
    }

    // ---- InboxList screen (Stage 4) --------------------------------------

    fn inbox_item(id: u64, title: &str, read: bool) -> InboxItem {
        InboxItem {
            id,
            kind: crate::inbox_item::InboxKind::Info,
            priority: crate::inbox_item::Priority::Normal,
            title: title.to_string(),
            body: String::new(),
            when: None,
            read,
        }
    }

    fn boot_snapshot_inbox(items: Vec<InboxItem>, now: Option<DateTime>) -> BootSnapshot {
        let mut snap = boot_snapshot(vec![], now, false, true);
        snap.inbox = items;
        snap
    }

    /// Boots with `items`, then opens the INBOX drawer destination (index 2)
    /// and selects it.
    fn open_inbox(state: &mut AppState, items: Vec<InboxItem>) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot_inbox(items, Some(dt(9, 0)))),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));
        // Down twice: 0 -> 1 -> 2 (INBOX).
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 2,
                origin: NavOrigin::Home
            }
        );
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
    }

    #[test]
    fn drawer_select_inbox_opens_sm_inbox_list() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot_inbox(vec![], Some(dt(9, 0)))),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Inbox { selected: 0 },
                intent: RenderIntent::Full,
                ..
            })
        )));
    }

    #[test]
    fn inbox_list_browses_and_clamps() {
        let mut state = AppState::default();
        open_inbox(
            &mut state,
            vec![
                inbox_item(1, "a", false),
                inbox_item(2, "b", true),
                inbox_item(3, "c", false),
            ],
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 2 });
        // Down past the end clamps.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 2 });
        // Up to top.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
    }

    #[test]
    fn inbox_enter_row_opens_item_detail_marks_read() {
        let mut state = AppState::default();
        open_inbox(
            &mut state,
            vec![inbox_item(1, "a", false), inbox_item(2, "b", true)],
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 1 });
        // Row 1 (id 2) is already read -> opening emits no read-mark effect.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::InboxItem { index: 1 });
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::MarkInboxRead { .. })));
        // Close back to the list: the detail is read-only, any button.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 1 });
    }

    #[test]
    fn inbox_enter_unread_row_emits_mark_read_and_opens_detail() {
        let mut state = AppState::default();
        open_inbox(
            &mut state,
            vec![inbox_item(1, "a", false), inbox_item(2, "b", true)],
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
        // Row 0 (id 1) is unread: opening marks it read optimistically +
        // fire-and-forgets the local read-mark persist, then shows the
        // detail screen (Full render of its view).
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::InboxItem { index: 0 });
        assert!(state.inbox.items[0].read, "opened item optimistically read");
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::MarkInboxRead { seq: 1 })));
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::InboxItem { index: 0 },
                ..
            })
        )));
    }

    #[test]
    fn inbox_enter_on_empty_list_is_noop() {
        let mut state = AppState::default();
        open_inbox(&mut state, vec![]);
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(!batches.iter().any(|b| b
            .effects
            .iter()
            .any(|e| matches!(e, Effect::MarkInboxRead { .. }))));
        assert!(!batches.iter().any(|b| b.effects.iter().any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::InboxItem { .. },
                ..
            })
        ))));
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
    }

    #[test]
    fn inbox_item_detail_any_button_returns_to_list() {
        let mut state = AppState::default();
        open_inbox(
            &mut state,
            vec![inbox_item(1, "a", false), inbox_item(2, "b", true)],
        );
        // Open row 1 (already read).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::InboxItem { index: 1 });
        // Back to the list so each loop iteration starts from a clean open.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 1 });
        // Any button closes back to the list with the row kept selected.
        let closes = [
            ButtonEvent::Pressed(ButtonId::Up),
            ButtonEvent::Pressed(ButtonId::Down),
            ButtonEvent::LongPressed(ButtonId::Up),
            ButtonEvent::LongPressed(ButtonId::Down),
            ButtonEvent::LongPressed(ButtonId::Enter),
        ];
        for close in closes {
            // From the list (selection kept on row 1), open the detail.
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            assert_eq!(state.screen, Screen::InboxItem { index: 1 });
            // Any button closes back to the list with the row kept selected.
            let batches = update(&mut state, Event::Button(close));
            assert_eq!(state.screen, Screen::Inbox { selected: 1 });
            assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
                e,
                Effect::Render(RenderRequest {
                    intent: RenderIntent::Full,
                    view: RenderView::Inbox { selected: 1 },
                    ..
                })
            )));
        }
    }

    #[test]
    fn inbox_long_enter_backs_home_and_drawer_is_inbox_origin() {
        let mut state = AppState::default();
        open_inbox(&mut state, vec![inbox_item(1, "a", false)]);
        // Long UP opens the drawer with INBOX highlighted (origin Inbox).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 2,
                origin: NavOrigin::Inbox
            }
        );
        // Select HOME (long UP jumps to 0), then ENTER.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Home,
                ..
            })
        )));
        // Re-enter INBOX and select INBOX again (no-move ENTER) returns to
        // the list.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 0 });
    }

    // ---- Calendar screen (Stage 4) ---------------------------------------

    /// Boots at `day` and opens the CALENDAR drawer destination (index 1).
    fn open_calendar(state: &mut AppState, day: u8) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot(
                vec![],
                Some(dt_full(9, 0, 1, day)),
                false,
                true,
            )),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: day,
            })
        );
    }

    #[test]
    fn calendar_day_moves_and_clamps_within_month() {
        let mut state = AppState::default();
        open_calendar(&mut state, 15);
        // Up to day 1 clamps.
        for _ in 0..20 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
            );
        }
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 1,
            })
        );
        // Down to the last day of August (31) clamps.
        for _ in 0..40 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 31,
            })
        );
    }

    #[test]
    fn calendar_enter_day_opens_week_view_screen() {
        let mut state = AppState::default();
        open_calendar(&mut state, 15);
        // Move to day 17 then ENTER.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 17,
            })
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        // ENTER opens the day's week view as an SM screen (Full render of
        // the WeekView surface).
        assert_eq!(
            state.screen,
            Screen::WeekView {
                year: 2026,
                month: 8,
                day: 17,
            }
        );
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::WeekView {
                    year: 2026,
                    month: 8,
                    day: 17,
                },
                ..
            })
        )));
        // Any button closes the read-only view back to the calendar grid,
        // keeping the opened day as the cursor.
        let close_batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 17,
            })
        );
        assert!(close_batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(
                e,
                Effect::Render(RenderRequest {
                    intent: RenderIntent::Full,
                    view: RenderView::Calendar {
                        year: 2026,
                        month: 8,
                        selected_day: 17,
                    },
                    ..
                })
            )));
    }

    #[test]
    fn calendar_long_enter_backs_home_and_drawer_is_calendar_origin() {
        let mut state = AppState::default();
        open_calendar(&mut state, 15);
        // Long UP opens the drawer with CALENDAR highlighted (origin
        // Calendar).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(
            state.screen,
            Screen::Navigation {
                selected: 1,
                origin: NavOrigin::Calendar
            }
        );
        // Select HOME (long UP jumps to 0) -> Home.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                intent: RenderIntent::Full,
                view: RenderView::Home,
                ..
            })
        )));
        // Re-enter CALENDAR: the grid re-opens with the cursor on today.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 15,
            })
        );
    }

    #[test]
    fn calendar_follows_cross_month_tick() {
        let mut state = AppState::default();
        // Open on Aug 31.
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(
                vec![],
                Some(dt_full(23, 59, 1, 31)),
                false,
                true,
            )),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 31,
            })
        );
        // A tick into September refreshes year/month and clamps the day
        // (the cursor was the 31st, which September lacks -> 30).
        let _ = update(
            &mut state,
            Event::Tick(DateTime {
                year: 2026,
                month: 9,
                day: 1,
                weekday: 2,
                hour: 0,
                minute: 1,
                second: 0,
                voltage_low: false,
            }),
        );
        assert_eq!(
            state.screen,
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 9,
                selected_day: 30,
            })
        );
    }

    #[test]
    fn still_legacy_screens_project_to_home_surface_until_migrated() {
        // Stage-4 transitional projection: every screen still drawn through
        // a legacy blocking page maps to the Home surface so the executor
        // keeps rendering Home behind/around the legacy page. AlarmRinging
        // is covered by the legacy ring screen, which draws over whatever
        // the SM render produced.
        let cases: Vec<(Screen, RenderView)> = vec![
            (Screen::Home, RenderView::Home),
            (
                Screen::Navigation {
                    selected: 0,
                    origin: NavOrigin::Home,
                },
                RenderView::Navigation { selected: 0 },
            ),
            (
                Screen::Navigation {
                    selected: 3,
                    origin: NavOrigin::Home,
                },
                RenderView::Navigation { selected: 3 },
            ),
            (
                Screen::Settings { selected: 0 },
                RenderView::Settings { selected: 0 },
            ),
            (
                Screen::Settings { selected: 3 },
                RenderView::Settings { selected: 3 },
            ),
            (
                Screen::Calendar(CalendarState {
                    year: 2026,
                    month: 8,
                    selected_day: 15,
                }),
                RenderView::Calendar {
                    year: 2026,
                    month: 8,
                    selected_day: 15,
                },
            ),
            (
                Screen::WeekView {
                    year: 2026,
                    month: 8,
                    day: 17,
                },
                RenderView::WeekView {
                    year: 2026,
                    month: 8,
                    day: 17,
                },
            ),
            (
                Screen::AlarmAdd(AlarmAddState {
                    stage: AddStage::Minute,
                    hour: 8,
                    value: 30,
                }),
                RenderView::NumberPick {
                    stage: AddStage::Minute,
                    value: 30,
                },
            ),
            (
                Screen::AlarmAdd(AlarmAddState {
                    stage: AddStage::Minute,
                    hour: 8,
                    value: 30,
                }),
                RenderView::NumberPick {
                    stage: AddStage::Minute,
                    value: 30,
                },
            ),
            (
                Screen::AlarmList { selected: 0 },
                RenderView::AlarmList { selected: 0 },
            ),
            (
                Screen::TodoList { selected: 0 },
                RenderView::TodoList { selected: 0 },
            ),
            (
                Screen::Inbox { selected: 0 },
                RenderView::Inbox { selected: 0 },
            ),
            (Screen::AlarmRinging, RenderView::Home),
            (
                Screen::BlePairing(BlePairingState {
                    phase: BlePairingPhase::Waiting,
                }),
                RenderView::BlePairing,
            ),
        ];
        for (screen, view) in cases {
            assert_eq!(screen.render_view(), view, "screen {screen:?}");
        }
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
        let dismiss_batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
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

    /// An alarm ringing over an SM content screen must dismiss back to that
    /// screen (Stage-4 semantics: `screen_before_ring` captures the current
    /// SM screen, not just Home).
    #[test]
    fn alarm_ringing_over_sm_screens_restores_them_on_dismiss() {
        // Each SM screen, current over a boot that armed a 9:00 Daily alarm.
        let screens = [
            Screen::Settings { selected: 2 },
            Screen::AlarmList { selected: 1 },
            Screen::TodoList { selected: 0 },
            Screen::Inbox { selected: 0 },
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 8,
                selected_day: 15,
            }),
            Screen::WeekView {
                year: 2026,
                month: 8,
                day: 17,
            },
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Minute,
                hour: 8,
                value: 30,
            }),
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
            }),
        ];
        for screen in screens {
            let mut state = AppState::default();
            let _ = update(
                &mut state,
                Event::Boot(boot_snapshot(
                    vec![alarm(1, 9, 0)],
                    Some(dt(8, 0)),
                    false,
                    true,
                )),
            );
            // Jump straight to the content screen (the SM owns the screen
            // field; only the ring preemption below exercises it).
            state.screen = screen.clone();
            let before = state.screen.clone();
            // The 9:00 Daily alarm fires.
            let _ring_batches = update(
                &mut state,
                Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                    now: dt(9, 0),
                    alarm_flag: true,
                    alarm_interrupt_enabled: true,
                }),
            );
            assert_eq!(
                state.screen,
                Screen::AlarmRinging,
                "{screen:?} must be preempted into AlarmRinging"
            );
            // Dismiss with ENTER.
            let dismiss_batches = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            assert_eq!(
                state.screen, before,
                "dismiss from {screen:?} must restore the pre-ring screen"
            );
            assert!(dismiss_batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::StopTone)));
            // The restored screen is re-rendered with its own view (Full).
            assert!(dismiss_batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(
                    e,
                    Effect::Render(RenderRequest {
                        intent: RenderIntent::Full,
                        ..
                    })
                )));
        }
    }

    /// The unified event loop keeps running while any SM screen is current:
    /// a minute-change Tick still drives the SM on every screen (Stage-4
    /// exit condition - no page blocks the loop). Time-sensitive screens
    /// (Home clock, list data, Calendar dates, TodoList due-today) re-render
    /// their view Partial; pure read-only sub-screens (week view, item
    /// detail, ADD picker, interval picker, pairing session) have no
    /// time-driven content and skip the e-ink refresh.
    #[test]
    fn minute_tick_keeps_rendering_while_any_sm_screen_is_current() {
        // (screen, expected partial-render view) - time-sensitive screens.
        let refresh_screens = [
            (Screen::Home, RenderView::Home),
            (
                Screen::Settings { selected: 0 },
                RenderView::Settings { selected: 0 },
            ),
            (
                Screen::AlarmList { selected: 1 },
                RenderView::AlarmList { selected: 1 },
            ),
            (
                Screen::TodoList { selected: 0 },
                RenderView::TodoList { selected: 0 },
            ),
            (
                Screen::Inbox { selected: 0 },
                RenderView::Inbox { selected: 0 },
            ),
            (
                Screen::Calendar(CalendarState {
                    year: 2026,
                    month: 8,
                    selected_day: 15,
                }),
                RenderView::Calendar {
                    year: 2026,
                    month: 8,
                    selected_day: 15,
                },
            ),
            (
                Screen::Navigation {
                    selected: 3,
                    origin: NavOrigin::Home,
                },
                RenderView::Navigation { selected: 3 },
            ),
        ];
        for (screen, expected_view) in refresh_screens {
            let mut state = AppState::default();
            let _ = update(
                &mut state,
                Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
            );
            state.screen = screen.clone();
            let tick_batches = update(&mut state, Event::Tick(dt(8, 1)));
            assert!(
                tick_batches
                    .iter()
                    .flat_map(|b| &b.effects)
                    .any(|e| matches!(
                        e,
                        Effect::Render(RenderRequest {
                            intent: RenderIntent::Partial,
                            view,
                            ..
                        }) if *view == expected_view
                    )),
                "tick on {screen:?} must Partial-render {expected_view:?}"
            );
        }

        // Pure read-only sub-screens (and the legacy ring overlay) skip
        // the minute render entirely.
        let skip_screens = [
            Screen::AlarmRinging,
            Screen::WeekView {
                year: 2026,
                month: 8,
                day: 17,
            },
            Screen::InboxItem { index: 0 },
            Screen::AlarmAdd(AlarmAddState {
                stage: AddStage::Minute,
                hour: 8,
                value: 30,
            }),
            Screen::SyncIntervalPick { selected: 0 },
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
            }),
        ];
        for screen in skip_screens {
            let mut state = AppState::default();
            let _ = update(
                &mut state,
                Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
            );
            state.screen = screen.clone();
            let tick_batches = update(&mut state, Event::Tick(dt(8, 1)));
            assert!(
                !tick_batches.iter().any(|b| b
                    .effects
                    .iter()
                    .any(|e| matches!(e, Effect::Render(RenderRequest { .. })))),
                "tick on read-only {screen:?} must not render"
            );
        }
    }
}
