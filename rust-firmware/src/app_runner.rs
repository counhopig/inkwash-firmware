//! Firmware-side `EffectExecutor` for the host-testable runner.
//!
//! The application-state driving engine (batch execution, EffectId
//! allocation, AbortBatch semantics, completion feedback, async-kick
//! collection, render-completion routing) lives in
//! `inkwash_logic::runner` and is host-testable with a fake executor.
//! This module provides the *real* executor: `EffectRunner` runs each
//! `Effect` against `DeviceContext`'s drivers (RTC, NVS stores, audio,
//! display, USB/BLE), plus the render/reply helpers it needs.
//!
//! `AppRunner`, `AsyncKick`, `EffectOutcome`, `EffectCategory` and
//! `err_for_category` are re-exported from `inkwash_logic::runner` so
//! the firmware call sites (`main.rs`) are unchanged in spelling.

use anyhow::Result;

use inkwash_logic::app::{Effect, EffectOutput, RenderRequest};
use inkwash_logic::protocol::ControlReply;
use inkwash_logic::runner::{EffectCategory, EffectExecutor, EffectOutcome};

pub use inkwash_logic::runner::{AppRunner, AsyncKick};

use crate::alarms::AlarmStore;
use crate::control::{self, Reply};
use crate::ctx::DeviceContext;
use crate::rtc::DateTime;

/// Executes one `Effect` against the existing drivers, reporting the
/// outcome back to `inkwash_logic::runner::AppRunner`.
pub struct EffectRunner<'a, 'ctx> {
    ctx: &'a mut DeviceContext<'ctx>,
    last_clock: Option<DateTime>,
}

impl<'a, 'ctx> EffectRunner<'a, 'ctx> {
    pub fn new(ctx: &'a mut DeviceContext<'ctx>, last_clock: Option<DateTime>) -> Self {
        Self { ctx, last_clock }
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

/// Re-renders Home on the EPD task and submits a partial CLOCK_RECT
/// refresh. The state machine emits this effect on every minute Tick
/// (the clock displayed a new minute) and on screen transitions back to
/// Home. Minute ticks must NOT become a full-screen refresh (review
/// round 7 P1 #5), and the partial refresh is cheap. Screen transitions
/// (e.g. alarm dismiss returning to Home) are handled by the main loop
/// pushing FULL_SCREEN_RECT into its dirty path - that is the deliberate
/// full refresh; this function only repaints the clock region.
///
/// Returns the EPD request id so the caller can correlate the eventual
/// `EpdCompletion` back to this render's `AsyncKick`.
fn render_home_into(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
    _req: &RenderRequest,
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
    // Partial CLOCK_RECT refresh only (not full): a minute Tick must not
    // degrade into a full-screen refresh. The returned id is threaded
    // back so the eventual EpdCompletion correlates to this render.
    ctx.board
        .display
        .refresh_partial(crate::canvas::Rect {
            x: 16,
            y: 36,
            width: 368,
            height: 92,
        })
        .map_err(|e| anyhow::anyhow!("{e:#}"))
}
