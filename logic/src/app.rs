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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RenderGeneration(pub u64);

impl RenderGeneration {
    pub fn next(self) -> Self {
        RenderGeneration(self.0.wrapping_add(1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct OperationId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EffectBatchId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EffectId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Navigation {
        selected: usize,

        origin: NavOrigin,

        screen_before: Box<Screen>,
    },
    Settings {
        selected: usize,
    },

    SyncIntervalPick {
        selected: usize,
    },
    Calendar(CalendarState),

    WeekView {
        year: u16,
        month: u8,
        day: u8,
    },
    AlarmList {
        selected: usize,
    },

    AlarmAdd(AlarmAddState),
    TodoList {
        selected: usize,
    },
    Inbox {
        selected: usize,
    },

    InboxItem {
        index: usize,
    },
    AlarmRinging,

    Reminder(ReminderState),
    BlePairing(BlePairingState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderKind {
    Urgent,

    Todo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderState {
    pub kind: ReminderKind,

    pub lines: Vec<String>,

    pub screen_before: Box<Screen>,

    pub deadline_unix: u64,
}

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

pub const NAV_DESTINATION_COUNT: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarState {
    pub year: u16,
    pub month: u8,

    pub selected_day: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddStage {
    Hour,
    Minute,
}

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

    pub pairing_deadline_unix: Option<u64>,

    pub session_id: u64,

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReply {
    pub operation_id: OperationId,
    pub request: ControlRequest,
    pub channel: Channel,

    pub reply: Option<ControlReply>,

    pub awaiting_ops: Vec<OperationId>,
}

impl PendingReply {
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

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConfigState {
    pub wifi_ssid: Option<String>,
    pub wifi_has_password: bool,

    pub server_url: Option<String>,
    pub server_has_token: bool,

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailurePolicy {
    Continue,
    AbortBatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectBatch {
    pub id: EffectBatchId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effects: Vec<Effect>,
    pub failure_policy: FailurePolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    PersistAlarms(Vec<StoredAlarm>),
    PersistTodos(Vec<Todo>),
    PersistInbox(Vec<InboxItem>),
    PersistConfig(DeviceConfig),
    PersistSyncMetadata(SyncMetadata),

    ApplySyncedData(SyncedData),

    ClearSyncEtag,

    ClearRtcAlignEpoch,

    PersistTimezone(i16),

    WriteRtcTime(DateTime),
    ProgramRtcAlarm(AlarmRegs),
    DisableRtcAlarm,
    AcknowledgeRtcAlarm,

    PersistAlarmToggle {
        alarms: Vec<StoredAlarm>,
        toggled_id: u8,
    },

    PersistTodoEdit {
        todos: Vec<Todo>,
        edited_id: u8,
    },
    StartSync(SyncRequest),

    PollUrgent,

    CollectReminderFacts(DateTime),

    StartSetWifi(WifiCreds),

    PersistWifiCredentials(WifiCreds),
    StartTone,

    StartReminderTone(ReminderKind),
    StopTone,
    Render(RenderRequest),

    Reply {
        channel: Channel,
        reply: ControlReply,
    },
    StartBlePairing(BlePairingRequest),
    StopBlePairing,
    EnterLightSleep(LightSleepPlan),

    DisableLightSleep,
    EnterDeepSleep(WakeupPlan),
    PrepareSleep {
        token: SleepToken,

        maintenance: Option<std::time::Duration>,
        light_wake_after_ms: u64,
    },
    CommitSleep(SleepToken),

    MarkInboxRead {
        seq: u64,
    },

    PersistReminder(ReminderPersistence),

    SetSyncInterval {
        minutes: u16,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderView {
    Home,

    Navigation {
        selected: usize,
        underlying: Box<RenderView>,
    },

    Settings {
        selected: usize,
    },

    AlarmList {
        selected: usize,
    },

    TodoList {
        selected: usize,
    },

    Inbox {
        selected: usize,
    },

    InboxItem {
        index: usize,
    },

    Calendar {
        year: u16,
        month: u8,
        selected_day: u8,
    },

    WeekView {
        year: u16,
        month: u8,
        day: u8,
    },

    NumberPick {
        stage: AddStage,
        value: u8,
    },

    SyncInterval {
        selected: usize,
    },

    BlePairing,

    AlarmRinging,

    Reminder {
        kind: ReminderKind,
        lines: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub generation: RenderGeneration,

    pub view: RenderView,

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistTarget {
    Alarms,
    Todos,
    Inbox,
    Config,
    SyncMetadata,

    SyncApply,

    Timezone,

    WifiCredentials,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutput {
    RenderDone,
    ReminderFacts(Option<ReminderPayload>),
    ReminderPersisted,
    SyncDone(SyncResult),
    Persisted(PersistTarget),
    RtcProgrammed,

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
        data: SyncedData,
    },
    OkWithMetadata {
        data: SyncedData,
        last_sync_epoch: u64,
        ntp_epoch: Option<u64>,
    },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedData {
    pub alarms: Vec<StoredAlarm>,
    pub todos: Vec<Todo>,
    pub inbox: Vec<InboxItem>,

    pub inbox_read_acked: Vec<u64>,
    pub inbox_truncated: bool,
    pub etag: Option<String>,

    pub uploaded_alarm_ids: Vec<u8>,

    pub uploaded_todo_ids: Vec<u8>,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectFailure {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub error: EffectError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcAlarmSnapshot {
    pub now: DateTime,
    pub alarm_flag: bool,
    pub alarm_interrupt_enabled: bool,
}

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

    pub status: DeviceStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DeviceStatus {
    pub wifi_ssid: Option<String>,
    pub wifi_has_password: bool,
    pub timezone_offset_minutes: i16,
}

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

    ReminderDue(ReminderPayload),
    SyncCompleted(SyncResult),

    SetWifiVerified(Result<WifiCreds, String>),

    UrgentPollCompleted {
        available: bool,
    },

    UrgentPollFailed,

    ReminderFacts(Option<ReminderPayload>),

    WifiConfigApplied(WifiConfigApplied),

    PowerPoll(PowerPoll),

    SleepPrepared {
        token: SleepToken,
        inputs: SleepInputs,
    },

    SleepCommitted(SleepToken),

    SleepCancelled(SleepToken),

    SyncBoundaryDue,

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
    fn inflight_is(&self, op: OperationId) -> bool {
        matches!(self, CommitState::InFlight { operation_id } if *operation_id == op)
    }
}

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

    pub config: ConfigState,
    pub pending_usb_reply: Option<PendingReply>,
    pub pending_ble_reply: Option<PendingReply>,

    pending_reminder_operation: Option<OperationId>,
    pending_reminder_facts: Option<OperationId>,

    pub sleep: SleepState,

    pub requested_sleep: Option<SleepKind>,
    pending_sleep_inputs: Option<SleepInputs>,
    pending_sleep_light_wake_after_ms: u64,
    pending_sleep_maintenance: Option<std::time::Duration>,
    pending_sleep_page_allows: bool,

    pending_sleep_operation: Option<OperationId>,
    pending_light_sleep_disable: Option<OperationId>,
    last_activity_ticks: Option<u64>,
    idle_since_ticks: Option<u64>,
    op_counter: u64,
    batch_counter: u64,
    screen_before_ring: Screen,
    pending_rtc: Option<PendingRtc>,

    pending_residue_ack: Option<OperationId>,

    pending_residue_time: Option<DateTime>,

    pending_alarm_list_edit: Option<PendingAlarmListEdit>,

    pending_alarm_add: Option<OperationId>,

    pending_todo_list_edit: Option<PendingTodoListEdit>,
    pending_sync_metadata: Option<PendingSyncMetadata>,
    pending_sync_data: Option<SyncedData>,
    pending_sync_rtc_op: Option<OperationId>,
    pending_sync_apply_op: Option<OperationId>,
    pending_sync_metadata_op: Option<OperationId>,
    pending_urgent_poll: Option<OperationId>,
    retries: Vec<RetryPending>,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingAlarmListEdit {
    operation_id: OperationId,

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

    fn schedule_retry(&mut self, action: RetryAction, now_minute: u64) {
        let due_minute = now_minute + RETRY_BACKOFF_MINUTES;
        if let Some(existing) = self.retries.iter_mut().find(|r| r.action == action) {
            existing.due_minute = due_minute;
        } else {
            self.retries.push(RetryPending { action, due_minute });
        }
    }

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

    pub fn urgent_poll_in_flight(&self) -> bool {
        self.pending_urgent_poll.is_some()
    }
}

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

pub fn ble_pairing_session_matches(screen: &Screen, session_id: u64) -> bool {
    matches!(screen, Screen::BlePairing(pairing) if pairing.session_id == session_id)
}

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
    let SyncState::Running { request_id } = state.sync else {
        return Vec::new();
    };
    let _request_id = request_id;

    let mut batches = Vec::new();
    let apply_op = state.next_operation_id();

    state.connectivity.wifi_connected = true;

    state.pending_sync_data = Some(data.clone());
    state.pending_sync_metadata = metadata.clone();
    state.pending_sync_rtc_op = None;
    state.pending_sync_apply_op = Some(apply_op);

    state.sync = SyncState::Applying {
        request_id: apply_op,
    };
    let _ = [Channel::Usb, Channel::Ble]
        .iter()
        .copied()
        .any(|channel| repoint_sync_to_apply(state, channel, apply_op));

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

    state.config.server_url = Some(snapshot.config.server_url).filter(|s| !s.is_empty());
    state.config.server_has_token = !snapshot.config.auth_token.is_empty();
    state.config.wifi_ssid = snapshot.status.wifi_ssid;
    state.config.wifi_has_password = snapshot.status.wifi_has_password;
    state.config.timezone_offset_minutes = snapshot.status.timezone_offset_minutes;
    state.connectivity.wifi_configured = state.config.wifi_configured();
    state.connectivity.server_configured = state.config.server_configured();

    let mut batches = Vec::new();

    if snapshot.rtc_alarm_flag {
        if let Some(now) = snapshot.now {
            batches.extend(resolve_rtc_alarm(
                state,
                now,
                snapshot.rtc_alarm_interrupt_enabled,
            ));
        }
    } else if state.alarm_runtime == AlarmRuntimeState::Disarmed {
        if let Some(now) = state.clock.now {
            batches.extend(program_alarm_for(state, &now));
        }
    }

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

fn is_due_now(alarm: &StoredAlarm, now: &DateTime) -> bool {
    alarm.enabled
        && alarm.hour == now.hour
        && alarm.minute == now.minute
        && alarm
            .repeat
            .fires_on(now.year, now.month, now.day, now.weekday)
}

fn resolve_rtc_alarm(state: &mut AppState, now: DateTime, aie: bool) -> Vec<EffectBatch> {
    let fired_minute = now.to_unix() / 60;

    if matches!(
        state.alarm_runtime,
        AlarmRuntimeState::Firing { .. } | AlarmRuntimeState::WaitingForRearm { .. }
    ) {
        return vec![];
    }

    if state.pending_residue_ack.is_some() {
        return vec![];
    }

    if !aie {
        state.alarm_runtime = AlarmRuntimeState::Disarmed;
        return clear_residue_alarm(state, now);
    }

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

                ring_deadline_unix: Some(now.to_unix() + 300),
            };
            state.screen = Screen::AlarmRinging;

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

            state.render_generation = state.render_generation.next();
            batches.push(render_batch(state));
            batches
        }

        None => {
            state.alarm_runtime = AlarmRuntimeState::Disarmed;
            clear_residue_alarm(state, now)
        }
    }
}

fn clear_residue_alarm(state: &mut AppState, at: DateTime) -> Vec<EffectBatch> {
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
    state.clock.now = Some(snapshot.now);

    if snapshot.alarm_flag {
        resolve_rtc_alarm(state, snapshot.now, snapshot.alarm_interrupt_enabled)
    } else {
        vec![]
    }
}

fn dismiss_ringing(state: &mut AppState) -> Vec<EffectBatch> {
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

fn transition_reminder_due(state: &mut AppState, payload: ReminderPayload) -> Vec<EffectBatch> {
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

fn nav_home_index(origin: NavOrigin) -> usize {
    match origin {
        NavOrigin::Home => 0,

        NavOrigin::Settings => NAV_DESTINATION_COUNT - 1,

        NavOrigin::AlarmList => 3,

        NavOrigin::TodoList => 4,

        NavOrigin::Inbox => 2,

        NavOrigin::Calendar => 1,
    }
}

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
                state.screen = next;
                state.render_generation = state.render_generation.next();
                vec![render_batch(state)]
            } else {
                vec![]
            }
        }
        Screen::Settings { selected } => {
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
                    state.screen = Screen::Home;
                    state.render_generation = state.render_generation.next();
                    vec![render_batch(state)]
                }
                ButtonEvent::Pressed(ButtonId::Enter) => {
                    if cur == SETTINGS_SYNC_NOW_ROW {
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
                        state.screen = Screen::SyncIntervalPick { selected: 0 };
                        state.render_generation = state.render_generation.next();
                        vec![render_batch(state)]
                    } else if cur == SETTINGS_SLEEP_ROW {
                        state.requested_sleep = Some(SleepKind::Deep);
                        state.render_generation = state.render_generation.next();
                        vec![render_batch(state)]
                    } else if cur == SETTINGS_BLE_PAIRING_ROW {
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

pub const SETTINGS_ROW_COUNT: usize = 4;

pub const SETTINGS_SLEEP_ROW: usize = 3;

pub const SETTINGS_BLE_PAIRING_ROW: usize = 2;

pub const SETTINGS_SYNC_INTERVAL_ROW: usize = 1;

pub const SETTINGS_SYNC_NOW_ROW: usize = 0;

fn transition_sync_interval_pick_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    match button {
        ButtonEvent::LongPressed(ButtonId::Enter) => {
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

pub const SYNC_INTERVAL_MINUTES: [u16; 5] = [1, 5, 10, 30, 60];

fn transition_alarm_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let alarm_count = state.alarms.alarms.len();

    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            open_navigation(state, NavOrigin::AlarmList)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
            state.screen = Screen::Home;
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Up) => {
            let selected = selected.min(alarm_count);
            state.screen = Screen::AlarmList {
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
            if state.pending_alarm_add.is_some() {
                return vec![];
            }
            if selected == alarm_count {
                state.screen = Screen::AlarmAdd(AlarmAddState::default());
                state.render_generation = state.render_generation.next();
                vec![render_batch(state)]
            } else if state.pending_alarm_list_edit.is_some() {
                vec![]
            } else {
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

fn transition_alarm_add_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::AlarmAdd(AlarmAddState { stage, hour, .. }) = &mut state.screen else {
        return vec![];
    };
    let (stage, hour) = (*stage, *hour);
    match button {
        ButtonEvent::LongPressed(ButtonId::Enter) => {
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
                if state.pending_alarm_add.is_some() {
                    return vec![];
                }
                let minute = {
                    let Screen::AlarmAdd(add) = &state.screen else {
                        return vec![];
                    };
                    add.value
                };

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

fn range_for_stage(stage: AddStage) -> (u8, u8) {
    match stage {
        AddStage::Hour => (0, 23),
        AddStage::Minute => (0, 59),
    }
}

fn transition_todo_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let count = state.todos.todos.len();
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
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

        ButtonEvent::Pressed(ButtonId::Enter) => {
            if state.pending_todo_list_edit.is_some() {
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

fn transition_inbox_list_button(
    state: &mut AppState,
    selected: usize,
    button: ButtonEvent,
) -> Vec<EffectBatch> {
    let count = state.inbox.items.len();
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            open_navigation(state, NavOrigin::Inbox)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
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
            let max = count.saturating_sub(1);
            state.screen = Screen::Inbox {
                selected: (selected + 1).min(max),
            };
            state.render_generation = state.render_generation.next();
            vec![render_batch(state)]
        }
        ButtonEvent::Pressed(ButtonId::Enter) => {
            if count == 0 {
                return vec![];
            }
            let index = selected.min(count - 1);

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

fn transition_calendar_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::Calendar(cal) = &mut state.screen else {
        return vec![];
    };
    match button {
        ButtonEvent::LongPressed(ButtonId::Up) | ButtonEvent::LongPressed(ButtonId::Down) => {
            open_navigation(state, NavOrigin::Calendar)
        }
        ButtonEvent::LongPressed(ButtonId::Enter) => {
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

fn transition_week_view_button(state: &mut AppState, button: ButtonEvent) -> Vec<EffectBatch> {
    let Screen::WeekView { year, month, day } = &state.screen else {
        return vec![];
    };
    let (year, month, day) = (*year, *month, *day);
    match button {
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

    if let Screen::Calendar(cal) = &mut state.screen {
        let dim = days_in_month(now.year, now.month);
        if cal.year != now.year || cal.month != now.month {
            cal.year = now.year;
            cal.month = now.month;
            cal.selected_day = cal.selected_day.min(dim).max(1);
        }
    }

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

    if let Screen::Reminder(ReminderState { deadline_unix, .. }) = state.screen {
        if now.to_unix() >= deadline_unix {
            batches.extend(dismiss_reminder(state));
        }
    }

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

    batches.extend(maybe_retry(state, current_minute, &now));

    if maybe_rearm(state, current_minute, &now) {
        batches.extend(program_alarm_for(state, &now));
    }

    if changed && tick_refreshes_current_screen(&state.screen) {
        state.render_generation = state.render_generation.next();
        batches.push(render_batch(state));
    }
    batches
}

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

fn maybe_retry(state: &mut AppState, current_minute: u64, now: &DateTime) -> Vec<EffectBatch> {
    let mut due: Vec<RetryAction> = state
        .retries
        .iter()
        .filter(|r| current_minute >= r.due_minute)
        .map(|r| r.action.clone())
        .collect();

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
        RetryAction::ProgramRtc | RetryAction::DisableRtc => program_alarm_for(state, now),
    }
}

fn transition_effect_completed(
    state: &mut AppState,
    completion: EffectCompletion,
) -> Vec<EffectBatch> {
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

            if state.pending_alarm_add == Some(completion.operation_id) {
                state.pending_alarm_add = None;
                if let Some(now) = state.clock.now {
                    batches.extend(program_alarm_for(state, &now));
                }
            }
        }
        EffectOutput::Persisted(PersistTarget::Todos) => {
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
            for channel in [Channel::Usb, Channel::Ble] {
                apply_confirmed_server_config(state, completion.operation_id, channel);
            }
        }
        EffectOutput::Persisted(PersistTarget::Timezone) => {
            for channel in [Channel::Usb, Channel::Ble] {
                apply_confirmed_timezone(state, completion.operation_id, channel);
            }

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

    if let Some(now) = state.clock.now {
        let current_minute = now.to_unix() / 60;
        if maybe_rearm(state, current_minute, &now) {
            batches.extend(program_alarm_for(state, &now));
        }
    }
    batches
}

fn transition_effect_failed(state: &mut AppState, failure: EffectFailure) -> Vec<EffectBatch> {
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
        EffectError::Ack(_) => {
            if let Some(commit) = state.commit_for_operation(failure.operation_id) {
                *commit = CommitState::Failed {
                    error: failure.error.clone(),
                };
                state.schedule_retry(RetryAction::Ack, now_minute);
            } else if state.pending_residue_ack == Some(failure.operation_id) {
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

        EffectError::Rtc(_)
            if state
                .pending_rtc
                .as_ref()
                .is_some_and(|p| p.operation_id() == failure.operation_id) =>
        {
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

    if matches!(failure.error, EffectError::Ble(_)) && matches!(state.screen, Screen::BlePairing(_))
    {
        state.screen = Screen::Settings {
            selected: SETTINGS_BLE_PAIRING_ROW,
        };
        state.render_generation = state.render_generation.next();
    }

    let mut batches = Vec::new();
    if render_after_failure {
        batches.push(render_batch(state));
    }

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
    let slot_occupied = match channel {
        Channel::Usb => state.pending_usb_reply.is_some(),
        Channel::Ble => state.pending_ble_reply.is_some(),
    };
    if slot_occupied {
        return vec![reply_batch(state, channel, Reply::Busy)];
    }

    match request {
        ControlRequest::SetRtc { epoch_secs } => {
            const MIN_EPOCH: u64 = 946_684_800;
            const MAX_EPOCH: u64 = 4_102_444_800;
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

            set_pending_reply(state, channel, op, ControlRequest::SyncNow, vec![op]);
            vec![batch(
                state,
                op,
                FailurePolicy::Continue,
                vec![Effect::StartSync(SyncRequest { now })],
            )]
        }
        ControlRequest::SetWifi { ssid, password } => {
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
            let persist_op = state.next_operation_id();
            let rtc_op = state.next_operation_id();
            state.alarms.alarms.clear();

            state.alarm_runtime = AlarmRuntimeState::Disarmed;
            set_pending_reply(
                state,
                channel,
                persist_op,
                ControlRequest::ClearAlarms,
                vec![persist_op, rtc_op],
            );

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

fn reply_batch(state: &mut AppState, channel: Channel, reply: ControlReply) -> EffectBatch {
    batch(
        state,
        OperationId(0),
        FailurePolicy::Continue,
        vec![Effect::Reply { channel, reply }],
    )
}

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

fn program_alarm_for(state: &mut AppState, now: &DateTime) -> Vec<EffectBatch> {
    let op = state.next_operation_id();
    match next_due(&state.alarms.alarms, now) {
        Some(alarm) => {
            let regs = match crate::alarm_schedule::alarm_regs_for(alarm, now) {
                Some(regs) => regs,

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
        let state = AppState::default();
        assert!(state.rtc_alarm_plan_confirmed());
    }

    #[test]
    fn rtc_plan_not_confirmed_while_program_in_flight() {
        let mut state = AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(8, 0)), false, true);
        let batches = update(&mut state, Event::Boot(snapshot));

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

        let batches = update(&mut state, Event::Tick(dt(9, 1)));
        assert!(!batches.iter().any(|b| b
            .effects
            .iter()
            .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)))));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));

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

        set_pending_reply(
            &mut state,
            Channel::Usb,
            op,
            ControlRequest::SyncNow,
            vec![op],
        );

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

        assert!(state.pending_usb_reply.is_some());
        assert!(state.pending_ble_reply.is_none());
    }

    #[test]
    fn same_command_on_usb_and_ble_replies_on_its_own_channel() {
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

        assert!(state.config.server_url.is_none(), "facts apply on confirm");

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
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 0;

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
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        state.config.timezone_offset_minutes = 0;

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
        state.config.timezone_offset_minutes = 120;
        let batches = update(
            &mut state,
            Event::UsbCommand(ControlRequest::SetRtc {
                epoch_secs: 946_684_800 + 10 * 3600,
            }),
        );
        assert!(
            state.pending_usb_reply.is_some(),
            "SetRtc must take a USB slot"
        );

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
    fn backward_clock_move_re_arms_the_full_sync_boundary() {
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

        assert!(update(&mut state, Event::Tick(dt(11, 0)))
            .iter()
            .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))));
        let _ = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Failed("test".into())),
        );
        assert_eq!(state.sync, SyncState::Idle);

        assert!(
            update(&mut state, Event::Tick(dt(9, 59)))
                .iter()
                .any(|b| b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))),
            "a backward clock move must re-arm auto-sync, not stall it"
        );
        let _ = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Failed("test".into())),
        );

        assert!(update(&mut state, Event::Tick(dt(9, 59)))
            .iter()
            .all(|b| !b.effects.iter().any(|e| matches!(e, Effect::StartSync(_)))));
    }

    #[test]
    fn sync_now_while_running_replies_busy_on_either_transport() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::UsbCommand(ControlRequest::SyncNow));

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

        assert!(state.pending_usb_reply.is_some());
    }

    #[test]
    fn sync_completed_ok_applies_data_then_replies_ok_on_the_requesting_transport() {
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));
        let _ = update(&mut state, Event::BleCommand(ControlRequest::SyncNow));
        assert!(matches!(state.sync, SyncState::Running { .. }));

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
        let mut state = AppState::default();
        state.clock.now = Some(dt(10, 0));

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

        assert!(batches
            .iter()
            .any(|b| { b.effects.iter().any(|e| matches!(e, Effect::Render(_))) }));

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let batches = update(
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
        let mut state = AppState::default();

        let renders = |b: &[EffectBatch]| -> usize {
            b.iter()
                .flat_map(|b| &b.effects)
                .filter(|e| matches!(e, Effect::Render(RenderRequest { .. })))
                .count()
        };

        let b = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );
        assert_eq!(renders(&b), 1, "drawer open emits one render");

        let b = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(renders(&b), 1, "drawer move emits one render");

        let b = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(renders(&b), 1, "drawer close (cancel) emits one render");

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Up)),
        );

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

        for _ in 0..3 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
            );
        }
        assert_eq!(state.screen, Screen::Settings { selected: 3 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 3 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 2 });

        for _ in 0..5 {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
            );
        }
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 2 });

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        assert_eq!(state.screen, Screen::SyncIntervalPick { selected: 4 });

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

        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(matches!(state.sync, SyncState::Running { .. }));
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartSync(_))));

        assert_eq!(state.screen, Screen::Settings { selected: 0 });
    }

    #[test]
    fn settings_sync_now_unconfigured_or_busy_is_noop() {
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
        assert_eq!(state.screen, Screen::Settings { selected: 0 });
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );

        assert_eq!(state.sync, SyncState::Idle);
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StartSync(_))));
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

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
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Waiting,
            ..Default::default()
        });

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
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Pairing,
            ..Default::default()
        });

        let _ = update(&mut state, Event::BleDisconnected);
        assert_eq!(
            state.screen,
            Screen::BlePairing(BlePairingState {
                phase: BlePairingPhase::Waiting,
                ..Default::default()
            })
        );

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
        let mut state = AppState::default();
        let _ = update(
            &mut state,
            Event::Boot(boot_snapshot(vec![], Some(dt(8, 0)), false, true)),
        );
        state.screen = Screen::BlePairing(BlePairingState {
            phase: BlePairingPhase::Waiting,
            ..Default::default()
        });

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

        let batches = update(&mut state, Event::Tick(dt(8, 0)));
        assert!(matches!(state.screen, Screen::BlePairing(_)));
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopBlePairing)));

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

        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert!(matches!(state.screen, Screen::BlePairing(_)));
        assert!(batches.iter().all(|batch| batch.effects.is_empty()));

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
        assert!(matches!(
            state.screen,
            Screen::Navigation {
                selected: 5,
                origin: NavOrigin::Settings,
                ..
            }
        ));

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Settings { selected: 0 });

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 1 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 2 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });

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

        assert!(state.alarms.alarms[0].enabled);
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );

        assert!(!state.alarms.alarms[0].enabled);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::PersistAlarmToggle { alarms, toggled_id: 1 }
                if alarms.first().is_some_and(|a| !a.enabled)
        )));

        assert!(state.pending_alarm_list_edit.is_some());

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );

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

        assert!(batches
            .iter()
            .any(|b| matches!(b.effects.first(), Some(Effect::PersistAlarms(_)))));
        assert!(state.pending_alarm_add.is_some(), "add in flight");
        assert_eq!(state.screen, Screen::AlarmList { selected: before });

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

        let before = state.alarms.alarms.len();
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::LongPressed(ButtonId::Enter)),
        );
        assert_eq!(state.alarms.alarms.len(), before);
        assert_eq!(state.screen, Screen::AlarmList { selected: before });
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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::AlarmList { selected: 0 });

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

    fn open_todo_list(state: &mut AppState, todos: Vec<Todo>) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot_todos(todos, Some(dt(9, 0)))),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 1 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });

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

        assert!(!state.todos.todos[0].done);
        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );

        assert!(state.todos.todos[0].done);
        assert!(batches.iter().flat_map(|b| &b.effects).any(|e| matches!(
            e,
            Effect::PersistTodoEdit { todos, edited_id: 1 }
                if todos.first().is_some_and(|t| t.done)
        )));

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert!(state.todos.todos[0].done);
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
            Event::Button(ButtonEvent::Pressed(ButtonId::Up)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::TodoList { selected: 0 });
    }

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

    fn open_inbox(state: &mut AppState, items: Vec<InboxItem>) {
        let _ = update(
            state,
            Event::Boot(boot_snapshot_inbox(items, Some(dt(9, 0)))),
        );
        let _ = update(state, Event::Button(ButtonEvent::LongPressed(ButtonId::Up)));

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 2 });

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

        let batches = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::InboxItem { index: 1 });
        assert!(!batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::MarkInboxRead { .. })));

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

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Down)),
        );
        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::InboxItem { index: 1 });

        let _ = update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        assert_eq!(state.screen, Screen::Inbox { selected: 1 });

        let closes = [
            ButtonEvent::Pressed(ButtonId::Up),
            ButtonEvent::Pressed(ButtonId::Down),
            ButtonEvent::LongPressed(ButtonId::Up),
            ButtonEvent::LongPressed(ButtonId::Down),
            ButtonEvent::LongPressed(ButtonId::Enter),
        ];
        for close in closes {
            let _ = update(
                &mut state,
                Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
            );
            assert_eq!(state.screen, Screen::InboxItem { index: 1 });

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

        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::StartTone)));
    }

    #[test]
    fn firing_alarm_tick_before_deadline_keeps_ringing() {
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
        let mut state = AppState::default();

        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));

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
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);

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

        assert_eq!(state.screen, Screen::AlarmRinging);
        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing { .. }
        ));

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
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);

        let _ = update(
            &mut state,
            Event::SyncCompleted(SyncResult::Ok {
                data: synced_data(vec![]),
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);

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
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let _ = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);

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

        let _ = update(
            &mut state,
            Event::RtcAlarmSnapshotReady(RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);

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

        let batches = update(&mut state, Event::Tick(dt(8, 3)));
        assert_eq!(state.screen, Screen::Home);
        assert!(batches
            .iter()
            .flat_map(|b| &b.effects)
            .any(|e| matches!(e, Effect::StopTone)));
    }

    #[test]
    fn double_enter_ring_dismisses_exactly_once() {
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

        let now = dt(9, 2);
        let batches = update(&mut state, Event::Tick(now));

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

    #[test]
    fn residue_ack_before_any_rtc_program() {
        let mut state = AppState::default();
        let snapshot = boot_snapshot(vec![alarm(1, 8, 30)], Some(dt(9, 0)), true, true);
        let batches = update(&mut state, Event::Boot(snapshot));

        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));

        let ack_op = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id)
            .unwrap();
        assert_ne!(ack_op, OperationId(0));

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

        let batches = update(&mut state, Event::Tick(dt(9, 2)));
        let retry_ack = batches
            .iter()
            .find(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm))
            .map(|b| b.operation_id);
        assert!(retry_ack.is_some());
        assert_ne!(retry_ack.unwrap(), ack_op);

        assert!(!batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_)) || e == &Effect::DisableRtcAlarm)
        }));
    }

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

        assert_eq!(state.retries.len(), 2);

        let batches = update(&mut state, Event::Tick(dt(9, 2)));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));

        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::PersistAlarms(list) if list == &state.alarms.alarms))
        }));

        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::Firing {
                ack: CommitState::InFlight { .. },
                persistence: CommitState::InFlight { .. },
                ..
            }
        ));
    }

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

        assert!(batches.iter().any(|b| {
            b.effects
                .iter()
                .any(|e| matches!(e, Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm))
        }));
    }

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

        let batches = update(&mut state, Event::Tick(dt(9, 1)));
        assert!(batches
            .iter()
            .any(|b| b.effects.contains(&Effect::AcknowledgeRtcAlarm)));
        assert!(state.pending_residue_ack.is_some());

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

    #[test]
    fn alarm_lifecycle_ring_dismiss_rearm_each_once() {
        let mut state = AppState::default();

        let snapshot = boot_snapshot(vec![alarm(1, 9, 0)], Some(dt(9, 0)), true, true);
        let boot_batches = update(&mut state, Event::Boot(snapshot));
        assert_eq!(state.screen, Screen::AlarmRinging);

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

        assert!(matches!(
            state.alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));

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

    #[test]
    fn alarm_ringing_over_sm_screens_restores_them_on_dismiss() {
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

            state.screen = screen.clone();
            let before = state.screen.clone();

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

    #[test]
    fn minute_tick_keeps_rendering_while_any_sm_screen_is_current() {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderPayload {
    pub kind: ReminderKind,
    pub lines: Vec<String>,

    pub urgent_read_ids: Vec<u64>,

    pub todo_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderPersistence {
    pub urgent_read_ids: Vec<u64>,
    pub todo_date: Option<String>,
}
