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

use inkwash_logic::app::{Effect, EffectOutput, RenderIntent, RenderRequest};
use inkwash_logic::runner::{EffectCategory, EffectExecutor, EffectOutcome};

pub use inkwash_logic::runner::AsyncKick;
pub use inkwash_logic::runtime::Runtime as AppRunner;

use crate::alarms::AlarmStore;
use crate::ctx::DeviceContext;
use crate::rtc::DateTime;

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
            Effect::Reply { channel, reply } => {
                // Record the reply for the caller to write after the pump:
                // only the caller knows the correlation id of the frame that
                // triggered it, and the reply must go to exactly one
                // transport (USB and BLE have independent pending slots).
                self.replies.push((*channel, reply.clone()));
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::Render(req) => match render_home_into(self.ctx, self.last_clock, req) {
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

/// Re-renders Home and submits the refresh requested by the state machine.
/// Minute ticks use a partial CLOCK_RECT refresh; alarm dismiss uses a full
/// refresh so the ALARM frame is replaced in one panel operation.
///
/// Returns the EPD request id so the caller can correlate the eventual
/// `EpdCompletion` back to this render's `AsyncKick`.
fn render_home_into(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    req: &RenderRequest,
) -> Result<u64> {
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
    let request_id = match req.intent {
        RenderIntent::Partial => ctx.board.display.refresh_partial(crate::canvas::Rect {
            x: 16,
            y: 36,
            width: 368,
            height: 92,
        }),
        RenderIntent::Full => ctx.board.display.refresh_full(),
    };
    request_id.map_err(|e| anyhow::anyhow!("{e:#}"))
}
