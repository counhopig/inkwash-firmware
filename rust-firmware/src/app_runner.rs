//! Firmware-side `EffectExecutor` for the host-testable `Runtime`.
//!
//! The application-state driving engine (event queue, single
//! `App::update` consumer, batch execution, EffectId allocation,
//! AbortBatch semantics, completion feedback, async-kick collection,
//! render-completion routing) lives in `inkwash_logic::runtime` and is
//! host-testable with a fake executor. This module provides the *real*
//! executor: `EffectRunner` runs each `Effect` against `DeviceContext`'s
//! drivers (RTC, NVS stores, audio, display, USB/BLE), plus the
//! render/reply helpers it needs.
//!
//! `Runtime`, `AsyncKick`, `EffectOutcome`, `EffectCategory` and
//! `err_for_category` come from `inkwash_logic`; this module re-exports
//! the names the firmware call sites use.

use anyhow::Result;

use inkwash_logic::app::{Effect, EffectOutput, RenderIntent, RenderRequest, RenderView};
use inkwash_logic::runner::{EffectCategory, EffectExecutor, EffectOutcome};

pub use inkwash_logic::runner::AsyncKick;
pub use inkwash_logic::runtime::Runtime as AppRunner;

use crate::alarms::AlarmStore;
use crate::ctx::DeviceContext;
use crate::rtc::DateTime;
use crate::todos::TodoStore;

/// Executes one `Effect` against the existing drivers, reporting the
/// outcome back to the `inkwash_logic::runtime::Runtime` consumer.
pub struct EffectRunner<'a, 'ctx> {
    ctx: &'a mut DeviceContext<'ctx>,
    last_clock: Option<DateTime>,
    /// Control replies the state machine emitted during this pump, in
    /// order, tagged with their target channel. The caller (main loop /
    /// blocking page) drains them after the pump and writes each to its
    /// transport with the correlation id of the frame that triggered it.
    replies: Vec<(
        inkwash_logic::protocol::Channel,
        inkwash_logic::protocol::Reply,
    )>,
    /// Legacy navigation destinations the state machine selected while
    /// running this pump. Opening them is deferred to the caller (after
    /// the pump releases the Runtime borrow) because the pages block and
    /// dispatch alarm/button events back through the same Runtime.
    deferred_nav: Vec<usize>,
    /// Settings row actions the state machine selected while running this
    /// pump (Sync Now / Sync Interval / BLE pairing / Sleep). Same
    /// deferral rationale as `deferred_nav`.
    deferred_settings_items: Vec<usize>,
    /// The state machine asked to open the legacy "+ ADD ALARM" editor
    /// wedge (AlarmList screen). Deferred post-pump; when it returns the
    /// caller dispatches `Event::AlarmStoreChanged` with the reloaded list.
    deferred_add_alarm: bool,
}

impl<'a, 'ctx> EffectRunner<'a, 'ctx> {
    pub fn new(ctx: &'a mut DeviceContext<'ctx>, last_clock: Option<DateTime>) -> Self {
        Self {
            ctx,
            last_clock,
            replies: Vec::new(),
            deferred_nav: Vec::new(),
            deferred_settings_items: Vec::new(),
            deferred_add_alarm: false,
        }
    }

    /// Drain the replies emitted by the state machine during the last
    /// pump, in emission order.
    pub fn take_replies(
        &mut self,
    ) -> Vec<(
        inkwash_logic::protocol::Channel,
        inkwash_logic::protocol::Reply,
    )> {
        std::mem::take(&mut self.replies)
    }

    /// Drain the legacy navigation destinations the state machine selected
    /// during the last pump. The caller opens each page after the pump.
    pub fn take_deferred_nav(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.deferred_nav)
    }

    /// Drain the Settings row actions the state machine selected during the
    /// last pump. The caller runs each wedge after the pump.
    pub fn take_deferred_settings(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.deferred_settings_items)
    }

    /// Whether the state machine asked to open the "+ ADD ALARM" editor
    /// wedge during the last pump.
    pub fn take_deferred_add_alarm(&mut self) -> bool {
        std::mem::take(&mut self.deferred_add_alarm)
    }
}

