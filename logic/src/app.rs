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
use crate::power_state::{SleepInputs, SleepKind, SleepState, SleepToken};
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
    Navigation {
        selected: usize,
        /// Which screen the drawer was opened from: cancelling the drawer
        /// (long ENTER) restores it, and the renderer draws it underneath
        /// the drawer while it is open.
        origin: NavOrigin,
        /// The complete state-machine screen shown underneath the drawer.
        /// This preserves cursors and other visible state, such as the
        /// selected day in Calendar.
        screen_before: Box<Screen>,
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
    /// A full-screen reminder overlay (urgent inbox or due-todo). Carries
    /// the final visible lines the renderer draws - the executor does not
    /// re-read stores, because urgent items were already marked read when
    /// the fact was raised. The reminder is transient: ENTER dismisses it
    /// (StopTone + restore + Full render), a Tick past its deadline does
    /// the same, and an RTC alarm preempts it.
    Reminder(ReminderState),
    BlePairing(BlePairingState),
}

/// The two reminder classes. Which one is showing changes the tone and the
/// renderer's header, but not the lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderKind {
    /// Urgent inbox messages (siren).
    Urgent,
    /// High-importance todos due today (beep).
    Todo,
}

/// A transient full-screen reminder overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderState {
    pub kind: ReminderKind,
    /// The final visible content lines (already deduped / read-marked by the
    /// fact layer when the fact was raised).
    pub lines: Vec<String>,
    /// Screen shown underneath; ENTER / timeout restores it. Boxed to break
    /// the Screen -> ReminderState -> Screen recursion.
    pub screen_before: Box<Screen>,
    /// Wall-clock unix deadline at which the reminder auto-dismisses (a
    /// Tick event, never a blocking timer).
    pub deadline_unix: u64,
}

