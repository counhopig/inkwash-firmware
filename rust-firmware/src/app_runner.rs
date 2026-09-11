use anyhow::Result;

use inkwash_logic::app::{Effect, EffectOutput, RenderView};
use inkwash_logic::runner::{EffectCategory, EffectExecutor, EffectOutcome};

pub use inkwash_logic::runner::AsyncKick;
pub use inkwash_logic::runtime::Runtime as AppRunner;

use crate::alarms::AlarmStore;
use crate::ctx::DeviceContext;
use crate::inbox::InboxItem;
use crate::rtc::DateTime;
use crate::todos::{Todo, TodoStore};

struct RenderFacts {
    alarms: Vec<crate::alarms::StoredAlarm>,
    todos: Vec<Todo>,
    inbox: Vec<InboxItem>,
    wifi_configured: bool,
}

fn render_facts(ctx: &DeviceContext<'_>) -> RenderFacts {
    let state = ctx.app_runner.borrow();
    RenderFacts {
        alarms: state.state().alarms.alarms.clone(),
        todos: state.state().todos.todos.clone(),
        inbox: state.state().inbox.items.clone(),
        wifi_configured: state.state().config.wifi_configured(),
    }
}

pub struct EffectRunner<'a, 'ctx> {
    ctx: &'a mut DeviceContext<'ctx>,
    last_clock: Option<DateTime>,

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
            Effect::PersistWifiCredentials(creds) => match self.ctx.counters.save_wifi_creds(creds)
            {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::WifiCredentials,
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
                let result = (|| -> Result<()> {
                    self.ctx.alarm_store.save(&data.alarms)?;
                    self.ctx.todo_store.save(&data.todos)?;
                    self.ctx.inbox_store.save(&data.inbox)?;
                    self.ctx.inbox_store.ack_read(&data.inbox_read_acked)?;

                    self.ctx
                        .alarm_store
                        .clear_dirty_ids(&data.uploaded_alarm_ids)?;
                    self.ctx
                        .todo_store
                        .clear_dirty_ids(&data.uploaded_todo_ids)?;
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
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcTimeWritten(*dt))),
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
            Effect::ClearRtcAlignEpoch => match self.ctx.counters.clear_rtc_align_epoch() {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
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
            Effect::StartTone => match self.ctx.audio_task {
                Some(task) => {
                    if let Err(err) = task.start_alarm_tone() {
                        Err((EffectCategory::Tone, format!("{err:#}")))
                    } else {
                        Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
                    }
                }
                None => Err((
                    EffectCategory::Tone,
                    "audio codec unavailable at boot".into(),
                )),
            },
            Effect::StartReminderTone(kind) => match self.ctx.audio_task {
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
            },
            Effect::StopTone => {
                if let Some(task) = self.ctx.audio_task {
                    if let Err(err) = task.stop() {
                        return Err((EffectCategory::Tone, format!("{err:#}")));
                    }
                }
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }

            Effect::MarkInboxRead { seq } => {
                if let Err(err) = self.ctx.inbox_store.mark_read(*seq) {
                    Err((EffectCategory::Persist, format!("{err:#}")))
                } else {
                    Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                }
            }
            Effect::PersistReminder(persistence) => {
                let result = (|| -> anyhow::Result<()> {
                    for seq in &persistence.urgent_read_ids {
                        self.ctx.inbox_store.mark_read(*seq)?;
                    }
                    if let Some(date) = persistence.todo_date.as_deref() {
                        self.ctx.counters.set_todo_reminded_date(date)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::ReminderPersisted)),
                    Err(err) => Err((EffectCategory::Persist, format!("{err:#}"))),
                }
            }
            Effect::CollectReminderFacts(_) => Err((
                EffectCategory::Persist,
                "reminder fact collection requires the effect worker".into(),
            )),
            Effect::SetSyncInterval { minutes } => {
                if let Err(err) = self.ctx.counters.set_sync_interval_minutes(*minutes) {
                    Err((EffectCategory::Persist, format!("{err:#}")))
                } else {
                    Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                }
            }
            Effect::PersistAlarmToggle { alarms, toggled_id } => {
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
                self.replies.push((*channel, reply.clone()));
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::Render(req) => {
                let renders = self.ctx.pending_renders.clone();
                let plan = renders.borrow().plan_for(&req.view_model);
                match plan {
                    inkwash_logic::render_plan::RenderPlan::Noop => {
                        Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
                    }
                    inkwash_logic::render_plan::RenderPlan::Partial { region, .. } => {
                        let facts = render_facts(self.ctx);
                        draw_sm_surface(self.ctx, self.last_clock, req.view.clone(), &facts);
                        let rect = partial_region_rect(region);
                        match self.ctx.board.display.refresh_partial(rect) {
                            Ok(request_id) => Ok(EffectOutcome::AsyncWithId(request_id)),
                            Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
                        }
                    }
                    inkwash_logic::render_plan::RenderPlan::Full { .. } => {
                        let facts = render_facts(self.ctx);
                        draw_sm_surface(self.ctx, self.last_clock, req.view.clone(), &facts);
                        match self.ctx.board.display.refresh_full() {
                            Ok(request_id) => Ok(EffectOutcome::AsyncWithId(request_id)),
                            Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
                        }
                    }
                }
            }
            Effect::StartSync(req) => {
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
            Effect::PollUrgent => match self.ctx.start_urgent_poll() {
                Ok(()) => Ok(EffectOutcome::Async),
                Err(err) => Err((EffectCategory::Sync, format!("{err:#}"))),
            },
            Effect::StartSetWifi(creds) => {
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
            Effect::StartBlePairing(req) => self
                .ctx
                .start_ble_pairing(req)
                .and_then(|started| {
                    if started {
                        Ok(EffectOutcome::Async)
                    } else {
                        Err(anyhow::anyhow!(
                            "Wi-Fi is busy or BLE radio is still stopping"
                        ))
                    }
                })
                .map_err(|err| {
                    (
                        EffectCategory::Ble,
                        format!("BLE worker start failed: {err:#}"),
                    )
                }),
            Effect::StopBlePairing => self
                .ctx
                .stop_ble_pairing()
                .map(|()| EffectOutcome::Async)
                .map_err(|err| {
                    (
                        EffectCategory::Ble,
                        format!("BLE worker stop failed: {err:#}"),
                    )
                }),
            Effect::EnterLightSleep(_plan) => {
                if let Err(err) = crate::power::configure_light_sleep() {
                    return Err((EffectCategory::Sleep, format!("{err:#}")));
                }
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::DisableLightSleep => crate::power::disable_light_sleep()
                .map(|()| EffectOutcome::Completed(EffectOutput::LightSleepDisabled))
                .map_err(|err| (EffectCategory::Sleep, format!("{err:#}"))),
            Effect::EnterDeepSleep(plan) => {
                match crate::power::enter_deep_sleep_with_wakeups(plan.maintenance) {
                    Ok(()) => Ok(EffectOutcome::Async),
                    Err(err) => Err((EffectCategory::Sleep, format!("{err:#}"))),
                }
            }
            Effect::PrepareSleep {
                token,
                maintenance,
                light_wake_after_ms: _,
            } => match token.kind {
                inkwash_logic::power_state::SleepKind::Light => {
                    crate::power::prepare_light_sleep_wakeups()
                }
                inkwash_logic::power_state::SleepKind::Deep => {
                    crate::power::prepare_deep_sleep_wakeups(*maintenance)
                }
            }
            .map(|()| EffectOutcome::Async)
            .map_err(|err| {
                (
                    EffectCategory::Sleep,
                    format!("sleep prepare failed: {err:#}"),
                )
            }),
            Effect::CommitSleep(_) => Ok(EffectOutcome::Async),
        }
    }
}

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

pub(crate) fn partial_region_rect(
    region: inkwash_logic::render_plan::PartialRegion,
) -> crate::canvas::Rect {
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
            y: inkwash_logic::list_window::LIST_REGION_Y as u16,
            width: 384,
            height: inkwash_logic::list_window::LIST_REGION_HEIGHT as u16,
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

fn draw_sm_surface(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    view: RenderView,
    facts: &RenderFacts,
) {
    match view {
        RenderView::Home => draw_home_surface(ctx, clock, facts),
        RenderView::Navigation {
            selected,
            underlying,
        } => {
            draw_sm_surface(ctx, clock, *underlying, facts);
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
            crate::screens::draw_alarm_list(ctx.board, &facts.alarms, selected);
        }
        RenderView::TodoList { selected } => {
            crate::screens::draw_todo_list(ctx.board, &facts.todos, selected, clock.as_ref());
        }
        RenderView::Inbox { selected } => {
            crate::screens::draw_inbox_list(ctx.board, &facts.inbox, selected);
        }
        RenderView::InboxItem { index } => {
            crate::screens::draw_inbox_item_detail(ctx.board, &facts.inbox, index);
        }
        RenderView::Calendar { selected_day, .. } => {
            crate::screens::draw_calendar_grid(ctx, clock.as_ref(), selected_day, &facts.todos);
        }
        RenderView::WeekView { year, month, day } => {
            crate::screens::draw_week_view(ctx, &facts.todos, year, month, day, clock.as_ref());
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

fn draw_home_surface(ctx: &mut DeviceContext<'_>, clock: Option<DateTime>, facts: &RenderFacts) {
    let next_alarm = clock
        .as_ref()
        .and_then(|dt| crate::screens::next_alarm_label_from(&facts.alarms, dt));
    let todo_summary = crate::screens::todo_summary_from(&facts.todos, clock.as_ref());
    let unread_inbox = facts.inbox.iter().filter(|item| !item.read).count();
    let wifi_configured = facts.wifi_configured;
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
