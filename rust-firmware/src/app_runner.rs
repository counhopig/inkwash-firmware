//! AppRunner: the firmware-side driver of `inkwash_logic::app::update`.
//!
//! Migration step 2 (see `docs/firmware-architecture.md` §迁移边界) wires the
//! boot, home, RTC alarm, and sleep flows through a single state machine +
//! event loop. The state machine itself lives in
//! `inkwash_logic::app` (host-testable, no ESP-IDF) and only deals in pure
//! data; this module is the bridge that:
//!
//! - owns the `AppState` (one business-state owner; per architecture §应用
//!   状态机);
//! - drives `update(state, event) -> Vec<EffectBatch>` for every event the
//!   firmware collects;
//! - runs each returned `EffectBatch` against the existing drivers via the
//!   `EffectRunner` helper, producing `EffectCompleted` / `EffectFailed`
//!   events that are fed back into the state machine.
//!
//! Per-category error mapping (P1 #4): each `Effect` variant maps its
//! `anyhow::Error` to the matching `EffectError` variant so the state
//! machine schedules the correct retry path. FailurePolicy::AbortBatch
//! is honoured (P1 #5): the first failure stops the rest of the batch
//! without rolling back successful writes. EffectIds are assigned in
//! declared order inside each batch (P1 #5): the same `Effect` always
//! carries the same id within one batch, so completion/failure events
//! correlate precisely.

use anyhow::Result;

use inkwash_logic::app::{
    self, AppState, Effect, EffectBatchId, EffectCompletion, EffectError, EffectFailure, EffectId,
    EffectOutput, Event, FailurePolicy, OperationId, RenderGeneration, RenderRequest,
};
use inkwash_logic::protocol::ControlReply;

use crate::alarms::AlarmStore;
use crate::control::{self, Reply};
use crate::ctx::DeviceContext;
use crate::rtc::DateTime;

/// Owns the application's `AppState` and drives every event through the
/// pure transition function.
pub struct AppRunner {
    state: AppState,
    /// Most recent RTC read - threaded into `Effect::Render` so the home
    /// screen does not need a fresh I2C transaction for each refresh.
    last_clock: Option<DateTime>,
    /// In-flight kicks returned by `dispatch`. The main loop drains them
    /// and tracks each render so the matching EPD completion can be fed
    /// back as `EffectCompleted(RenderDone)`.
    pending_kicks: Vec<AsyncKick>,
}