/// The navigation destination row associated with a drawer origin.
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
    /// `AlarmRinging` renders its own full-frame alarm view; the SM owns
    /// the whole lifecycle (Firing state, ENTER dismiss, ring-deadline
    /// timeout) and there is no legacy blocking ring screen anymore.
    pub fn render_view(&self) -> RenderView {
        match self {
            Screen::Navigation {
                selected,
                screen_before,
                ..
            } => RenderView::Navigation {
                selected: *selected,
                underlying: Box::new(screen_before.render_view()),
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
            Screen::AlarmRinging => RenderView::AlarmRinging,
            Screen::Reminder(ReminderState { kind, lines, .. }) => RenderView::Reminder {
                kind: *kind,
                lines: lines.clone(),
            },
            Screen::Home => RenderView::Home,
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
    /// Absolute wall-clock second at which the pairing session times out
    /// (Stage-5/6: the timeout is an event - the collection layer emits a
    /// Tick past this deadline - never a blocking-loop timer). None when no
    /// session deadline is armed (e.g. tests construct states directly).
    pub pairing_deadline_unix: Option<u64>,
    /// Monotonic session id used to discard a late worker result after the
    /// user has exited and re-entered pairing.
    pub session_id: u64,
    /// The key state must pass through a release after entering the screen
    /// before a long ENTER may cancel it. This prevents the long-press event
    /// for the key that opened pairing from immediately closing it again.
    pub input_released: bool,
}

impl Default for BlePairingState {
    fn default() -> Self {
        Self {
            phase: BlePairingPhase::Waiting,
            pairing_deadline_unix: None,
            session_id: 0,
            input_released: false,
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncSchedulerConfig {
    pub now_unix: u64,
    pub interval_minutes: u16,
    pub last_sync_epoch: Option<u64>,
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
    /// Invalidate the stale RTC alignment marker after a host-set or NTP
    /// time write (SetRtc, time shift). Fire-and-forget: the executor
    /// removes the NVS key; the machine does not track the value.
    ClearRtcAlignEpoch,
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
    /// Probe the server for urgent content; completion returns as a fact.
    PollUrgent,
    /// Collect reminder data from the persistence worker. The application
    /// receives the resulting payload as an effect completion and never reads
    /// NVS while collecting runtime events.
    CollectReminderFacts(DateTime),
    /// Ask the network executor to verify Wi-Fi credentials (it owns the
    /// Wi-Fi driver). A successful verification returns a fact; persistence
    /// is a separate confirmable effect.
    StartSetWifi(WifiCreds),
    /// Persist credentials after the network executor has verified them.
    PersistWifiCredentials(WifiCreds),
    StartTone,
    /// Start the reminder's attention tone (siren for urgent, beep for
    /// todo). Distinct from StartTone so the executor can pick the right
    /// pattern; stopped by StopTone like the alarm ring.
    StartReminderTone(ReminderKind),
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
    /// Disable the platform's light-sleep mode after activity wakes the
    /// committed light-sleep session.
    DisableLightSleep,
    EnterDeepSleep(WakeupPlan),
    PrepareSleep {
        token: SleepToken,
        /// Deep-sleep maintenance timer selected during admission. It is
        /// carried into the platform prepare step so wake configuration is
        /// complete before the token can be committed.
        maintenance: Option<std::time::Duration>,
        light_wake_after_ms: u64,
    },
    CommitSleep(SleepToken),
    /// The user opened an inbox item (ENTER on an Inbox row). Fire-and-forget
    /// local persistence of the read mark: the executor marks `seq` read and
    /// adds it to the pending-read set (two-way sync uploads it later). The
    /// state machine optimistically set the item's `read` flag when the
    /// detail screen opened; there is no confirmable completion to track.
    MarkInboxRead {
        /// The inbox item `seq` to mark read (from `state.inbox.items`).
        seq: u64,
    },
    /// Persist the reminder de-duplication facts before presenting the
    /// reminder overlay. The effect is kept as one worker-safe operation so
    /// a slow NVS write never runs on the application loop.
    PersistReminder(ReminderPersistence),
    /// The user confirmed a SYNC INTERVAL choice on the SM picker (Stage 4).
    /// Persist the sync interval after the state machine updates its
    /// scheduler cursor.
    SetSyncInterval {
        /// Interval in minutes (one of the fixed five: 1/5/10/30/60).
        minutes: u16,
    },
}

/// Which visible surface a render request targets. The renderer draws this
/// surface (not a call-site rectangle) and refreshes the panel rect the
/// surface maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderView {
    /// The idle home canvas (clock + cards), with no overlay.
    Home,
    /// The GO TO drawer overlay over the complete source screen. Carries the
    /// drawer's selected row and the source view so the executor can redraw
    /// the correct background before overlaying the bar.
    Navigation {
        selected: usize,
        underlying: Box<RenderView>,
    },
    /// The settings list (Stage 4): rows browsed in the state machine,
    /// row actions deferred to executor wedges. Carries the selected row.
    Settings { selected: usize },
    /// The alarm list (Stage 4): rows browsed in the state machine, ENTER
    /// toggles a row's enabled flag (confirmable SM edit); the trailing
    /// "+ ADD ALARM" row is a deferred executor wedge. Carries the selected
    /// row; the executor draws the rows from the collected AppState snapshot.
    AlarmList { selected: usize },
    /// The todo list (Stage 4): rows browsed in the state machine; short
    /// ENTER toggles a row's done flag and long ENTER returns Home. Carries
    /// the selected row.
    TodoList { selected: usize },
    /// The inbox list (Stage 4): rows browsed in the state machine, ENTER
    /// opens an item's detail (an SM screen). Carries the selected row; the
    /// executor draws the rows from the collected AppState snapshot.
    Inbox { selected: usize },
    /// The inbox item detail (Stage 4): title + body of the selected item,
    /// drawn by the executor from the collected AppState snapshot (carries only the index). ENTER
    /// on an Inbox row opens it (and marks it read); any button closes it
    /// back to the list.
    InboxItem { index: usize },
    /// The calendar month grid (Stage 4): a read-only grid of the current
    /// month with a day cursor the state machine moves; ENTER opens the
    /// day's week view through a deferred render. Carries the year /
    /// month and selected day so the executor can draw the grid from state
    /// (day marks come from the AppState snapshot).
    Calendar {
        year: u16,
        month: u8,
        selected_day: u8,
    },
    /// The week view (Stage 4): the Sun-Sat week containing the opened day,
    /// one column per day with each day's open todos from the AppState
    /// snapshot. Full refresh on open/close.
    WeekView { year: u16, month: u8, day: u8 },
    /// The ADD-ALARM number picker (Stage 4): a big digit stepped by
    /// UP/DOWN (wrapping). Carries the active stage (the executor maps it
    /// to the title + range) and the current value.
    NumberPick { stage: AddStage, value: u8 },
    /// The SYNC INTERVAL picker list (Stage 4): five fixed options, row
    /// cursor moved by the SM, ENTER confirms. Carries the selected row.
    SyncInterval { selected: usize },
    /// The BLE pairing session (Stage 4): the pairing-instructions screen.
    /// The SM owns the lifecycle (events drive the phase; long ENTER exits
    /// back to Settings); the executor draws the instructions canvas. The
    /// phase-specific visuals stay with the radio layer until the NimBLE
    /// event wiring lands (the SM phase transitions are locked regardless).
    BlePairing,
    /// The non-blocking alarm ring screen (Stage 5/6): shown while
    /// `AlarmRuntimeState::Firing` and the ring overlay is up. The executor
    /// draws the alarm frame; ENTER (via Event::Button) and the ring-deadline
    /// Tick both leave through the SM's `dismiss_ringing`. No legacy blocking
    /// ring owns this frame anymore.
    AlarmRinging,
    /// The full-screen reminder overlay (urgent or todo). Carries the final
    /// visible lines to draw; the executor draws them verbatim.
    Reminder {
        kind: ReminderKind,
        lines: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub generation: RenderGeneration,
    /// The surface to draw. Stage-5: `view` stays as the renderer's "what
    /// screen am I drawing" input until every screen is SM-drawn; the
    /// refresh plan (Noop/Partial/Full) is derived from the ViewModel diff,
    /// never chosen by the caller.
    pub view: RenderView,
    /// The visible-state snapshot this request was projected from
    /// (Stage 5). The renderer compares it with its privately-held previous
    /// ViewModel via `plan_render` to decide Noop / Partial / Full; it is
    /// NOT a rendering payload (the executor draws from the AppState snapshot), only
    /// the diff input.
    pub view_model: crate::render_plan::ViewModel,
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
    pub session_id: u64,
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
    /// SetWifi's verified credential NVS write.
    WifiCredentials,
}

/// Outcome of a completed effect, fed back to `update` via
/// `Event::EffectCompleted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutput {
    RenderDone,
    ReminderFacts(Option<ReminderPayload>),
    ReminderPersisted,
    SyncDone(SyncResult),
    Persisted(PersistTarget),
    RtcProgrammed,
    /// A `WriteRtcTime` (SetTimezone/SetRtc clock write) confirmed.
    /// Carries the time that was actually written so the state machine can
    /// re-derive the hardware alarm from the correct instant.
    RtcTimeWritten(DateTime),
    AckDone,
    ToneDone,
    BlePairingDone,
    LightSleepEntered,
    LightSleepDisabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncResult {
    Ok {
        /// Merged server data the state machine applies (persist is
        /// ordered by the machine; the network task never writes NVS).
        data: SyncedData,
    },
    OkWithMetadata {
        data: SyncedData,
        last_sync_epoch: u64,
        ntp_epoch: Option<u64>,
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
    /// Alarm `local_id`s that were uploaded in this sync snapshot. On apply,
    /// only these IDs are cleared from the dirty set so edits made during
    /// the network round-trip survive (P1-3 race fix).
    pub uploaded_alarm_ids: Vec<u8>,
    /// Todo `local_id`s uploaded in this sync snapshot — same semantics.
    pub uploaded_todo_ids: Vec<u8>,
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

/// Status-visible Wi-Fi facts confirmed by a background credential
/// verification. Carries no password; the sync task already persisted the
/// credentials before emitting this fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiConfigApplied {
    pub ssid: String,
    pub has_password: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingResult {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlePairingFailure {
    pub message: String,
}

/// Mechanical facts collected by the platform loop. Business facts such as
/// the current page, pending replies and RTC plan are filled from `AppState`
/// when `PowerPoll` is reduced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerPoll {
    pub now_ticks: u64,
    pub activity_observed: bool,
    pub final_display_pending: bool,
    pub final_persist_pending: bool,
    pub usb_connected: bool,
    pub event_queue_empty: bool,
    pub input_latch_clear: bool,
    pub wake_plan_confirmed: bool,
    pub network_resumable: bool,
    pub light_wake_after_ms: u64,
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
    /// The reminder fact layer raised a full-screen reminder (urgent inbox
    /// alerts or high-importance todos due now). The payload carries the
    /// *final* visible lines - the fact layer already applied urgent-read /
    /// todo-date persistence before raising - so the SM never re-reads
    /// stores or decides presentation on stale data.
    ReminderDue(ReminderPayload),
    SyncCompleted(SyncResult),
    /// The network worker completed SetWifi credential verification. The
    /// state machine emits `PersistWifiCredentials` only for the Ok fact.
    SetWifiVerified(Result<WifiCreds, String>),
    /// The network worker completed the lightweight urgent-content probe.
    /// The application decides whether this fact warrants a full sync.
    UrgentPollCompleted {
        available: bool,
    },
    /// The urgent probe failed before producing a server answer. The
    /// scheduler keeps that wall-clock boundary retryable.
    UrgentPollFailed,
    /// The persistence worker completed runtime reminder fact collection.
    ReminderFacts(Option<ReminderPayload>),
    /// A background credential verification (currently BLE handoff after the
    /// reply/disconnect window) confirmed the Wi-Fi config was saved.
    WifiConfigApplied(WifiConfigApplied),
    /// Mechanical sleep facts from the platform loop. `update` supplies the
    /// business-owned sleep gates and starts a tokenized prepare handshake.
    PowerPoll(PowerPoll),
    /// Platform completed the prepare handshake for the matching token.
    SleepPrepared {
        token: SleepToken,
        inputs: SleepInputs,
    },
    /// Platform completed the commit handshake for the matching token.
    SleepCommitted(SleepToken),
    /// Cancels a pending sleep handshake; normally emitted by platform code
    /// when it observes a new activity fact before commit.
    SleepCancelled(SleepToken),
    /// The wall-clock full-sync boundary is due (the event-collection layer
    /// detected the minute crossed the interval boundary; it only reports
    /// the fact - the state machine decides whether a sync may start).
    SyncBoundaryDue,
    /// Persisted scheduler inputs collected by the platform during boot.
    SyncSchedulerConfigured(SyncSchedulerConfig),
    EffectCompleted(EffectCompletion),
    EffectFailed(EffectFailure),
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
        /// Wall-clock unix second at which the ring must auto-dismiss if the
        /// user has not pressed ENTER. The timeout is an *event*: the
        /// collection layer keeps emitting Ticks and `transition_tick`
        /// compares them against this deadline (same shape as the BLE
        /// pairing deadline) - never a blocking-loop timer. Mirrors the
        /// legacy `MAX_RING_SECS` safety bound so an unattended alarm cannot
        /// ring forever.
        ring_deadline_unix: Option<u64>,
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
    pub sync_scheduler: crate::scheduler::SyncScheduler,
    pub connectivity: ConnectivityState,
    /// Device configuration (server endpoint + Wi-Fi presence flags). Holds
    /// only what the UI/status layer needs; the secrets (auth token,
    /// Wi-Fi password) are never stored in `AppState` - the firmware keeps
    /// them in NVS and only reports booleans.
    pub config: ConfigState,
    pub pending_usb_reply: Option<PendingReply>,
    pub pending_ble_reply: Option<PendingReply>,
    /// Reminder persistence is completed before its tone/render batch is
    /// released. This keeps reminder NVS writes off the app loop while
    /// retaining the visible overlay state.
    pending_reminder_operation: Option<OperationId>,
    pending_reminder_facts: Option<OperationId>,
    /// Token lifecycle for shallow/deep sleep admission. The platform owns
    /// only mechanical facts; this state remains the business owner.
    pub sleep: SleepState,
    /// Manual Settings sleep and idle sleep requests wait for the next
    /// `PowerPoll`, which supplies the platform facts needed for admission.
    pub requested_sleep: Option<SleepKind>,
    pending_sleep_inputs: Option<SleepInputs>,
    pending_sleep_light_wake_after_ms: u64,
    pending_sleep_maintenance: Option<std::time::Duration>,
    pending_sleep_page_allows: bool,
    /// Operation id for the current platform sleep handshake/effect. Sleep
    /// failures must match this id so a stale worker notice cannot cancel a
    /// newer token.
    pending_sleep_operation: Option<OperationId>,
    pending_light_sleep_disable: Option<OperationId>,
    last_activity_ticks: Option<u64>,
    idle_since_ticks: Option<u64>,
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
    /// An in-flight TodoList done-toggle (Stage 4). The matching
    /// `Persisted(Todos)` completion confirms it (the executor marks it
    /// dirty); a failure rolls the row back to its previous state. One edit
    /// at a time.
    pending_todo_list_edit: Option<PendingTodoListEdit>,
    pending_sync_metadata: Option<PendingSyncMetadata>,
    pending_sync_data: Option<SyncedData>,
    pending_sync_rtc_op: Option<OperationId>,
    pending_sync_apply_op: Option<OperationId>,
    pending_sync_metadata_op: Option<OperationId>,
    pending_urgent_poll: Option<OperationId>,
    retries: Vec<RetryPending>,
}

/// An in-flight single-row done-toggle from the TodoList screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingTodoListEdit {
    operation_id: OperationId,
    index: usize,
    previous_done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSyncMetadata {
    last_sync_epoch: u64,
    ntp_epoch: Option<u64>,
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
            sync_scheduler: crate::scheduler::SyncScheduler::new(0, 60, None),
            connectivity: ConnectivityState::default(),
            config: ConfigState::default(),
            pending_usb_reply: None,
            pending_ble_reply: None,
            pending_reminder_operation: None,
            pending_reminder_facts: None,
            sleep: SleepState::new(),
            requested_sleep: None,
            pending_sleep_inputs: None,
            pending_sleep_light_wake_after_ms: 1_000,
            pending_sleep_maintenance: None,
            pending_sleep_page_allows: false,
            pending_sleep_operation: None,
            pending_light_sleep_disable: None,
            last_activity_ticks: None,
            idle_since_ticks: None,
            pending_residue_ack: None,
            pending_residue_time: None,
            pending_alarm_list_edit: None,
            pending_alarm_add: None,
            pending_todo_list_edit: None,
            pending_sync_metadata: None,
            pending_sync_data: None,
            pending_sync_rtc_op: None,
            pending_sync_apply_op: None,
            pending_sync_metadata_op: None,
            pending_urgent_poll: None,
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

    /// True while the lightweight urgent-content probe is still awaiting its
    /// completion fact. The probe is an in-flight network operation even
    /// though `SyncState` remains `Idle` until the probe elects a full sync.
    pub fn urgent_poll_in_flight(&self) -> bool {
        self.pending_urgent_poll.is_some()
    }
}

/// The single state transition entry point. Mutates `state` and returns the
/// effects the side-effect execution layer must run.
pub fn update(state: &mut AppState, event: Event) -> Vec<EffectBatch> {
    let tick_cancels_prepared =
        matches!(event, Event::Tick(_)) && state.sleep.prepared_token().is_some();
    let tick_wakes_light_sleep =
        matches!(event, Event::Tick(_)) && state.sleep.committed_kind() == Some(SleepKind::Light);
    let light_sleep_was_committed = state.sleep.committed_kind() == Some(SleepKind::Light)
        && (sleep_activity_event(&event) || tick_wakes_light_sleep);
    let mut wake_effects = if light_sleep_was_committed {
        disable_light_sleep_batch(state)
    } else {
        Vec::new()
    };
    if sleep_activity_event(&event) || tick_cancels_prepared || tick_wakes_light_sleep {
        state.sleep.activity();
        state.pending_sleep_operation = None;
        // A real application event invalidates an in-flight handshake.  A
        // pending manual request is deliberately retained across the button
        // release that follows Settings/SLEEP and the next Tick; those are
        // bookkeeping, not new user intent. A real event clears it below.
        state.requested_sleep = None;
        if state.pending_sleep_inputs.is_some() {
            state.pending_sleep_inputs = None;
            state.pending_sleep_page_allows = false;
            state.pending_sleep_maintenance = None;
        }
    }
    let mut batches = match event {
        Event::Boot(snapshot) => transition_boot(state, snapshot),
        Event::RtcAlarmSnapshotReady(snapshot) => transition_rtc_snapshot(state, snapshot),
        Event::Tick(now) => transition_tick(state, now),
        Event::Button(button) => transition_button(state, button),
        Event::EffectCompleted(completion) => transition_effect_completed(state, completion),
        Event::EffectFailed(failure) => transition_effect_failed(state, failure),
        Event::UsbCommand(request) => transition_command(state, Channel::Usb, request),
        Event::BleCommand(request) => transition_command(state, Channel::Ble, request),
        Event::SyncCompleted(result) => transition_sync_completed(state, result),
        Event::SetWifiVerified(result) => transition_set_wifi_verified(state, result),
        Event::UrgentPollCompleted { available } => {
            transition_urgent_poll_completed(state, available)
        }
        Event::UrgentPollFailed => transition_urgent_poll_failed(state),
        Event::ReminderFacts(payload) => payload
            .map(|payload| transition_reminder_due(state, payload))
            .unwrap_or_default(),
        Event::WifiConfigApplied(fact) => transition_wifi_config_applied(state, fact),
        Event::PowerPoll(poll) => transition_power_poll(state, poll),
        Event::SleepPrepared { token, inputs } => transition_sleep_prepared(state, token, inputs),
        Event::SleepCommitted(token) => transition_sleep_committed(state, token),
        Event::SleepCancelled(token) => transition_sleep_cancelled(state, token),
        Event::SyncBoundaryDue => transition_sync_boundary_due(state),
        Event::SyncSchedulerConfigured(config) => {
            state.sync_scheduler = crate::scheduler::SyncScheduler::new(
                config.now_unix,
                config.interval_minutes,
                config.last_sync_epoch,
            );
            state.pending_urgent_poll = None;
            vec![]
        }
        Event::BlePairingStarted => transition_ble_pairing_started(state),
        Event::BlePairingSucceeded(result) => transition_ble_pairing_succeeded(state, result),
        Event::BlePairingFailed(failure) => transition_ble_pairing_failed(state, failure),
        Event::BleDisconnected => transition_ble_disconnected(state),
        Event::ReminderDue(payload) => transition_reminder_due(state, payload),
    };
    wake_effects.append(&mut batches);
    wake_effects
}

fn sleep_activity_event(event: &Event) -> bool {
    match event {
        Event::PowerPoll(_)
        | Event::Tick(_)
        | Event::SleepPrepared { .. }
        | Event::SleepCommitted(_)
        | Event::SleepCancelled(_) => false,
        Event::EffectCompleted(completion)
            if matches!(
                completion.output,
                EffectOutput::LightSleepEntered | EffectOutput::LightSleepDisabled
            ) =>
        {
            false
        }
        // Sleep failures are control-plane results, not new activity. The
        // transition below cancels only when the failure matches the current
        // sleep operation, preserving a newer token from stale notices.
        Event::EffectFailed(failure) if matches!(failure.error, EffectError::Sleep(_)) => false,
        Event::Button(button) => matches!(
            button,
            ButtonEvent::Pressed(_) | ButtonEvent::LongPressed(_)
        ),
        _ => true,
    }
}

fn app_sleep_inputs(state: &AppState, poll: PowerPoll) -> SleepInputs {
    let network_in_flight = state.sync != SyncState::Idle;
    let operation_in_flight = state.pending_rtc.is_some()
        || state.pending_light_sleep_disable.is_some()
        || state.pending_residue_ack.is_some()
        || state.pending_alarm_list_edit.is_some()
        || state.pending_alarm_add.is_some()
        || state.pending_todo_list_edit.is_some()
        || state.pending_sync_data.is_some()
        || state.pending_sync_metadata.is_some()
        || state.pending_sync_rtc_op.is_some()
        || state.pending_sync_apply_op.is_some()
        || state.pending_sync_metadata_op.is_some()
        || state.pending_urgent_poll.is_some()
        || !state.retries.is_empty();
    SleepInputs {
        input_pending: false,
        page_allows_sleep: matches!(state.screen, Screen::Home)
            || state.requested_sleep.is_some()
            || state.pending_sleep_page_allows,
        final_display_pending: poll.final_display_pending,
        final_persist_pending: poll.final_persist_pending,
        network_in_flight,
        protocol_reply_pending: state.pending_usb_reply.is_some()
            || state.pending_ble_reply.is_some(),
        operation_in_flight,
        rtc_plan_confirmed: state.rtc_alarm_plan_confirmed(),
        wake_plan_confirmed: poll.wake_plan_confirmed,
        usb_connected: poll.usb_connected,
        event_queue_empty: poll.event_queue_empty,
        input_latch_clear: poll.input_latch_clear,
        network_resumable: poll.network_resumable,
    }
}

fn transition_power_poll(state: &mut AppState, poll: PowerPoll) -> Vec<EffectBatch> {
    if poll.activity_observed {
        state.last_activity_ticks = Some(poll.now_ticks);
        state.idle_since_ticks = None;
        // The poll is sampled after the button/transport queues.  It may
        // therefore describe the same activity that requested manual sleep.
        // Preserve a manual request; an already prepared token is cancelled
        // because this is a genuinely new platform activity observation.
        let disable = state.sleep.committed_kind() == Some(SleepKind::Light);
        if state.sleep.prepared_token().is_some() || state.sleep.committed_kind().is_some() {
            state.sleep.activity();
            state.pending_sleep_operation = None;
            state.pending_sleep_inputs = None;
            state.pending_sleep_page_allows = false;
            state.pending_sleep_maintenance = None;
        }
        return if disable {
            disable_light_sleep_batch(state)
        } else {
            vec![]
        };
    }
    if state.sleep.committed_kind().is_some() {
        return vec![];
    }
    if state.pending_sleep_inputs.is_some() {
        return vec![];
    }
    let last = *state.last_activity_ticks.get_or_insert(poll.now_ticks);
    let idle_for = poll.now_ticks.saturating_sub(last);
    let idle_since = if idle_for >= 2_000 {
        *state.idle_since_ticks.get_or_insert(last + 2_000)
    } else {
        return vec![];
    };
    let automatic_kind = (poll.now_ticks.saturating_sub(idle_since) >= 300_000)
        .then_some(SleepKind::Deep)
        .or(Some(SleepKind::Light));
    let kind = state.requested_sleep.or(automatic_kind);
    let Some(kind) = kind else {
        return vec![];
    };
    let inputs = app_sleep_inputs(state, poll);
    let maintenance = (kind == SleepKind::Deep)
        .then(|| {
            state
                .clock
                .now
                .and_then(|now| maintenance_wakeup_delay(&state.alarms.alarms, &now))
                .map(|delay| delay.min(std::time::Duration::from_secs(600)))
                .or(Some(std::time::Duration::from_secs(600)))
        })
        .flatten();
    let manual_request = state.requested_sleep.is_some();
    state.requested_sleep = None;
    let Ok(token) = state.sleep.prepare(kind, poll.now_ticks, inputs) else {
        if manual_request {
            state.requested_sleep = Some(kind);
        }
        return vec![];
    };
    state.pending_sleep_inputs = Some(inputs);
    state.pending_sleep_operation = Some(state.next_operation_id());
    state.pending_sleep_light_wake_after_ms = poll.light_wake_after_ms;
    state.pending_sleep_maintenance = maintenance;
    state.pending_sleep_page_allows = inputs.page_allows_sleep;
    vec![batch(
        state,
        state
            .pending_sleep_operation
            .expect("sleep prepare operation"),
        // Prepare/commit are platform handshakes whose completion arrives
        // as a fact from the event collector.  They are therefore async at
        // the effect boundary and must never be placed in AbortBatch.
        FailurePolicy::Continue,
        vec![Effect::PrepareSleep {
            token,
            maintenance,
            light_wake_after_ms: poll.light_wake_after_ms,
        }],
    )]
}

fn transition_sleep_prepared(
    state: &mut AppState,
    token: SleepToken,
    inputs: SleepInputs,
) -> Vec<EffectBatch> {
    let Some(_prepared_inputs) = state.pending_sleep_inputs else {
        return vec![];
    };
    if state.sleep.prepared_token() != Some(token) || state.pending_sleep_operation.is_none() {
        return vec![];
    }
    let confirmed_inputs = app_sleep_inputs_from_facts(state, inputs);
    if state.sleep.commit(token, confirmed_inputs).is_err() {
        state.pending_sleep_inputs = None;
        state.pending_sleep_page_allows = false;
        state.pending_sleep_maintenance = None;
        state.pending_sleep_operation = None;
        return vec![];
    }
    let operation_id = state.next_operation_id();
    state.pending_sleep_operation = Some(operation_id);
    vec![batch(
        state,
        operation_id,
        // See PrepareSleep above: the collector owns the final mechanical
        // check and feeds SleepCommitted back as an event.
        FailurePolicy::Continue,
        vec![Effect::CommitSleep(token)],
    )]
}

fn app_sleep_inputs_from_facts(state: &AppState, facts: SleepInputs) -> SleepInputs {
    SleepInputs {
        page_allows_sleep: matches!(state.screen, Screen::Home)
            || state.requested_sleep.is_some()
            || state.pending_sleep_page_allows,
        network_in_flight: state.sync != SyncState::Idle,
        protocol_reply_pending: state.pending_usb_reply.is_some()
            || state.pending_ble_reply.is_some(),
        operation_in_flight: state.pending_rtc.is_some()
            || state.pending_light_sleep_disable.is_some()
            || state.pending_residue_ack.is_some()
            || state.pending_alarm_list_edit.is_some()
            || state.pending_alarm_add.is_some()
            || state.pending_todo_list_edit.is_some()
            || state.pending_sync_data.is_some()
            || state.pending_sync_metadata.is_some()
            || state.pending_sync_rtc_op.is_some()
            || state.pending_sync_apply_op.is_some()
            || state.pending_sync_metadata_op.is_some()
            || state.pending_urgent_poll.is_some()
            || !state.retries.is_empty(),
        rtc_plan_confirmed: state.rtc_alarm_plan_confirmed(),
        ..facts
    }
}

fn transition_sleep_committed(state: &mut AppState, token: SleepToken) -> Vec<EffectBatch> {
    // `pending_sleep_inputs` is cleared when this transition consumes the
    // platform commit fact. A duplicate SleepCommitted must not start a
    // second EnterLight/DeepSleep effect for the same token.
    if !state.sleep.is_committed(token)
        || state.pending_sleep_operation.is_none()
        || state.pending_sleep_inputs.is_none()
    {
        return vec![];
    }
    state.pending_sleep_inputs = None;
    state.pending_sleep_page_allows = false;
    let maintenance = state.pending_sleep_maintenance.take();
    let effect = match token.kind {
        SleepKind::Light => Effect::EnterLightSleep(LightSleepPlan {
            wake_after_ms: state.pending_sleep_light_wake_after_ms,
        }),
        SleepKind::Deep => Effect::EnterDeepSleep(WakeupPlan { maintenance }),
    };
    let operation_id = state.next_operation_id();
    state.pending_sleep_operation = Some(operation_id);
    vec![batch(
        state,
        operation_id,
        FailurePolicy::AbortBatch,
        vec![effect],
    )]
}

fn transition_sleep_cancelled(state: &mut AppState, token: SleepToken) -> Vec<EffectBatch> {
    let disable = state.sleep.is_committed(token) && token.kind == SleepKind::Light;
    if state.sleep.prepared_token() == Some(token) || state.sleep.is_committed(token) {
        state.sleep.cancel();
        state.pending_sleep_inputs = None;
        state.pending_sleep_page_allows = false;
        state.pending_sleep_maintenance = None;
        state.pending_sleep_operation = None;
    }
    if disable {
        disable_light_sleep_batch(state)
    } else {
        vec![]
    }
}

fn disable_light_sleep_batch(state: &mut AppState) -> Vec<EffectBatch> {
    if state.pending_light_sleep_disable.is_some() {
        return vec![];
    }
    let operation_id = state.next_operation_id();
    state.pending_light_sleep_disable = Some(operation_id);
    vec![batch(
        state,
        operation_id,
        FailurePolicy::Continue,
        vec![Effect::DisableLightSleep],
    )]
}

/// Returns whether a completion from the BLE worker belongs to the pairing
/// session currently shown by the state machine. Worker startup is allowed to
/// outlive a user cancel/re-entry, so late results must not advance a newer
/// session.
pub fn ble_pairing_session_matches(screen: &Screen, session_id: u64) -> bool {
    matches!(screen, Screen::BlePairing(pairing) if pairing.session_id == session_id)
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

fn transition_urgent_poll_completed(state: &mut AppState, available: bool) -> Vec<EffectBatch> {
    let Some(op) = state.pending_urgent_poll.take() else {
        return vec![];
    };
    let _ = op;
    if !available {
        state.sync_scheduler.clear_urgent_synced();
        return vec![];
    }
    if state.sync != SyncState::Idle
        || !state.connectivity.wifi_configured
        || !state.connectivity.server_configured
        || state.sync_scheduler.already_synced_urgent(&state.inbox)
    {
        return vec![];
    }
    let Some(now) = state.clock.now else {
        return vec![];
    };
    let request_id = state.next_operation_id();
    state.sync = SyncState::Running { request_id };
    vec![batch(
        state,
        request_id,
        FailurePolicy::Continue,
        vec![Effect::StartSync(SyncRequest { now })],
    )]
}

fn transition_urgent_poll_failed(state: &mut AppState) -> Vec<EffectBatch> {
    if state.pending_urgent_poll.take().is_some() {
        state.sync_scheduler.rollback_urgent_boundary();
    }
    vec![]
}

fn schedule_sync_from_tick(state: &mut AppState, now: DateTime) -> Vec<EffectBatch> {
    if state.sync != SyncState::Idle
        || state.pending_urgent_poll.is_some()
        || !state.connectivity.wifi_configured
        || !state.connectivity.server_configured
    {
        return vec![];
    }
    let unix = now.to_unix();
    let interval = state.sync_scheduler.interval_minutes();
    let urgent_due = state.sync_scheduler.urgent_due(unix);
    let full_due = state.sync_scheduler.full_due(unix, interval);
    if !urgent_due && !full_due {
        return vec![];
    }
    if full_due || (state.sync_scheduler.is_never_synced() && urgent_due) {
        state.sync_scheduler.advance_full_boundary(unix, interval);
        state.sync_scheduler.advance_urgent_boundary(unix);
        let request_id = state.next_operation_id();
        state.sync = SyncState::Running { request_id };
        return vec![batch(
            state,
            request_id,
            FailurePolicy::Continue,
            vec![Effect::StartSync(SyncRequest { now })],
        )];
    }
    state.sync_scheduler.advance_urgent_boundary(unix);
    let request_id = state.next_operation_id();
    state.pending_urgent_poll = Some(request_id);
    vec![batch(
        state,
        request_id,
        FailurePolicy::Continue,
        vec![Effect::PollUrgent],
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
    vec![render_batch(state)]
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
        render_batch(state),
    ]
}

fn transition_ble_pairing_failed(
    state: &mut AppState,
    failure: BlePairingFailure,
) -> Vec<EffectBatch> {
    if !matches!(state.screen, Screen::BlePairing(_)) {
        return vec![];
    }
    // A worker start failure cannot be shown by a stale connecting surface;
    // return to the selected Settings row where the user can retry. The
    // Stop serializes radio cleanup. The executor logs the failure detail.
    let _ = failure;
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
        render_batch(state),
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
    vec![render_batch(state)]
}

/// BlePairing screen buttons (Stage 4): only a long ENTER closes the pairing
/// session back to the Settings BLE PAIRING row (the UI's "HOLD ENTER BACK").
/// The entry key must first be released after the screen transition, so its
/// later long-press event cannot be interpreted as an exit while startup is
/// still pending. Release of any key arms the screen for a future long ENTER.
fn transition_ble_pairing_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::BlePairing(pairing) = &mut state.screen else {
        return vec![];
    };
    match button {
        ButtonEvent::Released(_) => {
            pairing.input_released = true;
            vec![]
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            if !pairing.input_released {
                return vec![];
            }
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
                render_batch(state),
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
            batches.extend(transition_sync_apply(state, data, None));
        }
        SyncResult::OkWithMetadata {
            data,
            last_sync_epoch,
            ntp_epoch,
        } => {
            batches.extend(transition_sync_apply(
                state,
                data,
                Some(PendingSyncMetadata {
                    last_sync_epoch,
                    ntp_epoch,
                }),
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

fn transition_sync_apply(
    state: &mut AppState,
    data: SyncedData,
    metadata: Option<PendingSyncMetadata>,
) -> Vec<EffectBatch> {
    // Guard: only stage merged data when there is a running sync to resolve.
    // A stale / unsolicited completion (e.g. replayed after a deep-sleep
    // restart) arriving from Idle must be harmless — no data adoption, no
    // reply, sync stays Idle.
    let SyncState::Running { request_id } = state.sync else {
        return Vec::new();
    };
    let _request_id = request_id;

    let mut batches = Vec::new();
    let apply_op = state.next_operation_id();

    // A successful sync proves the network was up; record the last-known
    // connectivity fact (GetStatus reports it).
    state.connectivity.wifi_connected = true;

    // Keep the network result private until ApplySyncedData confirms its
    // local commit. A failed apply must leave the visible lists untouched.
    state.pending_sync_data = Some(data.clone());
    state.pending_sync_metadata = metadata.clone();
    state.pending_sync_rtc_op = None;
    state.pending_sync_apply_op = Some(apply_op);

    // Keep the whole local commit pipeline in-flight. This blocks a second
    // SyncNow and deep sleep until Apply, RTC planning, and metadata (when
    // present) have all reached their matching completion events.
    state.sync = SyncState::Applying {
        request_id: apply_op,
    };
    let _ = [Channel::Usb, Channel::Ble]
        .iter()
        .copied()
        .any(|channel| repoint_sync_to_apply(state, channel, apply_op));

    // The confirmable ApplySyncedData persist: its completion (Persisted)
    // or failure resolves the pending transport reply.
    batches.push(batch(
        state,
        apply_op,
        FailurePolicy::AbortBatch,
        vec![Effect::ApplySyncedData(data)],
    ));

    batches
}

fn mark_sync_scheduler_completed(state: &mut AppState) {
    state.sync_scheduler.mark_full_sync_completed();
    if crate::scheduler::SyncScheduler::has_unread_urgent(&state.inbox) {
        state.sync_scheduler.mark_urgent_synced();
    } else {
        state.sync_scheduler.clear_urgent_synced();
    }
}

fn transition_set_wifi_verified(
    state: &mut AppState,
    result: Result<WifiCreds, String>,
) -> Vec<EffectBatch> {
    let mut batches = Vec::new();
    for channel in [Channel::Usb, Channel::Ble] {
        let Some(_pending_creds) = pending_set_wifi_creds(state, channel) else {
            continue;
        };
        match &result {
            Ok(verified) => {
                let persist_op = state.next_operation_id();
                if let Some(pending) = pending_reply_mut(state, channel) {
                    pending.awaiting_ops = vec![persist_op];
                }
                batches.push(batch(
                    state,
                    persist_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::PersistWifiCredentials(verified.clone())],
                ));
            }
            Err(message) => {
                let msg = format!("Connection verification failed: {message}");
                if let Some(pending) = take_pending_reply(state, channel) {
                    batches.push(reply_batch(
                        state,
                        pending.channel,
                        Reply::Error { message: msg },
                    ));
                }
            }
        }
    }
    batches
}

fn transition_wifi_config_applied(
    state: &mut AppState,
    fact: WifiConfigApplied,
) -> Vec<EffectBatch> {
    state.config.wifi_ssid = Some(fact.ssid);
    state.config.wifi_has_password = fact.has_password;
    state.connectivity.wifi_configured = state.config.wifi_configured();
    state.render_generation = state.render_generation.next();
    vec![render_batch(state)]
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
            // An alarm preempts any current reminder: the reminder was
            // already marked-read/persisted when shown, so it must *not*
            // resume after the alarm dismisses. Restore the reminder's own
            // underlying screen instead (the reviewer's air-preempt rule:
            // "urgent 被 alarm 抢占后不得继续显示 todo").
            state.screen_before_ring =
                if let Screen::Reminder(ReminderState { screen_before, .. }) = &state.screen {
                    screen_before.as_ref().clone()
                } else {
                    state.screen.clone()
                };
            state.alarm_runtime = AlarmRuntimeState::Firing {
                alarm_id,
                fired_minute,
                ack: CommitState::InFlight {
                    operation_id: ack_id,
                },
                persistence: CommitState::InFlight {
                    operation_id: persist_id,
                },
                // Safety bound: auto-dismiss 300 s after the trigger minute if
                // nobody presses ENTER (the legacy blocking ring's cap). The
                // collection layer's Ticks drive this - no blocking timer.
                ring_deadline_unix: Some(now.to_unix() + 300),
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
/// Leaves the ringing screen for a non-ringing future, shared by the ENTER
/// dismiss (transition_button) and the ring-timeout Tick (transition_tick).
///
/// Copies the in-flight commit states into `WaitingForRearm` so the
/// background ack/persistence still advance and the rearm happens on a later
/// minute (the same semantics the legacy blocking dismiss used), restores the
/// pre-ring page and issues StopTone + a Full render of the restored surface.
fn dismiss_ringing(state: &mut AppState) -> Vec<EffectBatch> {
    // Keep the current commit states; the background completions continue
    // to advance them while waiting for the minute to roll.
    let AlarmRuntimeState::Firing {
        alarm_id,
        fired_minute,
        ack,
        persistence,
        ..
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
        render_batch(state),
    ]
}

/// Enters the reminder overlay. Sets the current screen as `screen_before`,
/// shows `Screen::Reminder` with the payload's final lines, arms the
/// auto-dismiss deadline (120 s, matching the legacy urgent cap), starts the
/// reminder tone and renders the overlay (Full).
fn transition_reminder_due(state: &mut AppState, payload: ReminderPayload) -> Vec<EffectBatch> {
    // A reminder never preempts a ring; if an alarm is already ringing the
    // fact is dropped (the alarm is the higher-priority alert).
    if state.screen == Screen::AlarmRinging
        || matches!(state.screen, Screen::Reminder(_))
        || matches!(state.alarm_runtime, AlarmRuntimeState::Firing { .. })
    {
        return vec![];
    }
    let deadline = state
        .clock
        .now
        .map(|now| now.to_unix() + 120)
        .unwrap_or_else(|| crate::datetime::DateTime::from_unix(0).to_unix() + 120);
    state.screen = Screen::Reminder(ReminderState {
        kind: payload.kind,
        lines: payload.lines,
        screen_before: Box::new(state.screen.clone()),
        deadline_unix: deadline,
    });
    state.render_generation = state.render_generation.next();
    if !payload.urgent_read_ids.is_empty() || payload.todo_date.is_some() {
        let operation_id = state.next_operation_id();
        state.pending_reminder_operation = Some(operation_id);
        return vec![batch(
            state,
            operation_id,
            FailurePolicy::Continue,
            vec![Effect::PersistReminder(ReminderPersistence {
                urgent_read_ids: payload.urgent_read_ids,
                todo_date: payload.todo_date,
            })],
        )];
    }
    vec![
        batch(
            state,
            OperationId(0),
            FailurePolicy::Continue,
            vec![Effect::StartReminderTone(payload.kind)],
        ),
        render_batch(state),
    ]
}

/// Leaves the reminder overlay: stops the tone, restores the underlying
/// screen and issues a Full render. Shared by ENTER dismiss and the
/// reminder-deadline Tick.
fn dismiss_reminder(state: &mut AppState) -> Vec<EffectBatch> {
    let Screen::Reminder(ReminderState { screen_before, .. }) = &state.screen else {
        return vec![];
    };
    state.screen = screen_before.as_ref().clone();
    state.render_generation = state.render_generation.next();
    vec![
        batch(
            state,
            OperationId(0),
            FailurePolicy::Continue,
            vec![Effect::StopTone],
        ),
        render_batch(state),
    ]
}

fn transition_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    if matches!(button, ButtonEvent::Pressed(ButtonId::Enter))
        && state.screen == Screen::AlarmRinging
        && matches!(&state.alarm_runtime, AlarmRuntimeState::Firing { .. })
    {
        dismiss_ringing(state)
    } else if matches!(state.screen, Screen::Reminder(_))
        && matches!(
            button,
            ButtonEvent::Pressed(_) | ButtonEvent::LongPressed(_)
        )
    {
        // Any button closes the reminder overlay (matches the legacy
        // "ENTER = DISMISS" modal that also unwound on other presses).
        dismiss_reminder(state)
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

/// The screen a closed drawer leaves behind when the selection does not open
/// a destination (cancel, HOME, or the origin's own row).
fn nav_origin_screen(screen_before: &Screen) -> Screen {
    screen_before.clone()
}

fn open_navigation(state: &mut AppState, origin: NavOrigin) -> Vec<EffectBatch> {
    state.screen = Screen::Navigation {
        selected: nav_home_index(origin),
        origin,
        screen_before: Box::new(state.screen.clone()),
    };
    state.render_generation = state.render_generation.next();
    vec![render_batch(state)]
}

/// Navigation-drawer state machine (Stage 4): a long UP/DOWN opens the
/// drawer from a state-machine screen; inside the drawer, UP/DOWN move the selection (wrapping), long
/// UP/DOWN jump to the first/last destination, long ENTER cancels back to
/// the origin, and ENTER selects a destination. Every destination
/// (HOME/CALENDAR/INBOX/ALARMS/TODOS/SETTINGS) is a state-machine screen
/// now; a drawer over a screen highlights that screen's row, so a no-move
/// ENTER stays there.
fn transition_nav_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    match &state.screen {
        Screen::Navigation {
            selected,
            origin,
            screen_before,
        } => {
            let cur = *selected;
            let origin = *origin;
            let screen_before = screen_before.as_ref().clone();
            let next_screen = match button {
                ButtonEvent::Pressed(ButtonId::Up) => Some(Screen::Navigation {
                    selected: if cur == 0 {
                        NAV_DESTINATION_COUNT - 1
                    } else {
                        cur - 1
                    },
                    origin,
                    screen_before: Box::new(screen_before.clone()),
                }),
                ButtonEvent::Pressed(ButtonId::Down) => Some(Screen::Navigation {
                    selected: (cur + 1) % NAV_DESTINATION_COUNT,
                    origin,
                    screen_before: Box::new(screen_before.clone()),
                }),
                ButtonEvent::LongPressed(ButtonId::Up) => Some(Screen::Navigation {
                    selected: 0,
                    origin,
                    screen_before: Box::new(screen_before.clone()),
                }),
                ButtonEvent::LongPressed(ButtonId::Down) => Some(Screen::Navigation {
                    selected: NAV_DESTINATION_COUNT - 1,
                    origin,
                    screen_before: Box::new(screen_before.clone()),
                }),
                ButtonEvent::LongPressed(ButtonId::Enter) => {
                    Some(nav_origin_screen(&screen_before))
                }
                ButtonEvent::Pressed(ButtonId::Enter) => {
                    // Selecting a destination closes the drawer. Every
                    // destination is an SM screen now: HOME (0), CALENDAR
                    // (1), INBOX (2), ALARMS (3), TODOS (4), SETTINGS (5).
                    // Selecting the row for the current origin is a
                    // no-op navigation: restore the exact source state so
                    // cursors (notably Calendar's selected day) survive.
                    if cur == nav_home_index(origin) {
                        state.screen = screen_before;
                        state.render_generation = state.render_generation.next();
                        return vec![render_batch(state)];
                    }
                    return match cur {
                        0 => {
                            state.screen = Screen::Home;
                            state.render_generation = state.render_generation.next();
                            vec![render_batch(state)]
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
                            vec![render_batch(state)]
                        }
                        2 => {
                            state.screen = Screen::Inbox { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch(state)]
                        }
                        3 => {
                            state.screen = Screen::AlarmList { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch(state)]
                        }
                        4 => {
                            state.screen = Screen::TodoList { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch(state)]
                        }
                        5 => {
                            state.screen = Screen::Settings { selected: 0 };
                            state.render_generation = state.render_generation.next();
                            vec![render_batch(state)]
                        }
                        _ => unreachable!(),
                    };
                }
                _ => None,
            };
            if let Some(next) = next_screen {
                // The renderer derives the refresh plan from the ViewModel
                // diff: a still-drawer move (same NavBar surface) is a
                // NAV_BAR partial, leaving the drawer (surface change) is a
                // Full. The SM only signals "the visible state changed".
                state.screen = next;
                state.render_generation = state.render_generation.next();
                vec![render_batch(state)]
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
            // Preserve the settings screen underneath the drawer so cancel
            // and the origin row restore the exact cursor position.
            let cur = *selected;
            match button {
                ButtonEvent::LongPressed(ButtonId::Up)
                | ButtonEvent::LongPressed(ButtonId::Down) => {
                    open_navigation(state, NavOrigin::Settings)
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
                    vec![render_batch(state)]
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
                        batches.push(render_batch(state));
                        batches
                    } else if cur == SETTINGS_SYNC_INTERVAL_ROW {
                        // SYNC INTERVAL (row 1): open the SM interval picker.
                        state.screen = Screen::SyncIntervalPick { selected: 0 };
                        state.render_generation = state.render_generation.next();
                        vec![render_batch(state)]
                    } else if cur == SETTINGS_SLEEP_ROW {
                        // SLEEP (row 3): request the same tokenized admission
                        // path used by idle sleep. The next PowerPoll supplies
                        // USB/input/wake facts before any platform effect.
                        state.requested_sleep = Some(SleepKind::Deep);
                        state.render_generation = state.render_generation.next();
                        vec![render_batch(state)]
                    } else if cur == SETTINGS_BLE_PAIRING_ROW {
                        // BLE PAIRING (row 2): an SM screen now. Enter the
                        // pairing session (Waiting) and ask the executor to
                        // bring the radio up (StartBlePairing -> NimBLE
                        // advertising on the dedicated worker). Radio lifecycle events (Started /
                        // Succeeded / Failed / Disconnected) drive the
                        // phase; long ENTER exits back to the Settings BLE row
                        // with StopBlePairing; the session timeout is a Tick
                        // past an armed deadline. No blocking wedge.
                        let session_id = state.next_operation_id().0;
                        let pairing = BlePairingRequest {
                            name: "inkwash-note4".to_string(),
                            session_id,
                        };
                        state.screen = Screen::BlePairing(BlePairingState {
                            phase: BlePairingPhase::Waiting,
                            pairing_deadline_unix: None,
                            session_id,
                            input_released: false,
                        });
                        state.render_generation = state.render_generation.next();
                        vec![
                            batch(
                                state,
                                OperationId(0),
                                FailurePolicy::Continue,
                                vec![Effect::StartBlePairing(pairing)],
                            ),
                            render_batch(state),
                        ]
                    } else {
                        // Unreachable: every Settings row (0..3) is handled
                        // above.
                        vec![]
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
                open_navigation(state, NavOrigin::Home)
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
            vec![render_batch(state)]
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
            // Update cadence in the state machine and persist the same value
            // through the executor.
            let minutes = SYNC_INTERVAL_MINUTES[selected];
            if let Some(now) = state.clock.now {
                state
                    .sync_scheduler
                    .set_interval_minutes(now.to_unix(), minutes);
            }
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
                render_batch(state),
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
            open_navigation(state, NavOrigin::AlarmList)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the alarm list to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            let selected = selected.min(alarm_count);
            state.screen = Screen::AlarmList {
                // Wrap like the navigation drawer: the ADD row remains
                // reachable from either end even when the list is long.
                selected: if selected == 0 {
                    alarm_count
                } else {
                    selected - 1
                },
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            let selected = selected.min(alarm_count);
            // The selection moves through the ADD row (index == alarm_count)
            // and wraps like the navigation drawer instead of appearing
            // stuck when DOWN is held at the end of a long list.
            state.screen = Screen::AlarmList {
                selected: if selected == alarm_count {
                    0
                } else {
                    selected + 1
                },
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            // The append persist owns the authoritative list snapshot until
            // it completes. Ignore all list ENTER actions while it is in
            // flight so a row toggle cannot race and overwrite the append.
            if state.pending_alarm_add.is_some() {
                return vec![];
            }
            if selected == alarm_count {
                // "+ ADD ALARM": open the SM two-stage picker (hour, then
                // minute). The list's ADD-row stays selected underneath so a
                // cancel returns to it.
                state.screen = Screen::AlarmAdd(AlarmAddState::default());
                state.render_generation = state.render_generation.next();
                vec![render_batch(state)]
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
                    render_batch(state),
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
            vec![render_batch(state)]
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
                let new_index = state.alarms.alarms.len() - 1;
                let op = state.next_operation_id();
                state.pending_alarm_add = Some(op);
                // The append is optimistic, so show the newly-created row
                // immediately. Persist confirmation only releases the gate
                // and re-arms RTC; it must not leave the user in the picker.
                state.screen = Screen::AlarmList {
                    selected: new_index,
                };
                state.render_generation = state.render_generation.next();
                let list = state.alarms.alarms.clone();
                vec![
                    batch(
                        state,
                        op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::PersistAlarms(list)],
                    ),
                    render_batch(state),
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

/// Short ENTER toggles a row's done flag through an optimistic confirmable
/// edit (the executor persists the list + marks the row dirty). Long ENTER
/// returns Home without editing the selected todo. Long UP/DOWN opens the GO
/// TO drawer (origin TodoList). An empty list has no rows to act on (moves
/// stay at 0).
fn transition_todo_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let count = state.todos.todos.len();
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            // Open the GO TO drawer over the todo list (origin TodoList).
            open_navigation(state, NavOrigin::TodoList)
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            let selected = selected.min(count.saturating_sub(1));
            state.screen = Screen::TodoList {
                selected: if count == 0 {
                    0
                } else if selected == 0 {
                    count - 1
                } else {
                    selected - 1
                },
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Down) => {
            let selected = selected.min(count.saturating_sub(1));
            // Wrap at both ends; an empty list stays at 0.
            state.screen = Screen::TodoList {
                selected: if count == 0 || selected + 1 == count {
                    0
                } else {
                    selected + 1
                },
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        // Short ENTER toggles done through the confirmable-edit path.
        ButtonEvent::Pressed(ButtonId::Enter) => {
            if state.pending_todo_list_edit.is_some() {
                // One edit at a time: while a persist is in flight another
                // row action is ignored (completion/rollback releases it).
                return vec![];
            }
            let op = state.next_operation_id();
            let (previous_done, edited_id) = {
                let Some(todo) = state.todos.todos.get_mut(selected) else {
                    return vec![];
                };
                let previous_done = todo.done;
                let edited_id = todo.id;
                todo.done = !todo.done;
                (previous_done, edited_id)
            };
            state.pending_todo_list_edit = Some(PendingTodoListEdit {
                operation_id: op,
                index: selected,
                previous_done,
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
                render_batch(state),
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
            open_navigation(state, NavOrigin::Inbox)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the inbox list to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
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
            batches.push(render_batch(state));
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
            vec![render_batch(state)]
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
            open_navigation(state, NavOrigin::Calendar)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            // Back out of the calendar to root Home.
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
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
            vec![render_batch(state)]
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
            vec![render_batch(state)]
        }
        _ => vec![],
    }
}

fn transition_tick(state: &mut AppState, now: DateTime) -> Vec<EffectBatch> {
    let mut batches = Vec::new();
    if state.pending_reminder_facts.is_none() {
        let operation_id = state.next_operation_id();
        state.pending_reminder_facts = Some(operation_id);
        batches.push(batch(
            state,
            operation_id,
            FailurePolicy::Continue,
            vec![Effect::CollectReminderFacts(now)],
        ));
    }
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
    batches.extend(schedule_sync_from_tick(state, now));

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

    // Ring timeout (Stage 5/6): while an alarm is Firing with an armed ring
    // deadline, a Tick past it auto-dismisses exactly like an ENTER press -
    // the timeout is an event (Ticks), never a blocking-loop timer. Keeps an
    // unattended alarm from ringing forever without the legacy MAX_RING_SECS
    // blocking loop.
    if state.screen == Screen::AlarmRinging {
        if let AlarmRuntimeState::Firing {
            ring_deadline_unix: Some(deadline),
            ..
        } = state.alarm_runtime
        {
            if now.to_unix() >= deadline {
                batches.extend(dismiss_ringing(state));
            }
        }
    }
    // Reminder timeout: a Tick past the reminder's deadline dismisses it
    // exactly like a button press (StopTone + restore + Full render). The
    // deadline is an event, never a blocking-loop timer.
    if let Screen::Reminder(ReminderState { deadline_unix, .. }) = state.screen {
        if now.to_unix() >= deadline_unix {
            batches.extend(dismiss_reminder(state));
        }
    }

    // BLE pairing timeout (Stage 5/6): while a pairing session is current
    // with an armed deadline, a Tick past it ends the session as a timeout -
    // the timeout is an event (the collection layer emits Ticks; the SM owns
    // the decision), never a blocking-loop timer.
    if let Screen::BlePairing(st) = &state.screen {
        if st.pairing_deadline_unix.is_some_and(|d| now.to_unix() >= d)
            && matches!(
                st.phase,
                BlePairingPhase::Waiting | BlePairingPhase::Pairing
            )
        {
            if let Screen::BlePairing(st) = &mut state.screen {
                st.phase = BlePairingPhase::Failure("pairing timeout".into());
                st.pairing_deadline_unix = None;
            }
            // End the session: stop the radio and return to the Settings
            // BLE row, rendering the restored Settings surface (Full).
            state.screen = Screen::Settings {
                selected: SETTINGS_BLE_PAIRING_ROW,
            };
            state.render_generation = state.render_generation.next();
            batches.push(batch(
                state,
                OperationId(0),
                FailurePolicy::Continue,
                vec![Effect::StopBlePairing],
            ));
            batches.push(render_batch(state));
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
            | Screen::Reminder(_)
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
        EffectOutput::Persisted(PersistTarget::SyncMetadata)
            if state.pending_sync_metadata_op == Some(completion.operation_id) =>
        {
            state.pending_sync_metadata_op = None;
            state.pending_sync_metadata = None;
            state.pending_sync_rtc_op = None;
            state.sync = SyncState::Idle;
            mark_sync_scheduler_completed(state);
        }
        EffectOutput::Persisted(PersistTarget::SyncApply)
            if state.pending_sync_apply_op == Some(completion.operation_id) =>
        {
            state.pending_sync_apply_op = None;
            if let Some(data) = state.pending_sync_data.take() {
                state.alarms.alarms = data.alarms;
                state.todos.todos = data.todos;
                state.inbox.items = data.inbox;
                state.render_generation = state.render_generation.next();
                batches.push(render_batch(state));
            }
            // Apply must be followed by a confirmed RTC plan. If metadata
            // indicates an NTP time write, stage that first; otherwise plan
            // the alarm directly from the newly adopted data.
            let ntp_staged = if let Some(metadata) = state.pending_sync_metadata.clone() {
                if let Some(epoch) = metadata.ntp_epoch {
                    let target = DateTime::from_unix(epoch)
                        .shifted_minutes(state.config.timezone_offset_minutes as i32);
                    let write_op = state.next_operation_id();
                    state.pending_sync_metadata_op = Some(write_op);
                    state.sync = SyncState::Applying {
                        request_id: write_op,
                    };
                    for channel in [Channel::Usb, Channel::Ble] {
                        let _ = repoint_sync_to_apply(state, channel, write_op);
                    }
                    batches.push(batch(
                        state,
                        write_op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::WriteRtcTime(target)],
                    ));
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !ntp_staged {
                if let Some(now) = state.clock.now {
                    let rtc_batches = program_alarm_for(state, &now);
                    state.pending_sync_rtc_op = rtc_batches.first().map(|b| b.operation_id);
                    if let Some(rtc_op) = state.pending_sync_rtc_op {
                        state.sync = SyncState::Applying { request_id: rtc_op };
                        for channel in [Channel::Usb, Channel::Ble] {
                            let _ = repoint_sync_to_apply(state, channel, rtc_op);
                        }
                    }
                    batches.extend(rtc_batches);
                }
            }
        }
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
            if state.pending_sync_rtc_op == Some(completion.operation_id) {
                state.pending_sync_rtc_op = None;
                if let Some(metadata) = state.pending_sync_metadata.clone() {
                    let metadata_op = state.next_operation_id();
                    state.pending_sync_metadata_op = Some(metadata_op);
                    state.sync = SyncState::Applying {
                        request_id: metadata_op,
                    };
                    for channel in [Channel::Usb, Channel::Ble] {
                        let _ = repoint_sync_to_apply(state, channel, metadata_op);
                    }
                    batches.push(batch(
                        state,
                        metadata_op,
                        FailurePolicy::AbortBatch,
                        vec![Effect::PersistSyncMetadata(SyncMetadata {
                            etag: None,
                            last_sync_epoch: Some(metadata.last_sync_epoch),
                            rtc_align_epoch: metadata.ntp_epoch,
                        })],
                    ));
                } else {
                    state.sync = SyncState::Idle;
                    mark_sync_scheduler_completed(state);
                }
            }
        }
        EffectOutput::RtcTimeWritten(written_time)
            if state.pending_sync_metadata.is_none()
                || state.pending_sync_metadata_op == Some(completion.operation_id) =>
        {
            // A WriteRtcTime (SetTimezone/SetRtc clock write) confirmed: the
            // wall clock moved, so the derived PCF8563 alarm register may now
            // point at the wrong instant. Re-derive it from the time that was
            // actually written (carried in `written_time`) so a time shift
            // never leaves the hardware slot lagging the clock. (P1-5 fix.)
            // Guard against clobbering a concurrently in-flight program.
            state.clock.now = Some(written_time);
            if state.pending_rtc.is_none() {
                let rtc_batches = program_alarm_for(state, &written_time);
                if state.pending_sync_metadata.is_some() {
                    state.pending_sync_rtc_op = rtc_batches.first().map(|b| b.operation_id);
                    if let Some(rtc_op) = state.pending_sync_rtc_op {
                        state.sync = SyncState::Applying { request_id: rtc_op };
                        for channel in [Channel::Usb, Channel::Ble] {
                            let _ = repoint_sync_to_apply(state, channel, rtc_op);
                        }
                    }
                }
                batches.extend(rtc_batches);
            }
        }
        EffectOutput::LightSleepEntered
            if state.pending_sleep_operation == Some(completion.operation_id) =>
        {
            // The platform has accepted the light-sleep configuration. Keep
            // the committed token while the idle loop is parked; a later
            // input/activity event invalidates it and disables the platform
            // mode. Clearing the operation id prevents a late completion from
            // touching a newer sleep request.
            state.pending_sleep_operation = None;
        }
        EffectOutput::ReminderFacts(ref payload)
            if state.pending_reminder_facts == Some(completion.operation_id) =>
        {
            state.pending_reminder_facts = None;
            if let Some(payload) = payload.clone() {
                batches.extend(transition_reminder_due(state, payload));
            }
        }
        EffectOutput::ReminderPersisted
            if state.pending_reminder_operation == Some(completion.operation_id) =>
        {
            state.pending_reminder_operation = None;
            if let Screen::Reminder(reminder) = &state.screen {
                let kind = reminder.kind;
                batches.push(batch(
                    state,
                    OperationId(0),
                    FailurePolicy::Continue,
                    vec![Effect::StartReminderTone(kind)],
                ));
                batches.push(render_batch(state));
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
            // Stage 5: the clock display may shift with the new offset -
            // emit a render (renderer diff Noops if nothing visible changed).
            state.render_generation = state.render_generation.next();
            batches.push(render_batch(state));
        }
        EffectOutput::Persisted(PersistTarget::WifiCredentials) => {
            for channel in [Channel::Usb, Channel::Ble] {
                let Some(pending) = pending_reply_ref(state, channel) else {
                    continue;
                };
                if !matches!(&pending.request, ControlRequest::SetWifi { .. }) {
                    continue;
                }
                if pending.awaiting_ops == [completion.operation_id] {
                    if let Some(creds) = pending_set_wifi_creds(state, channel) {
                        state.config.wifi_ssid = Some(creds.ssid);
                        state.config.wifi_has_password = !creds.password.is_empty();
                        state.connectivity.wifi_configured = state.config.wifi_configured();
                        state.render_generation = state.render_generation.next();
                        batches.push(render_batch(state));
                    }
                }
            }
        }
        EffectOutput::LightSleepDisabled
            if state.pending_light_sleep_disable == Some(completion.operation_id) =>
        {
            state.pending_light_sleep_disable = None;
        }
        _ => {}
    }
    if matches!(
        completion.output,
        EffectOutput::Persisted(_) | EffectOutput::RtcProgrammed | EffectOutput::RtcTimeWritten(_)
    ) && state.pending_sync_metadata.is_none()
        && state.pending_sync_rtc_op.is_none()
    {
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
    let mut render_after_failure = false;
    if state.pending_reminder_facts == Some(failure.operation_id) {
        state.pending_reminder_facts = None;
    }
    if state.pending_light_sleep_disable == Some(failure.operation_id) {
        state.pending_light_sleep_disable = None;
    }
    if state.pending_reminder_operation == Some(failure.operation_id) {
        state.pending_reminder_operation = None;
        let mut batches = Vec::new();
        if let Screen::Reminder(reminder) = &state.screen {
            batches.push(batch(
                state,
                OperationId(0),
                FailurePolicy::Continue,
                vec![Effect::StartReminderTone(reminder.kind)],
            ));
            batches.push(render_batch(state));
        }
        return batches;
    }
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
                render_after_failure = true;
            }
            // A TodoList done-toggle failed to persist: roll the done flag
            // back (NVS never changed), release the gate and re-render.
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
                }
                state.render_generation = state.render_generation.next();
                render_after_failure = true;
            }
            // An ADD-ALARM append failed to persist: pop the optimistically
            // appended alarm back (NVS never changed), release the gate and
            // re-render. The SM screen is AlarmAdd (Minute stage); the
            // appended alarm is the last element.
            if state.pending_alarm_add == Some(failure.operation_id) {
                state.pending_alarm_add = None;
                state.alarms.alarms.pop();
                if let Screen::AlarmList { selected } = &mut state.screen {
                    *selected = (*selected).min(state.alarms.alarms.len());
                }
                state.render_generation = state.render_generation.next();
                render_after_failure = true;
            }
        }
        EffectError::Sync(_) => {
            if state.pending_urgent_poll == Some(failure.operation_id) {
                state.pending_urgent_poll = None;
                state.sync_scheduler.rollback_urgent_boundary();
            }
            if matches!(state.sync, SyncState::Running { .. })
                && state.pending_usb_reply.is_none()
                && state.pending_ble_reply.is_none()
            {
                state.sync_scheduler.rollback_full_boundary();
                state.sync_scheduler.rollback_urgent_boundary();
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
        // All other failure kinds (render, sync, tone, sleep) are
        // surfaced but do not alter the alarm commit state in step 1.
        _ => {}
    }
    if matches!(failure.error, EffectError::Sleep(_))
        && state.pending_sleep_operation == Some(failure.operation_id)
    {
        state.sleep.cancel();
        state.pending_sleep_inputs = None;
        state.pending_sleep_page_allows = false;
        state.pending_sleep_maintenance = None;
        state.pending_sleep_operation = None;
    }
    // A StartBlePairing failure (EffectError::Ble) while a pairing session
    // is current must not leave the SM stranded on the pairing screen with
    // no radio: exit back to the Settings BLE PAIRING row.
    if matches!(failure.error, EffectError::Ble(_)) && matches!(state.screen, Screen::BlePairing(_))
    {
        state.screen = Screen::Settings {
            selected: SETTINGS_BLE_PAIRING_ROW,
        };
        state.render_generation = state.render_generation.next();
    }
    // A command's confirmable effect failed: error the client on the
    // transport that sent the command and clear the slot. This is separate
    // from the alarm-internal retry logic above - a control command is not
    // auto-retried (the client resends).
    let mut batches = Vec::new();
    if render_after_failure {
        batches.push(render_batch(state));
    }
    // If a StartBlePairing failure exited the pairing screen above, emit
    // the render of the restored Settings surface.
    if let Screen::Settings { .. } = state.screen {
        if matches!(failure.error, EffectError::Ble(_)) {
            batches.push(render_batch(state));
        }
    }
    if state.pending_sync_apply_op == Some(failure.operation_id) {
        state.pending_sync_apply_op = None;
        state.sync = SyncState::Idle;
        state.pending_sync_data = None;
        state.pending_sync_metadata = None;
        state.pending_sync_rtc_op = None;
        state.pending_sync_metadata_op = None;
    }
    if state.pending_sync_rtc_op == Some(failure.operation_id)
        || state.pending_sync_metadata_op == Some(failure.operation_id)
    {
        state.pending_sync_rtc_op = None;
        state.pending_sync_metadata_op = None;
        state.pending_sync_metadata = None;
        state.pending_sync_data = None;
        state.sync = SyncState::Idle;
    }
    if state.pending_urgent_poll == Some(failure.operation_id) {
        state.pending_urgent_poll = None;
    }
    if matches!(
        state.sync,
        SyncState::Applying { request_id } if request_id == failure.operation_id
    ) {
        state.sync = SyncState::Idle;
        state.pending_sync_metadata = None;
        state.pending_sync_rtc_op = None;
        state.pending_sync_metadata_op = None;
    }
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
        ControlRequest::SetRtc { epoch_secs } => {
            // Route the host-provided Unix timestamp through the state
            // machine instead of performing a direct RTC write on the
            // firmware main loop. The shift to local time (epoch + stored
            // UTC offset) is computed here; the hardware write is a
            // confirmable `WriteRtcTime` effect executed by the effect
            // task. The reply waits for `RtcTimeWritten`, which also
            // triggers alarm re-derivation (P1-5 fix). Clearing the RTC
            // alignment marker (so NTP does not later undo the host-set
            // time) runs as a fire-and-forget persist in a separate op.
            const MIN_EPOCH: u64 = 946_684_800; // 2000-01-01
            const MAX_EPOCH: u64 = 4_102_444_800; // 2100-01-01
            if !(MIN_EPOCH..=MAX_EPOCH).contains(&epoch_secs) {
                return vec![reply_batch(
                    state,
                    channel,
                    Reply::Error {
                        message: "RTC timestamp must be between 2000-01-01 and 2100-01-01".into(),
                    },
                )];
            }
            let timezone_offset = state.config.timezone_offset_minutes as i32;
            let target_time = DateTime::from_unix(epoch_secs).shifted_minutes(timezone_offset);
            let clear_op = state.next_operation_id();
            let time_op = state.next_operation_id();
            set_pending_reply(
                state,
                channel,
                time_op,
                ControlRequest::SetRtc { epoch_secs },
                vec![time_op],
            );
            vec![
                batch(
                    state,
                    clear_op,
                    FailurePolicy::Continue,
                    vec![Effect::ClearRtcAlignEpoch],
                ),
                batch(
                    state,
                    time_op,
                    FailurePolicy::AbortBatch,
                    vec![Effect::WriteRtcTime(target_time)],
                ),
            ]
        }
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
            // emits StartSetWifi; Event::SetWifiVerified starts persistence. The
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
            // Stage 5: clearing the list is an immediate visible change (an
            // AlarmList shown right now empties), so emit a render right away
            // - the renderer diff Noops if no screen shows the list.
            state.render_generation = state.render_generation.next();
            vec![
                render_batch(state),
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
fn pending_set_wifi_creds(state: &AppState, channel: Channel) -> Option<WifiCreds> {
    let slot = match channel {
        Channel::Usb => &state.pending_usb_reply,
        Channel::Ble => &state.pending_ble_reply,
    };
    let pending = slot.as_ref()?;
    let ControlRequest::SetWifi { ssid, password } = &pending.request else {
        return None;
    };
    let creds = WifiCreds {
        ssid: ssid.clone(),
        password: password.clone(),
    };
    Some(creds)
}

fn pending_reply_mut(state: &mut AppState, channel: Channel) -> Option<&mut PendingReply> {
    match channel {
        Channel::Usb => state.pending_usb_reply.as_mut(),
        Channel::Ble => state.pending_ble_reply.as_mut(),
    }
}

fn pending_reply_ref(state: &AppState, channel: Channel) -> Option<&PendingReply> {
    match channel {
        Channel::Usb => state.pending_usb_reply.as_ref(),
        Channel::Ble => state.pending_ble_reply.as_ref(),
    }
}

fn take_pending_reply(state: &mut AppState, channel: Channel) -> Option<PendingReply> {
    match channel {
        Channel::Usb => state.pending_usb_reply.take(),
        Channel::Ble => state.pending_ble_reply.take(),
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

/// Emits a render request for the current visible state. The renderer
/// derives the refresh plan (Noop / Partial / Full) by diffing the carried
/// ViewModel against its private last-shown cache - the business layer only
/// signals "the visible state may have changed".
fn render_batch(state: &mut AppState) -> EffectBatch {
    let view = state.screen.render_view();
    EffectBatch {
        id: state.next_batch_id(),
        operation_id: OperationId(0),
        render_generation: Some(state.render_generation),
        effects: vec![Effect::Render(RenderRequest {
            generation: state.render_generation,
            view,
            view_model: crate::render_plan::ViewModel::from_state(state),
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
    use crate::todo::Importance;

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
            uploaded_alarm_ids: vec![],
            uploaded_todo_ids: vec![],
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
    fn state_changing_commands_emit_a_render_on_visible_change() {
        // Stage 5: ClearAlarms (optimistic clear at receipt), SetWifi
        // (facts applied on Ok completion) and SetTimezone (offset applied
        // on persist confirm) each emit a render - the business no longer
        // relies on the firmware's manual dirty-path redraw for these.
        // ClearAlarms on the AlarmList screen.
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.screen = Screen::AlarmList { selected: 0 };
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::ClearAlarms));
        assert!(state.alarms.alarms.is_empty());
        assert!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))),
            "ClearAlarms must emit a render (list visibly empties)"
        );
        // SetWifi completion applies facts -> render.
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetWifi {
                ssid: "net".into(),
                password: "pw".into(),
            }),
        );
        let done = update(
            &mut state,
            Event::SetWifiVerified(Ok(WifiCreds {
                ssid: "net".into(),
                password: "pw".into(),
            })),
        );
        assert_eq!(state.config.wifi_ssid, None);
        let persist_op = done
            .iter()
            .find_map(|b| match b.effects.first() {
                Some(Effect::PersistWifiCredentials(_)) => Some(b.operation_id),
                _ => None,
            })
            .expect("verification must admit a credential persistence effect");
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::WifiCredentials),
            }),
        );
        assert_eq!(state.config.wifi_ssid.as_deref(), Some("net"));
        assert!(
            done.iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))),
            "SetWifi Ok must emit a render (wifi facts visible)"
        );
    }

    #[test]
    fn set_timezone_confirm_emits_a_render() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 0;
        let _ = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetTimezone {
                offset_minutes: 120,
            }),
        );
        // Drive both confirmations (RTC write + timezone persist); the
        // timezone persist applies the offset and must emit a render.
        let (time_op, tz_op) = {
            let slot = state.pending_usb_reply.as_ref().expect("slot taken");
            (slot.awaiting_ops[0], slot.awaiting_ops[1])
        };
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: time_op,
                render_generation: None,
                output: EffectOutput::RtcTimeWritten(dt(12, 0)),
            }),
        );
        let batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: tz_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::Timezone),
            }),
        );
        assert_eq!(state.config.timezone_offset_minutes, 120);
        assert!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))),
            "SetTimezone persist confirm must emit a render (clock offset visible)"
        );
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
        let verified = update(
            &mut state,
            Event::SetWifiVerified(Ok(WifiCreds {
                ssid: "home-wifi".into(),
                password: "sekrit".into(),
            })),
        );
        let persist_op = verified
            .iter()
            .find_map(|b| match b.effects.first() {
                Some(Effect::PersistWifiCredentials(_)) => Some(b.operation_id),
                _ => None,
            })
            .expect("verification must produce persistence");
        let done = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: persist_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::WifiCredentials),
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
            Event::SetWifiVerified(Err("authentication failed".into())),
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
        let done = update(
            &mut state,
            Event::SetWifiVerified(Ok(WifiCreds {
                ssid: "test".into(),
                password: "pass".into(),
            })),
        );
        assert!(!done
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
    }

    #[test]
    fn background_wifi_config_applied_updates_facts_without_transport_reply() {
        let mut state = AppState::default();
        let batches = update(
            &mut state,
            Event::WifiConfigApplied(WifiConfigApplied {
                ssid: "home-wifi".into(),
                has_password: true,
            }),
        );

        assert_eq!(state.config.wifi_ssid.as_deref(), Some("home-wifi"));
        assert!(state.config.wifi_has_password);
        assert!(state.connectivity.wifi_configured);
        assert!(state.pending_usb_reply.is_none());
        assert!(state.pending_ble_reply.is_none());
        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. })))
        }));
        assert!(!batches
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
                output: EffectOutput::RtcTimeWritten(dt(18, 0)),
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
    fn set_timezone_rtc_write_confirms_rearms_alarm() {
        // After a WriteRtcTime (SetTimezone clock write) confirms, the
        // wall clock has moved, so the state machine must re-derive the
        // PCF8563 alarm register from the new time. Without this, a time
        // shift leaves the hardware slot pointing at the old instant (P1-5).
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 0;
        // An enabled Daily alarm at 10:30 — due today at 10:30.
        state.alarms.alarms = vec![alarm(1, 10, 30)];
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
        // RTC write confirms — must emit a ProgramRtcAlarm (not just reply).
        let confirmed = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                output: EffectOutput::RtcTimeWritten(dt(18, 0)),
            }),
        );
        assert!(
            confirmed.iter().any(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            }),
            "RtcTimeWritten must reprogram the hardware alarm after a clock shift"
        );
        // The clock fact must reflect the time that was actually written,
        // not the stale pre-write value (used above for alarm re-derivation).
        assert_eq!(
            state.clock.now,
            Some(dt(18, 0)),
            "RtcTimeWritten must update the clock fact to the written time"
        );
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
    fn set_rtc_routes_through_state_machine_and_clears_alignment() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 120; // UTC+2
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetRtc {
                epoch_secs: 946_684_800 + 10 * 3600, // 2000-01-01 10:00 UTC
            }),
        );
        assert!(
            state.pending_usb_reply.is_some(),
            "SetRtc must take a USB slot"
        );
        // Must emit both the alignment-clear (fire-and-forget persist) and
        // the confirmable WriteRtcTime effect.
        let has_clear = batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ClearRtcAlignEpoch))
        });
        let write_rtc = batches.iter().find(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::WriteRtcTime(_)))
        });
        assert!(has_clear, "SetRtc must clear the RTC alignment marker");
        let rtc_batch = write_rtc.expect("SetRtc must emit a WriteRtcTime effect");
        assert!(
            matches!(
                rtc_batch.effects.iter().find(|e| matches!(e, Effect::WriteRtcTime(_))),
                Some(Effect::WriteRtcTime(dt)) if dt.hour == 12 && dt.minute == 0
            ),
            "WriteRtcTime must carry local time (epoch UTC 10:00 + 120 min offset -> 12:00)"
        );
        let rtc_op = rtc_batch.operation_id;
        // RTC write confirms: clock fact must update to the written time,
        // alarm must re-derive from the new time, and the reply must fire.
        let confirmed = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                output: EffectOutput::RtcTimeWritten(dt(12, 0)),
            }),
        );
        assert_eq!(
            state.clock.now,
            Some(dt(12, 0)),
            "clock must reflect written time"
        );
        let has_reply = confirmed.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Ok
                    }
                )
            })
        });
        assert!(
            has_reply,
            "RtcTimeWritten must fire the pending SetRtc reply"
        );
        assert!(
            state.pending_usb_reply.is_none(),
            "slot released on confirm"
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
    fn tick_owns_full_sync_boundary_and_starts_single_flight() {
        let mut state = AppState::default();
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let boot_now = dt(10, 0);
        let _ = update(
            &mut state,
            Event::SyncSchedulerConfigured(SyncSchedulerConfig {
                now_unix: boot_now.to_unix(),
                interval_minutes: 1,
                last_sync_epoch: Some(boot_now.to_unix()),
            }),
        );
        let batches = update(&mut state, Event::Tick(dt(10, 1)));
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(batches
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))));
    }

    #[test]
    fn urgent_poll_completion_returns_to_app_before_starting_sync() {
        let mut state = AppState::default();
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let boot_now = dt(10, 0);
        let _ = update(
            &mut state,
            Event::SyncSchedulerConfigured(SyncSchedulerConfig {
                now_unix: boot_now.to_unix(),
                interval_minutes: 60,
                last_sync_epoch: Some(boot_now.to_unix()),
            }),
        );
        let poll = update(&mut state, Event::Tick(dt(10, 1)));
        assert!(poll
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::PollUrgent))));
        assert_eq!(state.sync, SyncState::Idle);
        let sync = update(&mut state, Event::UrgentPollCompleted { available: true });
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(sync
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))));
    }

    #[test]
    fn urgent_poll_failure_restores_boundary_for_retry() {
        let mut state = AppState::default();
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let boot_now = dt(10, 0);
        let _ = update(
            &mut state,
            Event::SyncSchedulerConfigured(SyncSchedulerConfig {
                now_unix: boot_now.to_unix(),
                interval_minutes: 60,
                last_sync_epoch: Some(boot_now.to_unix()),
            }),
        );
        let tick = DateTime {
            second: 31,
            ..boot_now
        };
        let poll = update(&mut state, Event::Tick(tick));
        let op = poll
            .iter()
            .find(|b| b.effects.iter().any(|e| matches!(e, Effect::PollUrgent)))
            .unwrap()
            .operation_id;
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Sync("poll failed".into()),
            }),
        );
        assert!(update(&mut state, Event::Tick(tick))
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::PollUrgent))));
    }

    #[test]
    fn full_sync_start_failure_restores_boundary_for_retry() {
        let mut state = AppState::default();
        state.connectivity.wifi_configured = true;
        state.connectivity.server_configured = true;
        let boot_now = dt(10, 0);
        let _ = update(
            &mut state,
            Event::SyncSchedulerConfigured(SyncSchedulerConfig {
                now_unix: boot_now.to_unix(),
                interval_minutes: 1,
                last_sync_epoch: Some(boot_now.to_unix()),
            }),
        );
        let tick = dt(10, 1);
        let start = update(&mut state, Event::Tick(tick));
        let op = start
            .iter()
            .find(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_))))
            .unwrap()
            .operation_id;
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: op,
                render_generation: None,
                error: EffectError::Sync("start failed".into()),
            }),
        );
        assert!(update(&mut state, Event::Tick(tick))
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))));
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
        // SyncCompleted(Ok) stages the merged data and emits an apply batch;
        // visible lists remain unchanged until the local apply confirms.
        let applied = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![alarm(1, 9, 0), alarm(2, 10, 30)]),
            }),
        );
        assert!(state.alarms.alarms.is_empty());
        assert!(matches!(state.sync, SyncState::Applying { .. }));
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
        // Apply confirms -> data is adopted and rendered, then the new alarm
        // plan must be confirmed before the sync can commit.
        let rtc_batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: apply_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::SyncApply),
            }),
        );
        assert_eq!(state.alarms.alarms, vec![alarm(1, 9, 0), alarm(2, 10, 30)]);
        assert!(rtc_batches
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Render(_)))));
        assert!(state.sync_scheduler.is_never_synced());
        let rtc_op = rtc_batches
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm))
                    .then_some(b.operation_id)
            })
            .expect("sync apply must confirm the RTC plan");
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
                        channel: Channel::Ble,
                        reply: Reply::Ok
                    }
                )
            })
        }));
        assert!(state.pending_ble_reply.is_none());
        assert!(state.pending_usb_reply.is_none());
        assert!(!state.sync_scheduler.is_never_synced());
    }

    #[test]
    fn sync_metadata_failure_does_not_mark_scheduler_complete() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        let applied = update(
            &mut state,
            Event::SyncCompleted(SyncResult::OkWithMetadata {
                data: synced_data(vec![alarm(1, 11, 0)]),
                last_sync_epoch: 123,
                ntp_epoch: None,
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
        let rtc_batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: apply_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::SyncApply),
            }),
        );
        let rtc_op = rtc_batches
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm))
                    .then_some(b.operation_id)
            })
            .unwrap();
        let metadata_batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: rtc_op,
                render_generation: None,
                output: EffectOutput::RtcProgrammed,
            }),
        );
        let metadata_op = metadata_batches
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::PersistSyncMetadata(_)))
                    .then_some(b.operation_id)
            })
            .unwrap();
        let failed = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: metadata_op,
                render_generation: None,
                error: EffectError::Persist("metadata failed".into()),
            }),
        );
        assert!(state.sync_scheduler.is_never_synced());
        assert!(failed.iter().any(|b| {
            b.effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Error { message }
                    } if message.contains("metadata failed")
                )
            })
        }));
        assert!(state.pending_usb_reply.is_none());
    }

    #[test]
    fn sync_apply_pipeline_blocks_second_sync_and_sleep() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
        let applied = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![alarm(1, 11, 0)]),
            }),
        );
        assert!(matches!(state.sync, SyncState::Applying { .. }));
        let busy = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(busy.iter().any(|b| {
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

        state.requested_sleep = Some(SleepKind::Deep);
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        let _ = update(&mut state, Event::PowerPoll(poll(0)));
        let sleep = update(&mut state, Event::PowerPoll(poll(2_000)));
        assert!(!sleep.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::PrepareSleep { .. }))
        }));
        assert_eq!(state.requested_sleep, Some(SleepKind::Deep));
        assert!(state.pending_sync_data.is_some());
        assert!(applied.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ApplySyncedData(_)))
        }));
    }

    #[test]
    fn sync_completed_apply_failure_replies_error_and_releases_slot() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.alarms.alarms = vec![alarm(9, 8, 0)];
        let original_alarms = state.alarms.alarms.clone();
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
        assert_eq!(state.alarms.alarms, original_alarms);
        assert!(state.pending_sync_data.is_none());
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
    fn sync_adopt_emits_a_render_with_the_merged_data_fingerprint() {
        // Stage 5: adopting merged server data may change what the current
        // screen shows, so SyncCompleted(Ok) emits a render request whose
        // ViewModel carries the merged lists' data fingerprint. The renderer
        // diffs it against its cache - an unchanged sync (no visible delta)
        // resolves to Noop, a changed list repaints.
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        // On the AlarmList screen, a sync that adds an alarm must change the
        // request's data fingerprint so the renderer does not Noop it.
        state.screen = Screen::AlarmList { selected: 0 };
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let _ = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        let batches = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![alarm(1, 9, 0), alarm(2, 10, 30)]),
            }),
        );
        assert_eq!(state.alarms.alarms, vec![alarm(1, 9, 0)]);
        assert!(!batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::Render(_))) }));
        let apply_op = batches
            .iter()
            .find_map(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ApplySyncedData(_)))
                    .then_some(b.operation_id)
            })
            .expect("sync adopt must emit apply");
        let after_apply = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: apply_op,
                render_generation: None,
                output: EffectOutput::Persisted(PersistTarget::SyncApply),
            }),
        );
        assert_eq!(state.alarms.alarms, vec![alarm(1, 9, 0), alarm(2, 10, 30)]);
        let render_requests: Vec<_> = after_apply
            .iter()
            .flat_map(|b| &b.effects)
            .filter_map(|e| match e {
                Effect::Render(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(
            render_requests.len(),
            1,
            "confirmed sync adopt emits one render"
        );
        let vm = &render_requests[0].view_model;
        assert_eq!(vm.view, RenderView::AlarmList { selected: 0 });
        // The fingerprint must differ from the pre-sync single-alarm list.
        let mut pre = state.clone();
        pre.alarms.alarms = vec![alarm(1, 9, 0)];
        let pre_fp = crate::render_plan::ViewModel::from_state(&pre).data_fingerprint;
        assert_ne!(
            vm.data_fingerprint, pre_fp,
            "merged-data fingerprint must reflect the added alarm"
        );
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home,
                ..
            }
        ));
        // A full render is requested for the drawer overlay.
        assert!(batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::Render(_))) }));
        // The request names the Navigation surface (executor draws the
        // drawer overlay, not a call-site rectangle).
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
                view: RenderView::Navigation { selected: 0, .. },
                ..
            })
        )));
    }

    #[test]
    fn navigation_preserves_every_source_screen_and_calendar_cursor() {
        let sources = [
            (Screen::Home, NavOrigin::Home),
            (Screen::Settings { selected: 2 }, NavOrigin::Settings),
            (Screen::AlarmList { selected: 1 }, NavOrigin::AlarmList),
            (Screen::TodoList { selected: 0 }, NavOrigin::TodoList),
            (Screen::Inbox { selected: 3 }, NavOrigin::Inbox),
            (
                Screen::Calendar(CalendarState {
                    year: 2026,
                    month: 9,
                    selected_day: 27,
                }),
                NavOrigin::Calendar,
            ),
        ];

        for (source, origin) in sources {
            let mut state = AppState {
                screen: source.clone(),
                ..AppState::default()
            };
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
            );
            let selected = nav_home_index(origin);
            assert!(matches!(
                &state.screen,
                Screen::Navigation {
                    selected: actual,
                    origin: actual_origin,
                    screen_before,
                } if *actual == selected && *actual_origin == origin && screen_before.as_ref() == &source
            ));
            assert_eq!(
                state.screen.render_view(),
                RenderView::Navigation {
                    selected,
                    underlying: Box::new(source.render_view()),
                }
            );
            let opened_vm = crate::render_plan::ViewModel::from_state(&state);

            // Moving the drawer selection changes only the nav-bar field;
            // the source view remains the same for a NAV_BAR partial.
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
            assert_eq!(
                state.screen.render_view(),
                RenderView::Navigation {
                    selected: (selected + 1) % NAV_DESTINATION_COUNT,
                    underlying: Box::new(source.render_view()),
                }
            );
            let moved_vm = crate::render_plan::ViewModel::from_state(&state);
            assert_eq!(
                crate::render_plan::plan_render(Some(&opened_vm), &moved_vm),
                crate::render_plan::RenderPlan::Partial {
                    frame: crate::render_plan::Frame(moved_vm.generation.0 as u32),
                    region: crate::render_plan::PartialRegion::NavBar,
                }
            );

            // Cancellation restores the exact source, including Calendar's
            // selected day rather than reconstructing it from NavOrigin.
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
            );
            assert_eq!(state.screen, source);
        }
    }

    #[test]
    fn navigation_origin_short_enter_restores_exact_source_state() {
        let sources = [
            Screen::Home,
            Screen::Settings { selected: 2 },
            Screen::AlarmList { selected: 1 },
            Screen::TodoList { selected: 0 },
            Screen::Inbox { selected: 3 },
            Screen::Calendar(CalendarState {
                year: 2026,
                month: 9,
                selected_day: 11,
            }),
        ];

        for source in sources {
            let origin = match &source {
                Screen::Home => NavOrigin::Home,
                Screen::Settings { .. } => NavOrigin::Settings,
                Screen::AlarmList { .. } => NavOrigin::AlarmList,
                Screen::TodoList { .. } => NavOrigin::TodoList,
                Screen::Inbox { .. } => NavOrigin::Inbox,
                Screen::Calendar(_) => NavOrigin::Calendar,
                _ => unreachable!(),
            };
            let mut state = AppState {
                screen: source.clone(),
                ..AppState::default()
            };
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
            );
            assert_eq!(
                state.screen.render_view(),
                RenderView::Navigation {
                    selected: nav_home_index(origin),
                    underlying: Box::new(source.render_view()),
                }
            );
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            assert_eq!(state.screen, source);
        }
    }

    #[test]
    fn navigation_from_other_origin_starts_calendar_at_today() {
        let mut state = AppState {
            screen: Screen::Settings { selected: 0 },
            ..AppState::default()
        };
        state.clock.now = Some(dt_full(12, 0, 5, 4));
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
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
                selected_day: 4,
            })
        );
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 1,
                origin: NavOrigin::Home,
                ..
            }
        ));
        // Up from 1 -> 0.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home,
                ..
            }
        ));
        // Up wraps from 0 -> last (5).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 5,
                origin: NavOrigin::Home,
                ..
            }
        ));
        // Long DOWN jumps to last; long UP jumps to first.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home,
                ..
            }
        ));
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 5,
                origin: NavOrigin::Home,
                ..
            }
        ));
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
                view: RenderView::Navigation { selected: 0, .. },
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
    fn drawer_actions_each_emit_one_render() {
        // Each drawer action emits a render request; whether that request
        // is a NAV_BAR Partial or a Full is the renderer's decision from
        // the ViewModel diff (open/close change the surface -> Full; a
        // cursor move stays on the Navigation surface -> NAV_BAR partial),
        // covered by the render_plan tests. The SM contract here is: every
        // visible drawer transition produces exactly one render.
        let mut state = AppState::default();

        let renders = |b: &[EffectBatch]| -> usize {
            b.iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::Render(RenderRequest { .. })))
                .count()
        };

        // Open.
        let b = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(renders(&b), 1, "drawer open emits one render");
        // Move.
        let b = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(renders(&b), 1, "drawer move emits one render");
        // Long ENTER cancels back to Home.
        let b = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(renders(&b), 1, "drawer close (cancel) emits one render");
        // Reopen + ENTER on a destination (still Home after cancel).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let b = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(renders(&b), 1, "drawer select emits one render");
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 5,
                origin: NavOrigin::Home,
                ..
            }
        ));
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
        let _ = update(
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
        // A clamped no-op leaves the visible state identical: the SM still
        // emits a render request (it always signals "input may have changed
        // the view") whose ViewModel is unchanged, so the renderer's diff
        // resolves it to Noop (covered by the render_plan tests). The SM
        // contract here is selection clamping only.
        let clamp_batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        let renders: Vec<_> = clamp_batches
            .iter()
            .flat_map(|b| &b.effects)
            .filter_map(|e| match e {
                Effect::Render(RenderRequest { view_model, .. }) => Some(view_model.clone()),
                _ => None,
            })
            .collect();
        assert!(!renders.is_empty(), "clamp still emits a render request");
        let vm_now = crate::render_plan::ViewModel::from_state(&state);
        assert!(
            renders.iter().all(|vm| vm.view == vm_now.view
                && vm.clock_minute == vm_now.clock_minute
                && vm.overlay == vm_now.overlay
                && vm.data_fingerprint == vm_now.data_fingerprint),
            "clamp render request carries an unchanged ViewModel (renderer -> Noop)"
        );
    }

    #[test]
    fn settings_ble_pairing_row_enters_the_sm_pairing_screen() {
        // P1#2: the Settings BLE PAIRING row is an SM screen now - ENTER
        // enters Screen::BlePairing (Waiting) and asks the executor to start
        // the radio (StartBlePairing); there is no legacy wedge.
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
        assert_eq!(state.screen, Screen::Settings { selected: 2 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
                session_id: 1,
                ..
            })
        ));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartBlePairing(_))));
        // The pairing screen is rendered.
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
    }

    #[test]
    fn ble_worker_results_ignore_stale_or_non_pairing_sessions() {
        let pairing = Screen::BlePairing(BlePairingState {
            session_id: 7,
            ..Default::default()
        });
        assert!(ble_pairing_session_matches(&pairing, 7));
        assert!(!ble_pairing_session_matches(&pairing, 6));
        assert!(!ble_pairing_session_matches(&Screen::Home, 7));
    }

    #[test]
    fn settings_sleep_row_requests_tokenized_sleep() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        // Boot's RTC disable is an effect; settle that mechanical fact before
        // asking the state machine to admit sleep.
        state.pending_rtc = None;
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
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        let _ = update(&mut state, Event::PowerPoll(poll(0)));
        let batches = update(&mut state, Event::PowerPoll(poll(2_000)));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::PrepareSleep { .. })));
        assert!(state.sleep.prepared_token().is_some());
    }

    #[test]
    fn power_poll_drives_manual_sleep_prepare_commit_and_enter() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = |now_ticks, wake_plan_confirmed| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        let _ = update(&mut state, Event::PowerPoll(poll(0, false)));
        let prepared = update(&mut state, Event::PowerPoll(poll(2_000, false)));
        let token = state.sleep.prepared_token().expect("sleep token");
        assert!(prepared
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::PrepareSleep { .. })));
        let committed = update(
            &mut state,
            Event::SleepPrepared {
                token,
                inputs: SleepInputs {
                    wake_plan_confirmed: true,
                    page_allows_sleep: true,
                    event_queue_empty: true,
                    input_latch_clear: true,
                    rtc_plan_confirmed: true,
                    ..SleepInputs::default()
                },
            },
        );
        assert!(committed
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::CommitSleep(_))));
        let entered = update(&mut state, Event::SleepCommitted(token));
        assert!(entered
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::EnterDeepSleep(_))));
        let duplicate = update(&mut state, Event::SleepCommitted(token));
        assert!(!duplicate
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::EnterDeepSleep(_))));
    }

    #[test]
    fn deep_sleep_prepare_fact_requires_wake_plan_confirmation() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: false,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0)));
        update(&mut state, Event::PowerPoll(poll(2_000)));
        let token = state.sleep.prepared_token().expect("prepared token");
        let batches = update(
            &mut state,
            Event::SleepPrepared {
                token,
                inputs: SleepInputs {
                    page_allows_sleep: true,
                    event_queue_empty: true,
                    input_latch_clear: true,
                    rtc_plan_confirmed: true,
                    wake_plan_confirmed: false,
                    ..SleepInputs::default()
                },
            },
        );
        assert!(batches.is_empty());
        assert!(state.sleep.prepared_token().is_none());
        assert!(state.pending_sleep_operation.is_none());
    }

    #[test]
    fn stale_sleep_failure_does_not_cancel_new_prepared_token() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0)));
        let first_batches = update(&mut state, Event::PowerPoll(poll(2_000)));
        let first_token = state.sleep.prepared_token().expect("first token");
        let first_batch = first_batches.first().expect("prepare batch");
        let first_operation = first_batch.operation_id;

        update(&mut state, Event::SleepCancelled(first_token));
        state.requested_sleep = Some(SleepKind::Deep);
        update(&mut state, Event::PowerPoll(poll(4_000)));
        let second_token = state.sleep.prepared_token().expect("second token");
        assert_ne!(first_token, second_token);
        let second_operation = state.pending_sleep_operation.expect("second operation");
        assert_ne!(first_operation, second_operation);

        update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: first_batch.id,
                effect_id: EffectId(1),
                operation_id: first_operation,
                render_generation: None,
                error: EffectError::Sleep("stale prepare failure".into()),
            }),
        );
        assert_eq!(state.sleep.prepared_token(), Some(second_token));
        assert_eq!(state.pending_sleep_operation, Some(second_operation));
    }

    #[test]
    fn activity_power_poll_invalidates_committed_sleep() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = |now_ticks, activity_observed| PowerPoll {
            now_ticks,
            activity_observed,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0, false)));
        update(&mut state, Event::PowerPoll(poll(2_000, false)));
        let token = state.sleep.prepared_token().expect("prepared token");
        update(
            &mut state,
            Event::SleepPrepared {
                token,
                inputs: SleepInputs {
                    wake_plan_confirmed: true,
                    page_allows_sleep: true,
                    event_queue_empty: true,
                    input_latch_clear: true,
                    rtc_plan_confirmed: true,
                    ..SleepInputs::default()
                },
            },
        );
        update(&mut state, Event::SleepCommitted(token));
        assert_eq!(state.sleep.committed_kind(), Some(SleepKind::Deep));

        update(&mut state, Event::PowerPoll(poll(3_000, true)));
        assert_eq!(state.sleep.committed_kind(), None);
        assert!(state.pending_sleep_operation.is_none());
    }

    #[test]
    fn urgent_poll_blocks_deep_sleep_admission() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            pending_urgent_poll: Some(OperationId(77)),
            ..AppState::default()
        };
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0)));
        let batches = update(&mut state, Event::PowerPoll(poll(2_000)));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|effect| { matches!(effect, Effect::PrepareSleep { .. }) }));
        assert_eq!(state.requested_sleep, Some(SleepKind::Deep));
        assert!(state.sleep.prepared_token().is_none());
    }

    #[test]
    fn light_sleep_completion_closes_enter_operation() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Light),
            ..AppState::default()
        };
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0)));
        update(&mut state, Event::PowerPoll(poll(2_000)));
        let token = state.sleep.prepared_token().expect("prepared token");
        update(
            &mut state,
            Event::SleepPrepared {
                token,
                inputs: SleepInputs {
                    page_allows_sleep: true,
                    event_queue_empty: true,
                    input_latch_clear: true,
                    rtc_plan_confirmed: true,
                    ..SleepInputs::default()
                },
            },
        );
        let entered = update(&mut state, Event::SleepCommitted(token));
        let enter_batch = entered
            .iter()
            .find(|batch| {
                batch
                    .effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::EnterLightSleep(_)))
            })
            .expect("enter-light batch");
        let operation_id = enter_batch.operation_id;
        assert_eq!(state.pending_sleep_operation, Some(operation_id));

        update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: enter_batch.id,
                effect_id: EffectId(1),
                operation_id,
                render_generation: None,
                output: EffectOutput::LightSleepEntered,
            }),
        );
        assert_eq!(state.sleep.committed_kind(), Some(SleepKind::Light));
        assert!(state.pending_sleep_operation.is_none());

        update(&mut state, Event::Tick(dt(8, 1)));
        assert!(state.sleep.committed_kind().is_none());
    }

    #[test]
    fn light_sleep_failure_cancels_matching_token_for_retry() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Light),
            ..AppState::default()
        };
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(&mut state, Event::PowerPoll(poll(0)));
        let prepare = update(&mut state, Event::PowerPoll(poll(2_000)));
        let prepare_batch = prepare.first().expect("prepare batch");
        let operation_id = prepare_batch.operation_id;
        update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: prepare_batch.id,
                effect_id: EffectId(1),
                operation_id,
                render_generation: None,
                error: EffectError::Sleep("light wake setup failed".into()),
            }),
        );
        assert!(state.sleep.prepared_token().is_none());
        state.requested_sleep = Some(SleepKind::Light);
        let retry = update(&mut state, Event::PowerPoll(poll(4_000)));
        assert!(retry.iter().flat_map(|batch| &batch.effects).any(|effect| {
            matches!(effect, Effect::PrepareSleep { token, .. } if token.kind == SleepKind::Light)
        }));
    }

    #[test]
    fn new_input_cancels_sleep_token_before_commit() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = PowerPoll {
            now_ticks: 2_000,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: false,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(
            &mut state,
            Event::PowerPoll(PowerPoll {
                now_ticks: 0,
                ..poll
            }),
        );
        update(&mut state, Event::PowerPoll(poll));
        let token = state.sleep.prepared_token().unwrap();
        update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(!state.sleep.is_committed(token));
        assert!(update(
            &mut state,
            Event::SleepPrepared {
                token,
                inputs: SleepInputs {
                    wake_plan_confirmed: true,
                    rtc_plan_confirmed: true,
                    page_allows_sleep: true,
                    event_queue_empty: true,
                    input_latch_clear: true,
                    ..SleepInputs::default()
                },
            },
        )
        .is_empty());
    }

    #[test]
    fn sleep_request_survives_release_tick_and_activity_poll_until_prepare() {
        let mut state = AppState {
            requested_sleep: Some(SleepKind::Deep),
            ..AppState::default()
        };
        let poll = |now_ticks, activity_observed| PowerPoll {
            now_ticks,
            activity_observed,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        update(
            &mut state,
            Event::Button(ButtonEvent::Released(ButtonId::Enter)),
        );
        update(&mut state, Event::Tick(dt(8, 1)));
        update(&mut state, Event::PowerPoll(poll(0, true)));
        assert_eq!(state.requested_sleep, Some(SleepKind::Deep));
        let batches = update(&mut state, Event::PowerPoll(poll(2_000, false)));
        assert!(batches
            .iter()
            .flat_map(|batch| &batch.effects)
            .any(|effect| matches!(effect, Effect::PrepareSleep { .. })));
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
        state.pending_rtc = None;
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
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let poll = |now_ticks| PowerPoll {
            now_ticks,
            activity_observed: false,
            final_display_pending: false,
            final_persist_pending: false,
            usb_connected: false,
            event_queue_empty: true,
            input_latch_clear: true,
            wake_plan_confirmed: true,
            network_resumable: false,
            light_wake_after_ms: 1_000,
        };
        let _ = update(&mut state, Event::PowerPoll(poll(0)));
        let batches = update(&mut state, Event::PowerPoll(poll(2_000)));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::PrepareSleep { .. })));
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
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::Render(RenderRequest {
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
            ..Default::default()
        });
        // Started -> Pairing.
        let batches = update(&mut state, Event::BlePairingStarted);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
                ..Default::default()
            })
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
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
                ..Default::default()
            })
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopBlePairing)));
        // Failed -> Settings BLE row + StopBlePairing.
        let batches = update(
            &mut state,
            Event::BlePairingFailed(BlePairingFailure {
                message: "timeout".into(),
            }),
        );
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

    #[test]
    fn ble_pairing_disconnect_then_reconnect_cycles_phase() {
        // The radio lifecycle channel delivers Connected / Disconnected as
        // separate events; a session survives a drop and a fresh connect
        // re-enters Pairing without leaving Screen::BlePairing.
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Pairing,
            ..Default::default()
        });
        // Client drops mid-pairing -> Waiting (radio torn down on the
        // other end; the screen stays so the user can retry).
        let _ = update(&mut state, Event::BleDisconnected);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
                ..Default::default()
            })
        );
        // A fresh connect re-enters Pairing.
        let batches = update(&mut state, Event::BlePairingStarted);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
                ..Default::default()
            })
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
    }

    #[test]
    fn ble_start_failure_exits_the_pairing_screen() {
        // A StartBlePairing failure while a pairing session is current must
        // not strand the SM on the pairing screen with no radio: it returns
        // to the Settings BLE PAIRING row and renders the restored surface.
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Waiting,
            ..Default::default()
        });
        // Fail the StartBlePairing effect (op 0, the pairing-entry batch).
        let batches = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(1),
                operation_id: OperationId(0),
                render_generation: None,
                error: EffectError::Ble("radio init failed".into()),
            }),
        );
        assert_eq!(
            state.screen,
            Screen::Settings {
                selected: SETTINGS_BLE_PAIRING_ROW,
            }
        );
        assert!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))),
            "the restored Settings surface must render"
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
    fn ble_pairing_deadline_tick_times_out_and_exits_to_settings() {
        // Stage-5/6: the pairing timeout is an event - a Tick past the armed
        // deadline ends the session (Failure("pairing timeout") + radio
        // teardown) and returns to the Settings BLE row. No blocking-loop
        // timer involved.
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Pairing,
            pairing_deadline_unix: Some(dt(8, 1).to_unix()),
            ..Default::default()
        });
        // Tick before the deadline: session continues.
        let batches = update(&mut state, Event::Tick(dt(8, 0)));
        assert!(matches!(state.screen, Screen::BlePairing(_)));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopBlePairing)));
        // Tick at/after the deadline: timeout.
        let batches = update(&mut state, Event::Tick(dt(8, 1)));
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
        assert!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))),
            "timeout exits must render the restored Settings surface"
        );
    }

    #[test]
    fn ble_pairing_short_buttons_do_not_exit_during_startup() {
        // The short ENTER that entered the screen, and incidental UP/DOWN
        // input while the worker starts, must not tear down the radio.
        let buttons = [
            ButtonEvent::Pressed(ButtonId::Enter),
            ButtonEvent::Pressed(ButtonId::Down),
            ButtonEvent::Pressed(ButtonId::Up),
        ];
        for b in buttons {
            let mut state = AppState::default();
            let _ = update(
                &mut state,
                Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
            );
            state.screen = Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
                ..Default::default()
            });
            let batches = update(&mut state, Event::Button(b));
            assert!(matches!(state.screen, Screen::BlePairing(_)));
            assert!(!batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(e, Effect::StopBlePairing)));
        }
    }

    #[test]
    fn ble_pairing_long_enter_exits_to_settings_once() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Pairing,
            ..Default::default()
        });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert!(matches!(state.screen, Screen::BlePairing(_)));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .all(|e| !matches!(e, Effect::StopBlePairing)));

        // Once the input layer reports a release, a new long ENTER is an
        // intentional exit and emits exactly one stop effect.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Released(ButtonId::Enter)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::Settings {
                selected: SETTINGS_BLE_PAIRING_ROW,
            }
        );
        assert_eq!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::StopBlePairing))
                .count(),
            1
        );
    }

    #[test]
    fn ble_pairing_entry_press_then_same_hold_long_press_does_not_exit() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        // Navigate Home -> Settings and select BLE PAIRING with the same
        // short ENTER that opens the pairing screen.
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
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(state.screen, Screen::BlePairing(_)));

        // The key remains held: its long-press event must not stop the new
        // session, and no render/side effect is needed.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert!(matches!(state.screen, Screen::BlePairing(_)));
        assert!(batches.iter().all(|batch| batch.effects.is_empty()));

        // A release arms the screen; only a subsequent long ENTER exits.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Released(ButtonId::Enter)),
        );
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(
            state.screen,
            Screen::Settings {
                selected: SETTINGS_BLE_PAIRING_ROW,
            }
        );
        assert_eq!(
            batches
                .iter()
                .flat_map(|batch| &batch.effects)
                .filter(|effect| matches!(effect, Effect::StopBlePairing))
                .count(),
            1
        );
    }

    #[test]
    fn ble_pairing_started_preserves_release_guard() {
        let mut state = AppState {
            screen: Screen::BlePairing(BlePairingState {
                input_released: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = update(&mut state, Event::BlePairingStarted);
        assert!(matches!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Pairing,
                input_released: true,
                ..
            })
        ));
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
                view: RenderView::Home,
                ..
            })
        )));
    }

    #[test]
    fn drawer_over_settings_restores_settings_origin() {
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
        // Long UP/DOWN inside Settings opens the GO TO drawer over Settings.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 5,
                origin: NavOrigin::Settings,
                ..
            }
        ));
        // A no-move ENTER selects SETTINGS and restores the source screen.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

        // Re-open and select SETTINGS explicitly; it also restores the
        // source rather than reconstructing a fresh page.
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 0,
                origin: NavOrigin::Home,
                ..
            }
        ));
        // Down three times: 0 -> 1 -> 2 -> 3 (ALARMS).
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Down)));
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 3,
                origin: NavOrigin::Home,
                ..
            }
        ));
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
                ..
            })
        )));
    }

    #[test]
    fn alarm_list_rows_browse_and_wrap_through_add_row() {
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
        // Down past ADD wraps to the first alarm, so a long list never
        // appears stuck on the trailing ADD row.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
        // Up from the top wraps back to ADD, then walks toward the top.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 2 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 1 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });
    }

    #[test]
    fn alarm_list_add_row_stays_reachable_with_many_alarms() {
        let alarms: Vec<StoredAlarm> = (0..12)
            .map(|index| alarm(index as u8, (index % 24) as u8, 0))
            .collect();
        let mut state = AppState::default();
        open_alarm_list(&mut state, alarms);

        // Twelve alarm rows plus ADD: repeated DOWN reaches the trailing row
        // even though only seven rows are visible at once, then wraps home.
        for selected in 1..=12 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
            assert_eq!(state.screen, Screen::AlarmList { selected });
        }
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });

        // The ADD row still opens the picker after scrolling to it.
        for _ in 0..12 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        assert_eq!(state.screen, Screen::AlarmList { selected: 12 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(state.screen, Screen::AlarmAdd(_)));
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
        assert!(batches.iter().all(|b| b
            .effects
            .iter()
            .all(|e| matches!(e, Effect::Render(RenderRequest { .. })))));
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
        assert_eq!(state.screen, Screen::AlarmList { selected: before });
        // Even if the user moves to ADD and presses ENTER again before the
        // persist completes, no second picker or concurrent list write starts.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(
            state.screen,
            Screen::AlarmList {
                selected: before + 1
            }
        );
        assert!(update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        )
        .is_empty());
        assert_eq!(
            state.screen,
            Screen::AlarmList {
                selected: before + 1
            }
        );
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
    fn alarm_add_can_be_repeated_after_each_persist_confirmation() {
        let mut state = AppState::default();
        open_alarm_list(&mut state, vec![]);

        for expected_len in 1..=3 {
            // Empty list starts with ADD selected; later iterations move
            // from the newly selected alarm to ADD.
            if expected_len > 1 {
                let _ = update(
                    &mut state,
                    Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
                );
            }
            assert_eq!(
                state.screen,
                Screen::AlarmList {
                    selected: expected_len - 1
                }
            );
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            let op = match state.pending_alarm_add {
                Some(op) => op,
                None => panic!("minute confirmation must start persistence"),
            };
            assert_eq!(state.alarms.alarms.len(), expected_len);
            assert_eq!(
                state.screen,
                Screen::AlarmList {
                    selected: expected_len - 1
                }
            );
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
            assert!(state.pending_alarm_add.is_none());
        }
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
        assert_eq!(state.screen, Screen::AlarmList { selected: before });
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 3,
                origin: NavOrigin::AlarmList,
                ..
            }
        ));
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 4,
                origin: NavOrigin::Home,
                ..
            }
        ));
        let _ = update(state, Event::Button(ButtonEvent::Pressed(ButtonId::Enter)));
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
    }

    #[test]
    fn todo_list_browse_wraps_and_empty_stays_at_zero() {
        let mut state = AppState::default();
        open_todo_list(&mut state, vec![todo(1, "a", false), todo(2, "b", true)]);
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
        // Down to row 1 (last row).
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 1 });
        // Down past the end wraps to the first row.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
        // Up from the first row wraps to the last row, then back to top.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 1 });
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });

        // Empty list: opens at 0, Down stays at 0.
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
    fn todo_list_long_enter_returns_home_without_editing() {
        let mut state = AppState::default();
        let todos = vec![todo(1, "a", false)];
        open_todo_list(&mut state, todos.clone());
        let before = state.todos.todos.clone();
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert_eq!(state.todos.todos, before);
        assert!(state.pending_todo_list_edit.is_none());
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|effect| matches!(effect, Effect::PersistTodoEdit { .. })));
    }

    #[test]
    fn todo_list_done_persist_failure_rolls_back_without_touching_importance() {
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
        assert_eq!(state.todos.todos[0].importance, Importance::Medium);
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 4,
                origin: NavOrigin::TodoList,
                ..
            }
        ));
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 2,
                origin: NavOrigin::Home,
                ..
            }
        ));
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 2,
                origin: NavOrigin::Inbox,
                ..
            }
        ));
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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 1,
                origin: NavOrigin::Calendar,
                ..
            }
        ));
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
    fn screens_project_to_their_render_surfaces() {
        let cases: Vec<(Screen, RenderView)> = vec![
            (Screen::Home, RenderView::Home),
            (
                Screen::Navigation {
                    selected: 0,
                    origin: NavOrigin::Home,
                    screen_before: Box::new(Screen::Home),
                },
                RenderView::Navigation {
                    selected: 0,
                    underlying: Box::new(RenderView::Home),
                },
            ),
            (
                Screen::Navigation {
                    selected: 3,
                    origin: NavOrigin::Home,
                    screen_before: Box::new(Screen::Home),
                },
                RenderView::Navigation {
                    selected: 3,
                    underlying: Box::new(RenderView::Home),
                },
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
            (Screen::AlarmRinging, RenderView::AlarmRinging),
            (
                Screen::BlePairing(BlePairingState {
                    phase: BlePairingPhase::Waiting,
                    ..Default::default()
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
    fn firing_alarm_tick_before_deadline_keeps_ringing() {
        // A Tick while Firing and before the ring deadline leaves the ring
        // untouched (no StopTone, screen stays AlarmRinging).
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let batches = update(&mut state, Event::Tick(dt(9, 1)));
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopTone)));
    }

    #[test]
    fn firing_alarm_timeout_tick_dismisses_like_enter() {
        // The ring timeout is an event: a Tick at/past the ring deadline
        // auto-dismisses exactly like an ENTER press (StopTone once,
        // WaitingForRearm, restore pre-ring page, Full render).
        let mut state = AppState::default();
        // Boot with a matching AF: Home is the pre-ring page; the ring
        // deadline is armed ~300s after the 9:00 trigger.
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));
        // A Tick well past the 300s deadline (9:06) dismisses like ENTER.
        let batches = update(&mut state, Event::Tick(dt(9, 6)));
        assert_eq!(state.screen, Screen::Home);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        assert_eq!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::StopTone))
                .count(),
            1,
            "timeout dismiss must StopTone exactly once"
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
    }

    #[test]
    fn usb_get_status_answered_while_alarm_ringing() {
        // The unified event loop keeps serving commands while Firing: a USB
        // GetStatus is answered and leaves the ringing screen untouched.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(batches
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
    }

    #[test]
    fn ble_command_answered_while_alarm_ringing() {
        // BLE commands take the same entry while Firing and are answered on
        // the BLE transport without disturbing the ring.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let batches = update(&mut state, Event::BleCommand(ControlRequest::GetStatus));
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(batches
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::Reply { .. }))));
    }

    #[test]
    fn tone_start_failure_keeps_ring_dismissable() {
        // Audio is a degraded-run category: if StartTone fails (EffectError::
        // Tone, e.g. no codec at boot), the SM stays Firing - the ring stays
        // visual - and an ENTER still dismisses exactly like a healthy ring.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        // StartTone is op 0, effect id varies; fail it.
        let _ = update(
            &mut state,
            Event::EffectFailed(EffectFailure {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(0),
                operation_id: OperationId(0),
                render_generation: None,
                error: EffectError::Tone("audio codec unavailable".into()),
            }),
        );
        // Still ringing (no tone but the visual + lifecycle survive).
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));
        // ENTER dismisses normally.
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        assert_eq!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::StopTone))
                .count(),
            1
        );
    }

    #[test]
    fn epd_completion_and_sync_answered_while_ringing() {
        // The unified loop keeps serving display + sync completions while
        // Firing: a RenderDone for a superseded/other request and a
        // SyncCompleted are absorbed without disturbing the ring.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        // A sync completion for a request that isn't ours is ignored.
        let _ = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![]),
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        // An EffectCompleted for an op that doesn't match our commit is
        // dropped (no state corruption).
        let _ = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(9),
                effect_id: EffectId(9),
                operation_id: OperationId(999),
                render_generation: None,
                output: EffectOutput::RenderDone,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));
    }

    #[test]
    fn rearmed_alarm_triggers_again_with_fresh_tone_and_render() {
        // After a ring is dismissed and the RTC is fully re-armed (the same
        // exactly-once rearm the lifecycle test covers), a fresh snapshot at
        // the alarm's minute re-rings through the exact same non-blocking
        // entry: StartTone + Full render.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        // Dismiss -> WaitingForRearm.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
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
        // Cross-minute Tick: the minute advanced, so ack (still in flight)
        // finishing now re-arms -> ProgramRtcAlarm.
        let _ = update(&mut state, Event::Tick(dt(9, 1)));
        let rearm_batches = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: EffectBatchId(0),
                effect_id: EffectId(2),
                operation_id: ack_op,
                render_generation: None,
                output: EffectOutput::AckDone,
            }),
        );
        let program_op = rearm_batches
            .iter()
            .find(|b| {
                b.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))
            })
            .map(|b| b.operation_id)
            .expect("rearm must ProgramRtcAlarm after ack completes past the minute");
        // Complete the program -> Armed (the precondition for another ring).
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
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Armed { alarm_id: 1 }
        ));
        // A fresh snapshot while Armed re-rings.
        let batches = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::StartTone)));
        assert!(
            batches
                .iter()
                .filter(|b| b.render_generation.is_some())
                .count()
                >= 1
        );
    }

    #[test]
    fn reminder_due_enters_overlay_with_tone_and_full_render() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        assert_eq!(state.screen, Screen::Home);
        let batches = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Urgent,
                lines: vec!["URGENT: server down".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(
            state.screen,
            Screen::Reminder(ReminderState {
                kind: ReminderKind::Urgent,
                ..
            })
        ));
        assert!(batches.iter().any(|b| b
            .effects
            .contains(&Effect::StartReminderTone(ReminderKind::Urgent))));
        assert!(
            batches
                .iter()
                .filter(|b| b.render_generation.is_some())
                .count()
                >= 1
        );
        // The ViewModel projection marks the overlay.
        let vm = crate::render_plan::ViewModel::from_state(&state);
        assert_eq!(vm.overlay, crate::render_plan::Overlay::Reminder);
        assert_eq!(
            vm.view,
            crate::app::RenderView::Reminder {
                kind: ReminderKind::Urgent,
                lines: vec!["URGENT: server down".into()],
            }
        );
    }

    #[test]
    fn reminder_persistence_releases_tone_and_render_after_worker_completion() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let batches = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Urgent,
                lines: vec!["urgent".into()],
                urgent_read_ids: vec![7, 9],
                todo_date: None,
            }),
        );
        assert_eq!(batches.len(), 1);
        assert!(matches!(batches[0].effects[0], Effect::PersistReminder(_)));
        let operation_id = batches[0].operation_id;
        assert!(!batches.iter().any(|batch| batch
            .effects
            .iter()
            .any(|effect| { matches!(effect, Effect::StartReminderTone(_)) })));

        let completed = update(
            &mut state,
            Event::EffectCompleted(EffectCompletion {
                batch_id: batches[0].id,
                effect_id: EffectId(0),
                operation_id,
                render_generation: batches[0].render_generation,
                output: EffectOutput::ReminderPersisted,
            }),
        );
        assert!(completed
            .iter()
            .any(|batch| batch.effects.iter().any(|effect| {
                matches!(effect, Effect::StartReminderTone(ReminderKind::Urgent))
            })));
        assert!(completed
            .iter()
            .any(|batch| batch.render_generation.is_some()));
    }

    #[test]
    fn reminder_any_button_dismisses_restores_and_stops_tone() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        // Enter a reminder over Home; then press UP -> dismiss.
        let _ = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Todo,
                lines: vec!["todo due".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(state.screen, Screen::Reminder(_)));
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::Home);
        assert_eq!(
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::StopTone))
                .count(),
            1
        );
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
    }

    #[test]
    fn reminder_over_settings_restores_settings_on_dismiss() {
        let mut state = AppState {
            screen: Screen::Settings { selected: 1 },
            ..Default::default()
        };
        let _ = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Todo,
                lines: vec!["due".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(state.screen, Screen::Reminder(_)));
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 1 });
    }

    #[test]
    fn alarm_preempts_reminder_and_does_not_resume_it() {
        // An RTC alarm while a reminder is up preempts to AlarmRinging; on
        // dismiss it restores the reminder's *underlying* screen - the
        // reminder never resumes (it was already persisted at show time).
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
        state.screen = Screen::Settings { selected: 2 };
        let _ = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Urgent,
                lines: vec!["urgent".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(state.screen, Screen::Reminder(_)));
        // Alarm fires at its 9:00 minute over the reminder (Settings was
        // under the reminder).
        let _ = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        // Dismiss the alarm: it must land on Settings (reminder's
        // underlying screen), not on the reminder.
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 2 });
    }

    #[test]
    fn reminder_deadline_tick_dismisses() {
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Todo,
                lines: vec!["due".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(state.screen, Screen::Reminder(_)));
        // A Tick well past the 120s deadline dismisses like a button.
        let batches = update(&mut state, Event::Tick(dt(8, 3)));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopTone)));
    }

    #[test]
    fn double_enter_ring_dismisses_exactly_once() {
        // Two ENTERs while Firing: the first dismisses (Firing ->
        // WaitingForRearm), the second is a no-op - StopTone + the restore
        // render fire exactly once.
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);
        let first = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let second = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Home);
        let count = |batches: &[EffectBatch]| {
            batches
                .iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::StopTone))
                .count()
        };
        assert_eq!(count(&first), 1);
        assert_eq!(count(&second), 0, "second ENTER must not StopTone again");
    }

    #[test]
    fn tick_while_reminder_shown_produces_no_extra_overlay_render() {
        // A reminder overlay is current; a per-minute Tick must not add a
        // reminder render (the reminder is in the tick-skip set - only its
        // own enter/dismiss render and deadline dismissal render).
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        let _ = update(
            &mut state,
            Event::ReminderDue(ReminderPayload {
                kind: ReminderKind::Todo,
                lines: vec!["due".into()],
                urgent_read_ids: vec![],
                todo_date: None,
            }),
        );
        assert!(matches!(state.screen, Screen::Reminder(_)));
        // A minute-roll Tick (not past the deadline) leaves the reminder
        // untouched and emits no render.
        let batches = update(&mut state, Event::Tick(dt(8, 1)));
        assert!(matches!(state.screen, Screen::Reminder(_)));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::Render(RenderRequest { .. }))));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopTone)));
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
    /// and no stage is skipped or doubled. This is the logic the firmware
    /// AppRunner drives (the ring is a non-blocking SM state now); it is
    /// testable on the host because `update` is pure.
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
                ..Default::default()
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
            // The restored screen is re-rendered with its own view (Full) -
            // the Full render's view must match the restored screen's
            // projection, so the panel actually shows the screen the keys
            // now operate on.
            let expected_view = before.render_view();
            assert!(dismiss_batches
                .iter()
                .flat_map(|b| &b.effects)
                .any(|e| matches!(
                    e,
                    Effect::Render(RenderRequest {
                        view,
                        ..
                    }) if *view == expected_view
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
                    screen_before: Box::new(Screen::Home),
                },
                RenderView::Navigation {
                    selected: 3,
                    underlying: Box::new(RenderView::Home),
                },
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
                ..Default::default()
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

    /// USB/BLE commands arriving while any SM screen is current must be
    /// handled without disturbing the screen (Stage-4 test-matrix item: the
    /// unified loop services protocol commands on every screen).
    #[test]
    fn commands_are_serviced_while_any_sm_screen_is_current() {
        let screens = [
            Screen::Home,
            Screen::Settings { selected: 0 },
            Screen::AlarmList { selected: 0 },
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
                stage: AddStage::Hour,
                hour: 0,
                value: 9,
            }),
            Screen::SyncIntervalPick { selected: 0 },
            Screen::InboxItem { index: 0 },
        ];
        for screen in screens {
            let mut state = AppState::default();
            // Configured wifi + server so a SyncNow may start.
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
            state.screen = screen.clone();

            // GetStatus: a pure read - replies and leaves the screen as-is.
            let batches = update(&mut state, Event::UsbCommand(ControlRequest::GetStatus));
            assert!(
                batches.iter().any(|b| b.effects.iter().any(|e| matches!(
                    e,
                    Effect::Reply {
                        channel: Channel::Usb,
                        reply: Reply::Status { .. }
                    }
                ))),
                "GetStatus on {screen:?} must reply"
            );
            assert_eq!(state.screen, screen, "screen must not change");

            // SyncNow: starts the SM sync engine and takes the transport's
            // pending slot; the screen stays current.
            let batches = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));
            assert!(
                batches
                    .iter()
                    .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))),
                "SyncNow on {screen:?} must start a sync"
            );
            assert!(state.pending_usb_reply.is_some(), "transport slot taken");
            assert_eq!(state.screen, screen, "screen must not change");
        }
    }
}
/// The final visible payload for a reminder overlay, raised by the
/// firmware's reminder fact layer (and the host harness). The layer has
/// already applied urgent-read and todo-reminded-date persistence, so these
/// are the exact lines to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderPayload {
    pub kind: ReminderKind,
    pub lines: Vec<String>,
    /// Inbox sequence numbers to mark read as part of presenting this fact.
    pub urgent_read_ids: Vec<u64>,
    /// Date key to record for a due-todo reminder, if applicable.
    pub todo_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderPersistence {
    pub urgent_read_ids: Vec<u64>,
    pub todo_date: Option<String>,
}
