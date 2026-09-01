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
//! Coexistence with `ctx::DeviceContext`: the runner borrows the same
//! drivers, stores, and audio/display/EPD subsystems `DeviceContext` already
//! exposes. While step 2 is being wired in, `DeviceContext` still owns
//! scheduling decisions (sync cadence, alarm re-arm); the runner owns
//! state-machine transitions for the flows the architecture marks as "first
//! to migrate" (boot/home/RTC alarm/sleep). Both share the same underlying
//! hardware through the existing module boundaries.

use anyhow::Result;

use inkwash_logic::app::{
    self, AppState, Effect, EffectBatchId, EffectCompletion, EffectError, EffectFailure, EffectId,
    EffectOutput, Event, OperationId, RenderGeneration, RenderRequest,
};
use inkwash_logic::protocol::ControlReply;

use crate::alarms::AlarmStore;
use crate::control::{self, Reply};
use crate::ctx::DeviceContext;
use crate::rtc::DateTime;

/// Owns the application's `AppState` and drives every event through the
/// pure transition function. The runner is constructed once at boot, given
/// the facts that make up a `BootSnapshot`, and accepts further events via
/// [`AppRunner::dispatch`].
pub struct AppRunner {
    state: AppState,
    /// Most recent RTC read - threaded into `Effect::Render` so the home
    /// screen does not need a fresh I2C transaction for each refresh. The
    /// legacy main loop reads the RTC on the same 1.2 s / 10 s cadence, so
    /// the runner simply observes the timestamp the main loop hands it
    /// rather than owning its own RTC reads.
    last_clock: Option<DateTime>,
}

#[allow(dead_code)]
impl AppRunner {
    /// Build a runner with an empty `AppState`. Boot facts are pushed in
    /// via [`AppRunner::dispatch`] once the caller has collected them - the
    /// runner does not own I2C reads.
    pub fn new() -> Self {
        Self {
            state: AppState::default(),
            last_clock: None,
        }
    }