impl EffectExecutor for EffectRunner<'_, '_> {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
        match effect {
            // ---- synchronous effects ---------------------------------------
            Effect::PersistAlarms(list) => match AlarmStore::save(self.ctx.alarm_store, list) {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Alarms,
                ))),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistTodos(list) => {
                match crate::todos::TodoStore::save(self.ctx.todo_store, list) {
                    Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Todos,
                    ))),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::PersistInbox(list) => {
                match crate::inbox::InboxStore::save(self.ctx.inbox_store, list) {
                    Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Inbox,
                    ))),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::PersistConfig(cfg) => match self.ctx.counters.save_device_config(cfg) {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Config,
                ))),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::PersistTimezone(offset_minutes) => match self
                .ctx
                .counters
                .save_timezone_offset_minutes(*offset_minutes)
            {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Timezone,
                ))),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::ApplySyncedData(data) => {
                // The one place a sync's merged server state is written:
                // the network task only transports; the state machine owns
                // the apply ordering, and this executor performs the NVS
                // writes (alarm/todo/inbox replace + dirty-clear + inbox
                // read acks + ETag) as a single confirmable op.
                let result = (|| -> Result<()> {
                    self.ctx.alarm_store.save(&data.alarms)?;
                    self.ctx.todo_store.save(&data.todos)?;
                    self.ctx.inbox_store.save(&data.inbox)?;
                    self.ctx.inbox_store.ack_read(&data.inbox_read_acked)?;
                    // The merged server state reflects everything uploaded,
                    // so the pending local changes are spent.
                    self.ctx.alarm_store.clear_dirty()?;
                    self.ctx.todo_store.clear_dirty()?;
                    if let Some(etag) = data.etag.as_deref() {
                        self.ctx.counters.save_sync_etag(etag)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::SyncApply,
                    ))),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::WriteRtcTime(dt) => match self.ctx.rtc.write_time(dt) {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcTimeWritten)),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::PersistSyncMetadata(meta) => {
                let result = (|| -> Result<()> {
                    if let Some(etag) = meta.etag.as_deref() {
                        self.ctx.counters.save_sync_etag(etag)?;
                    }
                    if let Some(epoch) = meta.last_sync_epoch {
                        self.ctx.counters.set_last_sync_epoch(epoch)?;
                    }
                    if let Some(epoch) = meta.rtc_align_epoch {
                        self.ctx.counters.set_rtc_align_epoch(epoch)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::SyncMetadata,
                    ))),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::ClearSyncEtag => match self.ctx.counters.clear_sync_etag() {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::SyncMetadata,
                ))),
                Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
            },
            Effect::ProgramRtcAlarm(regs) => match self.ctx.rtc.program(regs) {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed)),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::DisableRtcAlarm => match self.ctx.rtc.disable() {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed)),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::AcknowledgeRtcAlarm => match self.ctx.rtc.acknowledge() {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::AckDone)),
                Err(err) => Err((EffectCategory::Ack, format!("{err:#}"))),
            },
            Effect::StartTone => {
                // The state machine emits StartTone when entering Firing.
                // The ES8311 codec starts muted; without `set_mute(false)`
                // here, every subsequent `play_sine_stereo` call from
                // `alarms::ring_screen` writes to I2S but produces no
                // sound. ring_screen does the actual tone bursts; this
                // effect only owns the codec unmute so the first alarm
                // *and* every alarm after a StopTone are audible.
                if let Some(audio) = self.ctx.board.audio.as_mut() {
                    if let Err(err) = audio.set_mute(false) {
                        return Err((EffectCategory::Tone, format!("{err:#}")));
                    }
                }
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            Effect::StopTone => {
                // StopTone must NOT call `set_mute(true)` - doing so
                // leaves the codec muted after dismiss and silences every
                // future alarm. ring_screen already stops the I2S stream
                // (`drain_and_disable` after `play_sine_stereo`); we
                // simply acknowledge the effect so the state machine can
                // transition to WaitingForRearm.
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            // ---- asynchronous / out-of-band effects -------------------------
            Effect::OpenNavigationDestination { destination } => {
                // The executor NEVER opens a legacy blocking page inside a
                // pump: a blocking page dispatches RTC alarm snapshots /
                // button events through the same shared Runtime, which would
                // re-enter `borrow_mut` on the RefCell the outer pump is
                // holding and panic. Defer the open to the caller: the main
                // loop drains this buffer after the pump (borrow released)
                // and opens the page there, mirroring the legacy inline
                // `open_navigation` dispatch structure. The state machine
                // already transitioned to the target screen before the
                // effect ran, so the page opens onto the correct state.
                self.deferred_nav.push(*destination);
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::OpenSettingsItem { item } => {
                // Same deferral as OpenNavigationDestination: Settings row
                // actions (Sync Now / Sync Interval / BLE pairing / Sleep)
                // are still legacy blocking screens and must not run inside
                // a pump. The main loop opens the wedge after the pump and
                // re-renders the Settings screen on return.
                self.deferred_settings_items.push(*item);
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::OpenAddAlarm => {
                // Same deferral: "+ ADD ALARM" runs the legacy editor wedge
                // after the pump. The wedge persists to the alarm store; the
                // main loop dispatches Event::AlarmStoreChanged with the
                // reloaded list so the SM adopts the new alarm.
                self.deferred_add_alarm = true;
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::PersistAlarmToggle { alarms, toggled_id } => {
                // The AlarmList screen's confirmable enabled-toggle: save the
                // list and mark the row dirty (two-way sync contract). The
                // completion feeds back as Persisted(Alarms), which releases
                // the machine's one-at-a-time toggle gate and re-programs
                // the RTC slot.
                match (
                    AlarmStore::save(self.ctx.alarm_store, alarms),
                    self.ctx.alarm_store.mark_dirty(*toggled_id),
                ) {
                    (Ok(()), Ok(())) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Alarms,
                    ))),
                    (Err(err), _) => Err((EffectCategory::Persist, format!("{err:#}"))),
                    (Ok(()), Err(err)) => Err((
                        EffectCategory::Persist,
                        format!("dirty mark failed: {err:#}"),
                    )),
                }
            }
            Effect::PersistTodoEdit { todos, edited_id } => {
                // The TodoList screen's confirmable done-toggle / importance
                // cycle: save the list and mark the row dirty (two-way sync
                // contract). The completion feeds back as Persisted(Todos),
                // which releases the machine's one-at-a-time edit gate.
                match (
                    TodoStore::save(self.ctx.todo_store, todos),
                    self.ctx.todo_store.mark_dirty(*edited_id),
                ) {
                    (Ok(()), Ok(())) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                        inkwash_logic::app::PersistTarget::Todos,
                    ))),
                    (Err(err), _) => Err((EffectCategory::Persist, format!("{err:#}"))),
                    (Ok(()), Err(err)) => Err((
                        EffectCategory::Persist,
                        format!("dirty mark failed: {err:#}"),
                    )),
                }
            }
            Effect::Reply { channel, reply } => {
                // Record the reply for the caller to write after the pump:
                // only the caller knows the correlation id of the frame that
                // triggered it, and the reply must go to exactly one
                // transport (USB and BLE have independent pending slots).
                self.replies.push((*channel, reply.clone()));
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::Render(req) => match render_view_into(self.ctx, self.last_clock, req) {
                Ok(request_id) => {
                    // The panel refresh is in flight and its EPD completion
                    // will carry `request_id`. Return async *with* the id so
                    // the caller can correlate the eventual completion to
                    // this kick.
                    Ok(EffectOutcome::AsyncWithId(request_id))
                }
                Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
            },
            Effect::StartSync(req) => {
                // The sync task owns Wi-Fi; this only dispatches the
                // request. `Ok(true)` = dispatched (async, receipt later);
                // `Ok(false)` = another Wi-Fi operation is already in
                // flight, so this sync did NOT start - fail immediately so
                // the state machine's single-flight lock is not left
                // waiting on a receipt that will never be this sync's.
                let result = self.ctx.start_sync(req.now);
                match result {
                    Ok(true) => Ok(EffectOutcome::Async),
                    Ok(false) => Err((
                        EffectCategory::Sync,
                        "another Wi-Fi operation is already in progress".into(),
                    )),
                    Err(err) => Err((EffectCategory::Sync, format!("{err:#}"))),
                }
            }
            Effect::StartSetWifi(creds) => {
                // Wi-Fi verification + save runs on the sync task (it owns
                // the Wi-Fi driver). `Ok(true)` = dispatched (receipt
                // later); `Ok(false)` = another Wi-Fi operation is in
                // flight, so fail immediately rather than leave the
                // state machine's pending slot waiting forever.
                let result = self.ctx.start_set_wifi(creds.clone());
                match result {
                    Ok(true) => Ok(EffectOutcome::Async),
                    Ok(false) => Err((
                        EffectCategory::Sync,
                        "another Wi-Fi operation is already in progress".into(),
                    )),
                    Err(err) => Err((EffectCategory::Sync, format!("{err:#}"))),
                }
            }
            Effect::StartBlePairing(_req) => Ok(EffectOutcome::Async),
            Effect::StopBlePairing => Ok(EffectOutcome::Async),
            Effect::EnterLightSleep(_plan) => {
                if let Err(err) = crate::power::configure_light_sleep() {
                    return Err((EffectCategory::Sleep, format!("{err:#}")));
                }
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::EnterDeepSleep(plan) => {
                crate::power::enter_deep_sleep_with_wakeups(plan.maintenance);
            }
        }
    }
}

