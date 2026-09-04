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

use inkwash_logic::app::{Effect, EffectOutput, RenderView};
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
}

impl<'a, 'ctx> EffectRunner<'a, 'ctx> {
    pub fn new(ctx: &'a mut DeviceContext<'ctx>, last_clock: Option<DateTime>) -> Self {
        Self {
            ctx,
            last_clock,
            replies: Vec::new(),
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
                // The audio task (the sole owner of the ES8311 codec) starts
                // the repeating alarm ring; this effect only dispatches the
                // command and acknowledges - the main loop never blocks on a
                // tone. Audio unavailability degrades cleanly (the effect
                // still completes; the ring stays visual + dismissable).
                match self.ctx.audio_task {
                    Some(task) => {
                        if let Err(err) = task.start_alarm_tone() {
                            Err((EffectCategory::Tone, format!("{err:#}")))
                        } else {
                            Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
                        }
                    }
                    None => {
                        // No codec at boot: report the failure so the SM can
                        // record the degraded audio state, but the ring
                        // lifecycle (visual + dismiss) must keep working.
                        Err((
                            EffectCategory::Tone,
                            "audio codec unavailable at boot".into(),
                        ))
                    }
                }
            }
            Effect::StartReminderTone(kind) => {
                // Start the reminder's attention tone through the audio task
                // (siren for urgent, bounded beep for todo). Non-blocking;
                // audio unavailability degrades cleanly (the effect still
                // completes; the overlay stays visible + dismissable).
                match self.ctx.audio_task {
                    Some(task) => {
                        let result = match kind {
                            inkwash_logic::app::ReminderKind::Urgent => task.start_siren(),
                            inkwash_logic::app::ReminderKind::Todo => task.beep_todo(),
                        };
                        if let Err(err) = result {
                            Err((EffectCategory::Tone, format!("{err:#}")))
                        } else {
                            Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
                        }
                    }
                    None => Err((
                        EffectCategory::Tone,
                        "audio codec unavailable at boot".into(),
                    )),
                }
            }
            Effect::StopTone => {
                // Ask the audio task to stop the ring. The effect completes
                // immediately (the sound fades within a burst gap); the SM
                // transitions to WaitingForRearm without waiting for audio.
                if let Some(task) = self.ctx.audio_task {
                    if let Err(err) = task.stop() {
                        return Err((EffectCategory::Tone, format!("{err:#}")));
                    }
                }
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            // ---- asynchronous / out-of-band effects -------------------------
            Effect::MarkInboxRead { seq } => {
                // Fire-and-forget local persist of the read mark: the
                // executor marks `seq` read + adds it to the pending-read
                // set (two-way sync uploads later). The SM already set the
                // item's read flag optimistically; nothing to track.
                if let Err(err) = self.ctx.inbox_store.mark_read(*seq) {
                    Err((EffectCategory::Persist, format!("{err:#}")))
                } else {
                    Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                }
            }
            Effect::SetSyncInterval { minutes } => {
                // Fire-and-forget persistence of the SYNC INTERVAL picker
                // choice: the executor writes the NVS counters the sync
                // scheduler reads for its cadence.
                if let Err(err) = self.ctx.counters.set_sync_interval_minutes(*minutes) {
                    Err((EffectCategory::Persist, format!("{err:#}")))
                } else {
                    Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                }
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
            Effect::Render(req) => {
                // Stage 5: the refresh plan is derived by diffing the
                // request's ViewModel against the renderer's cached
                // last-shown ViewModel (plan_render in logic). Noop renders
                // nothing to the panel and completes synchronously; Partial
                // and Full draw the requested surface and submit an EPD
                // refresh, returning async so the completion correlates by
                // request id.
                let renders = self.ctx.pending_renders.clone();
                let plan = renders.borrow().plan_for(&req.view_model);
                match plan {
                    inkwash_logic::render_plan::RenderPlan::Noop => {
                        Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                    }
                    inkwash_logic::render_plan::RenderPlan::Partial { region, .. } => {
                        draw_sm_surface(self.ctx, self.last_clock, req.view.clone());
                        let rect = partial_region_rect(region);
                        match self.ctx.board.display.refresh_partial(rect) {
                            Ok(request_id) => Ok(EffectOutcome::AsyncWithId(request_id)),
                            Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
                        }
                    }
                    inkwash_logic::render_plan::RenderPlan::Full { .. } => {
                        draw_sm_surface(self.ctx, self.last_clock, req.view.clone());
                        match self.ctx.board.display.refresh_full() {
                            Ok(request_id) => Ok(EffectOutcome::AsyncWithId(request_id)),
                            Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
                        }
                    }
                }
            }
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
            Effect::StartBlePairing(_req) => {
                // Bring the NimBLE control channel up (advertising started,
                // control service registered). The caller (SM) entered
                // Screen::BlePairing; radio lifecycle events (connect/
                // disconnect) are reported by the NimBLE callbacks through
                // the unified event queue when wired. Starting advertising
                // is synchronous from the caller's view (BleControl::start
                // blocks until advertising is up), so complete immediately.
                // The slot teardown on session exit is StopBlePairing.
                match crate::ble_control::BleControl::start() {
                    Ok(ble) => {
                        *self.ctx.ble_control = Some(ble);
                        Ok(EffectOutcome::Completed(EffectOutput::BlePairingDone))
                    }
                    Err(err) => Err((
                        EffectCategory::Ble,
                        format!("BLE advertising start failed: {err:#}"),
                    )),
                }
            }
            Effect::StopBlePairing => {
                // Tear the radio down: dropping the slot runs BleControl's
                // Drop (nimble_port deinit). Synchronous completion - the
                // SM does not await an external event for teardown.
                if let Some(ble) = self.ctx.ble_control.take() {
                    drop(ble);
                }
                Ok(EffectOutcome::Completed(EffectOutput::BlePairingDone))
            }
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

/// Maps a Stage-5 `PartialRegion` to the panel rectangle that region
/// occupies. The region already encodes the *kind* of local change (Home
/// clock, drawer bar, list block, calendar grid, or a whole-surface repaint
/// for read-only pages); the refresh is submitted for exactly that rect.
/// Stage-5 renderer cache terminal update, called from the EPD-completion
/// path after a render kick was matched and consumed.
///
/// Updates the renderer's private "last successfully shown" ViewModel cache
/// exactly per firmware-architecture.md: only a success whose render
/// generation is still current replaces `last_shown` (Full resets the
/// partial counter, Partial increments it, Noop leaves it); a failure
/// invalidates the cache so the next render is a recovery Full; a
/// superseded completion never touches the cache (its pixels never reached
/// the panel - the replacement request updates the cache on its own
/// completion). `#[inline(never)]` keeps this out of the completion loop's
/// inlined frame.
#[inline(never)]
pub(crate) fn apply_render_cache_terminal(
    registry: &std::rc::Rc<std::cell::RefCell<inkwash_logic::epd_registry::RenderRegistry>>,
    kick: &AsyncKick,
    failed: bool,
    current_generation: inkwash_logic::app::RenderGeneration,
) {
    let mut reg = registry.borrow_mut();
    if failed {
        reg.invalidate_cache();
        return;
    }
    let generation_is_current = kick
        .render_generation
        .is_some_and(|g| g == current_generation);
    reg.note_kick_terminal(kick, true, generation_is_current);
}

fn partial_region_rect(region: inkwash_logic::render_plan::PartialRegion) -> crate::canvas::Rect {
    match region {
        inkwash_logic::render_plan::PartialRegion::Clock => crate::canvas::Rect {
            x: 16,
            y: 36,
            width: 368,
            height: 92,
        },
        inkwash_logic::render_plan::PartialRegion::NavBar => crate::screens::NAV_BAR_RECT,
        inkwash_logic::render_plan::PartialRegion::List => crate::canvas::Rect {
            x: 8,
            y: 34,
            width: 384,
            height: 226,
        },
        inkwash_logic::render_plan::PartialRegion::CalendarGrid => crate::canvas::Rect {
            x: 0,
            y: 36,
            width: 400,
            height: 264,
        },
        inkwash_logic::render_plan::PartialRegion::Surface => crate::canvas::Rect {
            x: 0,
            y: 36,
            width: 400,
            height: 264,
        },
    }
}

/// Renders the visible surface named by `view` into the shared canvas.
/// Home is always drawn first (the drawer opens over it from Home); the
/// Navigation view then overlays the GO TO bar, and the Settings / AlarmList
/// / TodoList / Inbox / Calendar views draw their own content instead.
/// Shared with main's legacy dirty-path redraws so the canvas always matches
/// the SM's current screen (a dirty full redraw while the drawer is open
/// must keep the overlay).
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
        RenderView::SyncInterval { selected } => {
            let mut canvas = ctx.board.display.canvas_mut();
            crate::screens::draw_sync_interval(&mut canvas, selected);
        }
        RenderView::AlarmList { selected } => {
            crate::screens::draw_alarm_list(ctx.board, ctx.alarm_store, selected);
        }
        RenderView::TodoList { selected } => {
            crate::screens::draw_todo_list(ctx.board, ctx.todo_store, selected, clock.as_ref());
        }
        RenderView::Inbox { selected } => {
            crate::screens::draw_inbox_list(ctx.board, ctx.inbox_store, selected);
        }
        RenderView::InboxItem { index } => {
            crate::screens::draw_inbox_item_detail(ctx, index, clock.as_ref());
        }
        RenderView::Calendar { selected_day, .. } => {
            // The grid always renders the current clock month (matching the
            // SM, which keeps its year/month synced to the clock on every
            // tick); only the day cursor comes from the view.
            crate::screens::draw_calendar_grid(ctx, clock.as_ref(), selected_day);
        }
        RenderView::WeekView { year, month, day } => {
            crate::screens::draw_week_view(ctx, year, month, day, clock.as_ref());
        }
        RenderView::NumberPick { stage, value } => {
            crate::screens::draw_number_pick(ctx.board, stage, value);
        }
        RenderView::BlePairing => {
            crate::screens::draw_ble_pairing(ctx.board);
        }
        RenderView::AlarmRinging => {
            crate::screens::draw_alarm_ringing(ctx.board);
        }
        RenderView::Reminder { kind, lines } => {
            crate::screens::draw_reminder(ctx.board, kind, lines.as_slice());
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