impl AppRunner {
    pub fn new() -> Self {
        Self {
            state: AppState::default(),
            last_clock: None,
            pending_kicks: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn from_state(state: AppState) -> Self {
        Self {
            state,
            last_clock: None,
            pending_kicks: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    #[allow(dead_code)]
    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn set_last_clock(&mut self, clock: Option<DateTime>) {
        self.last_clock = clock;
    }

    /// Dispatch a single event through the state machine and run every
    /// returned effect batch against the existing drivers. Asynchronous
    /// kicks are stashed in `self.pending_kicks`; callers drain them via
    /// `take_pending_kicks`.
    pub fn dispatch(&mut self, event: Event, ctx: &mut DeviceContext<'_>) -> Result<()> {
        let batches = app::update(&mut self.state, event);
        for batch in batches {
            self.run_batch(batch, ctx)?;
        }
        Ok(())
    }

    /// Drain the queue of in-flight asynchronous kicks accumulated by
    /// the most recent `dispatch` / `on_effect_completed` /
    /// `on_effect_failed` calls.
    pub fn take_pending_kicks(&mut self) -> Vec<AsyncKick> {
        std::mem::take(&mut self.pending_kicks)
    }

    fn run_batch(&mut self, batch: app::EffectBatch, ctx: &mut DeviceContext<'_>) -> Result<()> {
        let mut runner = EffectRunner {
            ctx,
            last_clock: self.last_clock,
        };
        // EffectId allocation: stable, position-based within the batch so
        // completion/failure events correlate precisely (P1 #5). The
        // `batch.failure_policy` decides whether to keep going after a
        // failure: `AbortBatch` stops the rest of the batch on the first
        // failure; `Continue` runs every effect regardless.
        for (idx, effect) in batch.effects.iter().cloned().enumerate() {
            let effect_id = EffectId(idx as u64 + 1);
            let outcome = runner.run(&effect);
            // End the runner's borrow before re-borrowing `self` / `ctx`
            // for the recursive `run_batch` call below. We shadow the
            // variable (rather than `drop(runner)`) because
            // `EffectRunner` has no `Drop` impl - clippy flags
            // `std::mem::drop` on non-`Drop` types as a no-op.
            let _ = runner;
            match outcome {
                Ok(EffectOutcome::Completed(output)) => {
                    let completion = EffectCompletion {
                        batch_id: batch.id,
                        effect_id,
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        output,
                    };
                    let chained = app::update(&mut self.state, Event::EffectCompleted(completion));
                    for next in chained {
                        self.run_batch(next, ctx)?;
                    }
                }
                Ok(EffectOutcome::Async) => self.pending_kicks.push(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                }),
                Err((category, msg)) => {
                    let failure = EffectFailure {
                        batch_id: batch.id,
                        effect_id,
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        error: err_for_category(category, &msg),
                    };
                    let chained = app::update(&mut self.state, Event::EffectFailed(failure));
                    for next in chained {
                        self.run_batch(next, ctx)?;
                    }
                    if batch.failure_policy == FailurePolicy::AbortBatch {
                        break;
                    }
                }
            }
            // Re-establish the runner for the next effect in this batch.
            runner = EffectRunner {
                ctx,
                last_clock: self.last_clock,
            };
        }
        Ok(())
    }

    /// Feed an externally-observed completion back into the state machine.
    #[allow(dead_code)]
    pub fn on_effect_completed(
        &mut self,
        completion: EffectCompletion,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<()> {
        let batches = app::update(&mut self.state, Event::EffectCompleted(completion));
        for batch in batches {
            self.run_batch(batch, ctx)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn on_effect_failed(
        &mut self,
        failure: EffectFailure,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<()> {
        let batches = app::update(&mut self.state, Event::EffectFailed(failure));
        for batch in batches {
            self.run_batch(batch, ctx)?;
        }
        Ok(())
    }

    /// Feed a render completion (success or failure) back into the state
    /// machine via the matching in-flight `AsyncKick`. Returns
    /// immediately if the kick carries a non-render effect.
    pub fn feed_render_completion(
        &mut self,
        kick: AsyncKick,
        output: EffectOutput,
        failure: Option<EffectError>,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<()> {
        if !matches!(kick.effect, Effect::Render(_)) {
            return Ok(());
        }
        let chained = match failure {
            Some(err) => {
                let failure = EffectFailure {
                    batch_id: kick.batch_id,
                    effect_id: kick.effect_id,
                    operation_id: kick.operation_id,
                    render_generation: kick.render_generation,
                    error: err,
                };
                app::update(&mut self.state, Event::EffectFailed(failure))
            }
            None => {
                let completion = EffectCompletion {
                    batch_id: kick.batch_id,
                    effect_id: kick.effect_id,
                    operation_id: kick.operation_id,
                    render_generation: kick.render_generation,
                    output,
                };
                app::update(&mut self.state, Event::EffectCompleted(completion))
            }
        };
        for batch in chained {
            self.run_batch(batch, ctx)?;
        }
        Ok(())
    }
}

impl Default for AppRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// A side-effect request the dispatch could not complete synchronously.
#[derive(Clone)]
pub struct AsyncKick {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effect: Effect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectCategory {
    Ack,
    Persist,
    Rtc,
    Render,
    Sync,
    Tone,
    Sleep,
}

enum EffectOutcome {
    Completed(EffectOutput),
    Async,
}

/// Executes one `Effect` against the existing drivers.
struct EffectRunner<'a, 'ctx> {
    ctx: &'a mut DeviceContext<'ctx>,
    last_clock: Option<DateTime>,
}

impl<'a, 'ctx> EffectRunner<'a, 'ctx> {
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
            Effect::ProgramRtcAlarm(regs) => match self.ctx.board.rtc.set_alarm(regs) {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed)),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::DisableRtcAlarm => match self.ctx.board.rtc.clear_alarm() {
                Ok(()) => Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed)),
                Err(err) => Err((EffectCategory::Rtc, format!("{err:#}"))),
            },
            Effect::AcknowledgeRtcAlarm => match self.ctx.board.rtc.ack_alarm() {
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
            Effect::Reply(reply) => {
                // The main loop routes USB/BLE replies through the matching
                // channel; the state machine emits this Reply when an
                // `Event::UsbCommand` / `Event::BleCommand` slot is busy.
                let raw = reply_from_control_reply(reply);
                crate::usb_console::write_reply(&raw, None);
                if let Some(ble) = self.ctx.ble_control.as_ref() {
                    ble.write_reply(&raw, None);
                }
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::Render(req) => match render_home_into(self.ctx, self.last_clock, req) {
                Ok(()) => {
                    // EPD completion arrives asynchronously via the EPD
                    // task. The caller feeds it back through
                    // `AppRunner::on_effect_completed` with the matching
                    // `EffectCompletion { output: RenderDone }` once the
                    // canvas reaches the panel. We return `Async` so the
                    // caller knows to expect that completion.
                    Ok(EffectOutcome::Async)
                }
                Err(err) => Err((EffectCategory::Render, format!("{err:#}"))),
            },
            Effect::StartSync(req) => {
                let result = self.ctx.start_sync(
                    crate::sync_task::OpSource::Internal,
                    req.now,
                    inkwash_logic::protocol::Command::SyncNow,
                );
                match result {
                    Ok(_) => Ok(EffectOutcome::Async),
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

fn err_for_category(category: EffectCategory, msg: &str) -> EffectError {
    match category {
        EffectCategory::Ack => EffectError::Ack(msg.to_string()),
        EffectCategory::Persist => EffectError::Persist(msg.to_string()),
        EffectCategory::Rtc => EffectError::Rtc(msg.to_string()),
        EffectCategory::Render => EffectError::Render(msg.to_string()),
        EffectCategory::Sync => EffectError::Sync(msg.to_string()),
        EffectCategory::Tone => EffectError::Tone(msg.to_string()),
        EffectCategory::Sleep => EffectError::Sleep(msg.to_string()),
    }
}

fn reply_from_control_reply(reply: &ControlReply) -> control::Reply {
    match reply {
        ControlReply::Ok => Reply::Ok,
        ControlReply::Busy => Reply::Busy,
        ControlReply::Pending => Reply::Pending,
        ControlReply::Status {
            wifi_configured,
            server_configured,
            wifi_connected,
            wifi_ssid,
            wifi_has_password,
            server_url,
            server_has_token,
            timezone_offset_minutes,
        } => Reply::Status {
            wifi_configured: *wifi_configured,
            server_configured: *server_configured,
            wifi_connected: *wifi_connected,
            wifi_ssid: wifi_ssid.clone(),
            wifi_has_password: *wifi_has_password,
            server_url: server_url.clone(),
            server_has_token: *server_has_token,
            timezone_offset_minutes: *timezone_offset_minutes,
        },
        ControlReply::Error { message } => Reply::Error {
            message: message.clone(),
        },
    }
}

/// Re-renders Home on the EPD task and submits the refresh. The state
/// machine emits this effect whenever it needs to switch back to Home
/// (after dismiss, after boot, after a sync-driven data change). The
/// full-screen refresh best-effort guarantees the canvas reaches the
/// panel even if the legacy dirty-rect skip logic would otherwise
/// suppress the refresh. The completion arrives via the EPD task's
/// `EpdCompletion`, which the main loop feeds back as
/// `EffectCompleted(RenderDone)` carrying the matching `render_generation`.
fn render_home_into(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    _req: &RenderRequest,
) -> Result<()> {
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
    // Submit the refresh so the canvas reaches the panel. Without
    // this, a boot alarm dismissed back to Home would leave the panel
    // stuck on the ALARM screen until the next minute's clock-region
    // refresh overlaid it (review round 6 P0 #3).
    ctx.board.display.refresh_full_best_effort();
    Ok(())
}