/// Draws the current SM screen's canvas and submits the refresh the state
/// machine requested. The renderer is state-driven (Stage-4/5): the request
/// names the surface to draw - Home, the GO TO drawer overlay, or the
/// Settings list - so the executor draws the SM screen instead of a
/// call-site rectangle.
///
/// Home canvases are always fully re-rendered before the refresh (a minute
/// tick re-draws Home then partial-refreshes only the clock rect). The
/// drawer overlay is drawn on top of the same Home canvas; a Partial
/// refresh of the Navigation view updates only the overlay rect (cheap
/// selection moves on the drawer), while open/close/select stay Full.
/// Settings is a full screen: its Partial updates are full list redraws
/// (the list region; cheap enough on a list that redraws as one block).
///
/// Returns the EPD request id so the caller can correlate the eventual
/// `EpdCompletion` back to this render's `AsyncKick`.
fn render_view_into(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    req: &RenderRequest,
) -> Result<u64> {
    draw_sm_surface(ctx, clock, req.view);
    let request_id = match (req.view, req.intent) {
        (_, RenderIntent::Full) => ctx.board.display.refresh_full(),
        (RenderView::Home, RenderIntent::Partial) => {
            ctx.board.display.refresh_partial(crate::canvas::Rect {
                x: 16,
                y: 36,
                width: 368,
                height: 92,
            })
        }
        // Navigation view partials redraw just the GO TO bar (selection
        // moves); the rest of the Home canvas stays put on the panel.
        (RenderView::Navigation { .. }, RenderIntent::Partial) => ctx
            .board
            .display
            .refresh_partial(crate::screens::NAV_BAR_RECT),
        // Settings / AlarmList / TodoList row moves redraw the list region
        // (rows share one chrome block; a per-row diff is a Stage-5
        // RenderPlan concern).
        (
            RenderView::Settings { .. }
            | RenderView::AlarmList { .. }
            | RenderView::TodoList { .. },
            RenderIntent::Partial,
        ) => ctx.board.display.refresh_partial(crate::canvas::Rect {
            x: 8,
            y: 34,
            width: 384,
            height: 226,
        }),
    };
    request_id.map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// Renders the visible surface named by `view` into the shared canvas.
/// Home is always drawn first (the drawer opens over it from Home); the
/// Navigation view then overlays the GO TO bar, and the Settings / AlarmList
/// / TodoList views draw their own lists instead. Shared with main's legacy
/// dirty-path redraws so the canvas always matches the SM's current screen
/// (a dirty full redraw while the drawer is open must keep the overlay).
pub(crate) fn draw_sm_surface(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    view: RenderView,
) {
    match view {
        RenderView::Home => draw_home_surface(ctx, clock),
        RenderView::Navigation { selected } => {
            draw_home_surface(ctx, clock);
            let mut canvas = ctx.board.display.canvas_mut();
            crate::screens::draw_navigation_bar(&mut canvas, selected);
        }
        RenderView::Settings { selected } => {
            let mut canvas = ctx.board.display.canvas_mut();
            crate::screens::draw_settings(&mut canvas, selected);
        }
        RenderView::AlarmList { selected } => {
            crate::screens::draw_alarm_list(ctx.board, ctx.alarm_store, selected);
        }
        RenderView::TodoList { selected } => {
            crate::screens::draw_todo_list(ctx.board, ctx.todo_store, selected, clock.as_ref());
        }
    }
}

/// Draws the idle Home canvas (clock + cards). Shared by Home and
/// Navigation views (the drawer overlays the Home canvas).
fn draw_home_surface(ctx: &mut DeviceContext<'_>, clock: Option<DateTime>) {
    let next_alarm = clock
        .as_ref()
        .and_then(|dt| crate::screens::next_alarm_label(ctx.alarm_store, dt));
    let todo_summary = crate::screens::todo_summary(ctx.todo_store, clock.as_ref());
    let unread_inbox = ctx.inbox_store.unread_count().unwrap_or(0);
    let wifi_configured = ctx
        .counters
        .wifi_creds()
        .map(|creds| creds.is_some())
        .unwrap_or(false);
    let battery_percent = ctx.board.battery_percent();
    let charge = ctx.board.charge_snapshot();
    ctx.board.display.render_home(
        clock.as_ref(),
        next_alarm.as_ref().map(|label| label.time.as_str()),
        next_alarm.as_ref().and_then(|label| label.date.as_deref()),
        next_alarm.as_ref().map(|label| label.days_left),
        todo_summary.pending,
        todo_summary.due_today,
        unread_inbox,
        wifi_configured,
        battery_percent,
        charge,
    );
}