    /// Build a runner from a pre-populated `AppState` (used when boot
    /// facts have already been merged into the state directly by the legacy
    /// boot path).
    pub fn from_state(state: AppState) -> Self {
        Self {
            state,
            last_clock: None,
        }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn set_last_clock(&mut self, clock: Option<DateTime>) {
        self.last_clock = clock;
    }

    /// Dispatch a single event through the state machine and run every
    /// returned effect batch against the existing drivers. Synchronous
    /// completions are fed back into the state machine in the same call;
    /// asynchronous effects (renders, sync, BLE pairing, sleep) are
    /// returned to the caller as [`AsyncKick`]s. The caller is expected to
    /// keep the corresponding subsystem handle alive and, when the
    /// eventual completion arrives, build an [`EffectCompletion`] /
    /// [`EffectFailure`] and feed it via [`AppRunner::on_effect_completed`]
    /// or [`AppRunner::on_effect_failed`].
    pub fn dispatch(
        &mut self,
        event: Event,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<Vec<AsyncKick>> {
        let batches = app::update(&mut self.state, event);
        let mut kicks = Vec::new();
        for batch in batches {
            kicks.extend(self.run_batch(batch, ctx)?);
        }
        Ok(kicks)
    }

    fn run_batch(
        &mut self,
        batch: app::EffectBatch,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<Vec<AsyncKick>> {
        let mut kicks = Vec::new();
        let mut runner = EffectRunner {
            ctx,
            last_clock: self.last_clock,
        };
        // Drive every effects in this batch. For each we either:
        // - synchronous outcome -> feed the matching completion into the
        //   state machine, which may return further batches. We invoke
        //   `run_batch` recursively; recursion depth is bounded by the
        //   chain length the state machine produces (one or two in
        //   practice).
        // - asynchronous outcome -> hand the effect back to the caller as
        //   an `AsyncKick` so the matching subsystem (EPD task, sync
        //   task, BLE pairing screen, ...) can drive it to completion.
        // - failure -> feed an `EffectFailed` into the state machine and
        //   recurse on whatever it returns.
        for effect in batch.effects {
            let outcome = runner.run(&effect);
            // End the runner's borrow before re-borrowing `self` /
            // `ctx` for the recursive `run_batch` call below. We
            // shadow the variable (rather than `drop(runner)`) because
            // `EffectRunner` has no `Drop` impl - clippy flags
            // `std::mem::drop` on non-`Drop` types as a no-op.
            let _ = runner;
            match outcome {
                Ok(EffectOutcome::Completed(output)) => {
                    let completion = EffectCompletion {
                        batch_id: batch.id,
                        effect_id: EffectId(0),
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        output,
                    };
                    let chained = app::update(&mut self.state, Event::EffectCompleted(completion));
                    for next in chained {
                        kicks.extend(self.run_batch(next, ctx)?);
                    }
                }
                Ok(EffectOutcome::Async) => kicks.push(AsyncKick {
                    batch_id: batch.id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                }),
                Err(err) => {
                    let failure = EffectFailure {
                        batch_id: batch.id,
                        effect_id: EffectId(0),
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        error: map_err(&err),
                    };
                    let chained = app::update(&mut self.state, Event::EffectFailed(failure));
                    for next in chained {
                        kicks.extend(self.run_batch(next, ctx)?);
                    }
                }
            }
            // Re-establish the runner for the next effect in this batch.
            runner = EffectRunner {
                ctx,
                last_clock: self.last_clock,
            };
        }
        Ok(kicks)
    }
    pub fn on_effect_completed(
        &mut self,
        completion: EffectCompletion,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<Vec<AsyncKick>> {
        let batches = app::update(&mut self.state, Event::EffectCompleted(completion));
        let mut kicks = Vec::new();
        for batch in batches {
            kicks.extend(self.run_batch(batch, ctx)?);
        }
        Ok(kicks)
    }

    pub fn on_effect_failed(
        &mut self,
        failure: EffectFailure,
        ctx: &mut DeviceContext<'_>,
    ) -> Result<Vec<AsyncKick>> {
        let batches = app::update(&mut self.state, Event::EffectFailed(failure));
        let mut kicks = Vec::new();
        for batch in batches {
            kicks.extend(self.run_batch(batch, ctx)?);
        }
        Ok(kicks)
    }
}

impl Default for AppRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// A side-effect request the dispatch could not complete synchronously:
/// either an `Effect::Render` (waiting for the EPD task), or a sync/ble/
/// sleep that is dispatched onto another task. The caller is expected to
/// keep the corresponding subsystem handle alive and, when the eventual
/// completion arrives, build an [`EffectCompletion`] /
/// [`EffectFailure`] and feed it back to `AppRunner`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AsyncKick {
    pub batch_id: EffectBatchId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effect: Effect,
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
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome> {
        match effect {
            // ---- synchronous effects ---------------------------------------
            Effect::PersistAlarms(list) => {
                AlarmStore::save(self.ctx.alarm_store, list)?;
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Alarms,
                )))
            }
            Effect::PersistTodos(list) => {
                crate::todos::TodoStore::save(self.ctx.todo_store, list)?;
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Todos,
                )))
            }
            Effect::PersistInbox(list) => {
                crate::inbox::InboxStore::save(self.ctx.inbox_store, list)?;
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Inbox,
                )))
            }
            Effect::PersistConfig(cfg) => {
                self.ctx.counters.save_device_config(cfg)?;
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::Config,
                )))
            }
            Effect::PersistSyncMetadata(meta) => {
                if let Some(etag) = meta.etag.as_deref() {
                    self.ctx.counters.save_sync_etag(etag)?;
                }
                if let Some(epoch) = meta.last_sync_epoch {
                    self.ctx.counters.set_last_sync_epoch(epoch)?;
                }
                if let Some(epoch) = meta.rtc_align_epoch {
                    self.ctx.counters.set_rtc_align_epoch(epoch)?;
                }
                Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                    inkwash_logic::app::PersistTarget::SyncMetadata,
                )))
            }
            Effect::ProgramRtcAlarm(regs) => {
                self.ctx.board.rtc.set_alarm(regs)?;
                Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
            }
            Effect::DisableRtcAlarm => {
                self.ctx.board.rtc.clear_alarm()?;
                Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
            }
            Effect::AcknowledgeRtcAlarm => {
                self.ctx.board.rtc.ack_alarm()?;
                Ok(EffectOutcome::Completed(EffectOutput::AckDone))
            }
            Effect::StartTone => {
                if let Some(audio) = self.ctx.board.audio.as_mut() {
                    audio.play_sine_stereo(880.0, 0.05, 8000)?;
                }
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            Effect::StopTone => {
                if let Some(audio) = self.ctx.board.audio.as_mut() {
                    audio.set_mute(true)?;
                }
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            // ---- asynchronous / out-of-band effects -------------------------
            Effect::Reply(reply) => {
                // The legacy `control::dispatch` paths route USB/BLE replies
                // through the matching channel. The state machine does not
                // know which channel an `Event::UsbCommand` /
                // `Event::BleCommand` arrived on; the main loop carries that
                // fact separately and runs the legacy `control::dispatch` for
                // now. This branch is exercised when the state machine emits
                // `Reply::Busy` for an in-flight command (see
                // `transition_command`) - we mirror the reply onto both
                // transports; whichever does not have a matching slot drops
                // the write.
                let raw = reply_from_control_reply(reply);
                crate::usb_console::write_reply(&raw, None);
                if let Some(ble) = self.ctx.ble_control.as_ref() {
                    ble.write_reply(&raw, None);
                }
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::Render(req) => {
                render_home_into(self.ctx, self.last_clock, req)?;
                // The actual completion arrives via the EPD task; tell the
                // caller to expect one. The legacy EPD task's `EpdCompletion`
                // maps 1:1 to `EffectOutput::RenderDone` - the rendering
                // itself succeeded if the canvas reached the panel; failures
                // surface as `EffectFailed` with `EffectError::Render(_)` from
                // `EpdCompletion.ok = false`.
                Ok(EffectOutcome::Async)
            }
            Effect::StartSync(req) => {
                self.ctx.start_sync(
                    crate::sync_task::OpSource::Internal,
                    req.now,
                    inkwash_logic::protocol::Command::SyncNow,
                )?;
                Ok(EffectOutcome::Async)
            }
            Effect::StartBlePairing(_req) => Ok(EffectOutcome::Async),
            Effect::StopBlePairing => Ok(EffectOutcome::Async),
            Effect::EnterLightSleep(plan) => {
                crate::power::configure_light_sleep()?;
                let _ = plan;
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::EnterDeepSleep(plan) => {
                crate::power::enter_deep_sleep_with_wakeups(plan.maintenance);
            }
        }
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

/// Re-renders Home on the EPD task, mirroring `main::render_home_now` but
/// driven by the state machine. The fingerprint / dirty-region logic
/// stays in the main loop for now - the runner only does the unconditional
/// paint the state machine asks for.
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
    Ok(())
}

fn map_err(err: &anyhow::Error) -> EffectError {
    let msg = format!("{err:#}");
    EffectError::Ack(msg)
}
