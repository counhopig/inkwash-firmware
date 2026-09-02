//! Host fake harness for the generic `runner::AppRunner`.
//!
//! `runner::AppRunner` executes effect batches through a caller-provided
//! `EffectExecutor`. The firmware provides the real executor
//! (`rust-firmware::app_runner::EffectRunner`) that drives
//! `DeviceContext`; this module provides a recording fake so the full
//! alarm lifecycle - boot, ring, dismiss, rearm, retries, render
//! completions - can be exercised on the host without ESP-IDF.
//!
//! The fake records every effect invocation (counted per effect and per
//! category), executes synchronous effects immediately with the output
//! the state machine expects, and can be scripted to fail a category the
//! first N times (e.g. an RTC I2C glitch) to exercise the retry paths.
//! Render effects return `AsyncWithId` with a caller-controlled request
//! id, so the harness can drive EPD completions (including stale ones)
//! through `AppRunner::feed_render_completion`.

use std::collections::BTreeMap;

use crate::app::{AppState, Effect, EffectError, EffectOutput, Event, Screen};
use crate::background_outcome::BackgroundOutcome;
use crate::runner::{AppRunner, AsyncKick, EffectCategory, EffectExecutor, EffectOutcome};

/// Counts how many times each effect variant ran. Keyed by a stable
/// string label so the harness can assert "ACK ran exactly once".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectLog {
    pub by_name: BTreeMap<String, usize>,
    pub by_category: BTreeMap<EffectCategory, usize>,
}

impl EffectLog {
    pub fn count(&self, name: &str) -> usize {
        self.by_name.get(name).copied().unwrap_or(0)
    }
    pub fn category(&self, cat: EffectCategory) -> usize {
        self.by_category.get(&cat).copied().unwrap_or(0)
    }
    pub fn total(&self) -> usize {
        self.by_name.values().sum()
    }
}

/// Scripted failure: fail the next `n` invocations in a category with a
/// synthetic message, then succeed. Simulates an I2C/NVS transient.
#[derive(Clone, Debug, Default)]
pub struct ScriptedFailures {
    pub per_category: BTreeMap<EffectCategory, usize>,
}

impl ScriptedFailures {
    pub fn fail_next(&mut self, cat: EffectCategory, n: usize) -> &mut Self {
        self.per_category.insert(cat, n);
        self
    }
    /// Consume one failure budget for `cat`; true means the next call
    /// should fail.
    fn take(&mut self, cat: EffectCategory) -> bool {
        match self.per_category.get_mut(&cat) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }
}

/// A recording fake `EffectExecutor`.
#[derive(Debug)]
pub struct FakeExecutor {
    pub log: EffectLog,
    pub failures: ScriptedFailures,
    /// Next request id handed to `Effect::Render` (the EPD would echo it).
    pub next_request_id: u64,
    /// Renders issued (request_id, generation) in order.
    pub renders: Vec<(u64, Option<crate::app::RenderGeneration>)>,
    /// When true, `Effect::Render` fails with `EffectError::Render`.
    pub fail_render: bool,
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self {
            log: EffectLog::default(),
            failures: ScriptedFailures::default(),
            next_request_id: 1,
            renders: Vec::new(),
            fail_render: false,
        }
    }
}

impl FakeExecutor {
    fn record(&mut self, effect: &Effect, cat: EffectCategory) {
        *self
            .log
            .by_name
            .entry(effect_name(effect).to_string())
            .or_insert(0) += 1;
        *self.log.by_category.entry(cat).or_insert(0) += 1;
    }
}

/// Stable label for `Effect` variants (used for count assertions).
pub fn effect_name(effect: &Effect) -> &'static str {
    match effect {
        Effect::PersistAlarms(_) => "PersistAlarms",
        Effect::PersistTodos(_) => "PersistTodos",
        Effect::PersistInbox(_) => "PersistInbox",
        Effect::PersistConfig(_) => "PersistConfig",
        Effect::PersistSyncMetadata(_) => "PersistSyncMetadata",
        Effect::ProgramRtcAlarm(_) => "ProgramRtcAlarm",
        Effect::DisableRtcAlarm => "DisableRtcAlarm",
        Effect::AcknowledgeRtcAlarm => "AcknowledgeRtcAlarm",
        Effect::StartTone => "StartTone",
        Effect::StopTone => "StopTone",
        Effect::Reply(_) => "Reply",
        Effect::Render(_) => "Render",
        Effect::StartSync(_) => "StartSync",
        Effect::StartBlePairing(_) => "StartBlePairing",
        Effect::StopBlePairing => "StopBlePairing",
        Effect::EnterLightSleep(_) => "EnterLightSleep",
        Effect::EnterDeepSleep(_) => "EnterDeepSleep",
    }
}

impl EffectExecutor for FakeExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
        let cat = match effect {
            Effect::PersistAlarms(_)
            | Effect::PersistTodos(_)
            | Effect::PersistInbox(_)
            | Effect::PersistConfig(_)
            | Effect::PersistSyncMetadata(_) => EffectCategory::Persist,
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm => EffectCategory::Rtc,
            Effect::AcknowledgeRtcAlarm => EffectCategory::Ack,
            Effect::StartTone | Effect::StopTone => EffectCategory::Tone,
            Effect::Reply(_) => EffectCategory::Ack,
            Effect::Render(_) => EffectCategory::Render,
            Effect::StartSync(_) => EffectCategory::Sync,
            Effect::StartBlePairing(_) | Effect::StopBlePairing => EffectCategory::Sync,
            Effect::EnterLightSleep(_) | Effect::EnterDeepSleep(_) => EffectCategory::Sleep,
        };
        self.record(effect, cat);
        if self.failures.take(cat) {
            return Err((cat, format!("scripted {} failure", effect_name(effect))));
        }
        match effect {
            Effect::PersistAlarms(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Alarms,
            ))),
            Effect::PersistTodos(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Todos,
            ))),
            Effect::PersistInbox(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Inbox,
            ))),
            Effect::PersistConfig(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Config,
            ))),
            Effect::PersistSyncMetadata(_) => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::SyncMetadata),
            )),
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm => {
                Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
            }
            Effect::AcknowledgeRtcAlarm => Ok(EffectOutcome::Completed(EffectOutput::AckDone)),
            Effect::StartTone | Effect::StopTone => {
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            Effect::Reply(_) => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::Render(req) => {
                if self.fail_render {
                    return Err((EffectCategory::Render, "render failed".into()));
                }
                let id = self.next_request_id;
                self.next_request_id += 1;
                self.renders.push((id, Some(req.generation)));
                Ok(EffectOutcome::AsyncWithId(id))
            }
            Effect::StartSync(_) | Effect::StartBlePairing(_) | Effect::StopBlePairing => {
                Ok(EffectOutcome::Async)
            }
            Effect::EnterLightSleep(_) => {
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::EnterDeepSleep(_) => Ok(EffectOutcome::Async),
        }
    }
}

/// Convenience driver combining `AppRunner` + `FakeExecutor`, mirroring
/// the firmware main loop's use: dispatch events, drain render kicks,
/// and feed EPD completions (including stale ones) back.
#[derive(Debug, Default)]
pub struct Harness {
    pub runner: AppRunner,
    pub executor: FakeExecutor,
    /// In-flight render kicks (request_id -> kick), matching the
    /// firmware `pending_renders` registry.
    pub pending_renders: Vec<AsyncKick>,
}

impl Harness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dispatch(&mut self, event: Event) -> Result<(), String> {
        self.runner.dispatch(event, &mut self.executor)?;
        self.drain_kicks();
        Ok(())
    }

    /// Move newly-issued render kicks into `pending_renders`.
    pub fn drain_kicks(&mut self) {
        for kick in self.runner.take_pending_kicks() {
            if kick.is_render() {
                self.pending_renders.push(kick);
            }
        }
    }

    /// Feed an EPD completion for `request_id` back into the runner.
    /// `ok=false` produces `EffectFailed(Render)`. Returns true when a
    /// matching kick existed and was consumed.
    pub fn complete_render(&mut self, request_id: u64, ok: bool) -> Result<bool, String> {
        let Some(idx) = self
            .pending_renders
            .iter()
            .position(|k| k.request_id == Some(request_id))
        else {
            // A completion with no matching kick (stale / superseded) is
            // observed but not fed back - mirrors firmware behaviour.
            return Ok(false);
        };
        let kick = self.pending_renders.remove(idx);
        let failure = if ok {
            None
        } else {
            Some(EffectError::Render("epd failed".into()))
        };
        self.runner.feed_render_completion(
            kick,
            EffectOutput::RenderDone,
            failure,
            &mut self.executor,
        )?;
        self.drain_kicks();
        Ok(true)
    }

    pub fn screen(&self) -> &Screen {
        &self.runner.state().screen
    }

    pub fn state(&self) -> &AppState {
        self.runner.state()
    }

    /// Convenience: is an alarm currently ringing?
    pub fn ringing(&self) -> bool {
        matches!(self.screen(), Screen::AlarmRinging)
    }

    /// A background-poll merge helper exposed for completeness: the
    /// harness can assert the stable priority rule.
    pub fn merge(a: BackgroundOutcome, b: BackgroundOutcome) -> BackgroundOutcome {
        a.merge(b)
    }
}

/// Test helpers for building snapshots on the host.
pub mod helpers {
    use crate::app::{BootSnapshot, RtcAlarmSnapshot};
    use crate::datetime::DateTime;
    use crate::device_config::DeviceConfig;
    use crate::wake_cause::WakeCause;

    pub fn dt(hour: u8, minute: u8) -> DateTime {
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

    pub fn boot(
        now: Option<DateTime>,
        af: bool,
        aie: bool,
        alarms: Vec<crate::alarm_schedule::StoredAlarm>,
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
        }
    }

    pub fn alarm(id: u8, hour: u8, minute: u8) -> crate::alarm_schedule::StoredAlarm {
        crate::alarm_schedule::StoredAlarm {
            id,
            hour,
            minute,
            repeat: crate::alarm_schedule::Repeat::Daily,
            enabled: true,
            label: String::new(),
        }
    }

    pub fn snapshot(now: DateTime, af: bool, aie: bool) -> RtcAlarmSnapshot {
        RtcAlarmSnapshot {
            now,
            alarm_flag: af,
            alarm_interrupt_enabled: aie,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AlarmRuntimeState, Event, Screen};
    use crate::button_event::ButtonEvent as Btn;
    use crate::datetime::DateTime;
    use crate::runner::EffectCategory;
    use helpers::{alarm, boot, dt, snapshot};

    // ---- verification point 1: boot alarm rings then dismisses -----------

    #[test]
    fn boot_alarm_rings_and_dismisses() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(h.ringing(), "boot with AF+AIE+due alarm must ring");
        // ACK, persist, tone, render all issued on ring.
        assert_eq!(h.executor.log.count("AcknowledgeRtcAlarm"), 1);
        assert_eq!(h.executor.log.count("PersistAlarms"), 1);
        assert_eq!(h.executor.log.count("StartTone"), 1);
        assert_eq!(h.executor.log.count("Render"), 1);

        h.dispatch(Event::Button(Btn::Pressed)).unwrap();
        assert!(!h.ringing(), "dismiss leaves ringing screen");
        assert_eq!(h.screen(), &Screen::Home, "dismiss restores Home");
        assert_eq!(h.executor.log.count("StopTone"), 1);
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
    }

    // ---- verification point 2: runtime alarm preempts everywhere ---------

    #[test]
    fn runtime_alarm_rings_from_home() {
        let mut h = Harness::new();
        // Boot clean (no AF), alarm armed for 9:00.
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(!h.ringing());
        // AF asserts at 9:00 -> rings.
        h.dispatch(Event::RtcAlarmSnapshotReady(snapshot(dt(9, 0), true, true)))
            .unwrap();
        assert!(h.ringing(), "runtime AF must ring");
    }

    #[test]
    fn background_outcome_merge_priorities_alarm_first() {
        // The blocking-page/reminder route reduces to BackgroundOutcome;
        // alarm must win over every other source.
        use crate::background_outcome::BackgroundOutcome as B;
        assert_eq!(B::AlarmHandled.merge(B::NoChange), B::AlarmHandled);
        assert_eq!(B::NoChange.merge(B::AlarmHandled), B::AlarmHandled);
        assert_eq!(B::AlarmHandled.merge(B::VisibleChanged), B::AlarmHandled);
        assert_eq!(B::VisibleChanged.merge(B::AlarmHandled), B::AlarmHandled);
        assert_eq!(B::VisibleChanged.merge(B::NoChange), B::VisibleChanged);
        assert_eq!(B::NoChange.merge(B::NoChange), B::NoChange);
    }

    // ---- verification point 3: two consecutive alarms --------------------

    #[test]
    fn two_consecutive_alarms_each_ring_once() {
        let mut h = Harness::new();
        // First alarm at 9:00.
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(h.ringing());
        // Duplicate snapshot while firing is ignored (idempotent).
        h.dispatch(Event::RtcAlarmSnapshotReady(snapshot(dt(9, 0), true, true)))
            .unwrap();
        assert_eq!(
            h.executor.log.count("AcknowledgeRtcAlarm"),
            1,
            "no double ACK"
        );

        // Dismiss. The fake auto-completed ACK + persist during boot, so
        // both commits are already Succeeded; only the minute advance is
        // needed for rearm.
        h.dispatch(Event::Button(Btn::Pressed)).unwrap();
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        h.dispatch(Event::Tick(dt(9, 1))).unwrap();
        assert_eq!(h.executor.log.count("ProgramRtcAlarm"), 1);

        // Second alarm at next day's 9:00 (Daily): AF again.
        let now2 = DateTime {
            year: 2026,
            month: 9,
            day: 1,
            weekday: 2,
            hour: 9,
            minute: 0,
            second: 0,
            voltage_low: false,
        };
        h.dispatch(Event::RtcAlarmSnapshotReady(snapshot(now2, true, true)))
            .unwrap();
        assert!(h.ringing(), "second alarm must ring again");
        assert_eq!(h.executor.log.count("AcknowledgeRtcAlarm"), 2);
        assert_eq!(h.executor.log.count("PersistAlarms"), 2);
    }

    // ---- verification point 4: transient RTC failures retry -------------

    #[test]
    fn rtc_ack_failure_retries_on_tick() {
        let mut h = Harness::new();
        h.executor.failures.fail_next(EffectCategory::Ack, 1);
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        // The ACK failed once; the state machine must have scheduled a
        // retry rather than leaving ACK stuck failed.
        assert!(
            h.ringing(),
            "still firing (commit failed but not abandoned)"
        );
        // Cross-minute tick re-issues the failed ACK.
        h.dispatch(Event::Tick(dt(9, 1))).unwrap();
        let ack_count = h.executor.log.count("AcknowledgeRtcAlarm");
        assert!(
            ack_count >= 2,
            "ACK retried after transient failure, got {ack_count}"
        );
    }

    #[test]
    fn rtc_program_failure_degrades_then_retries() {
        let mut h = Harness::new();
        // Boot clean, then program the alarm at 9:00; make the first
        // ProgramRtcAlarm fail.
        h.executor.failures.fail_next(EffectCategory::Rtc, 1);
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        // Program failed -> Degraded, retry scheduled.
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::Degraded { .. }
        ));
        // Next minute tick retries the program.
        h.dispatch(Event::Tick(dt(9, 0))).unwrap();
        let program_count = h.executor.log.count("ProgramRtcAlarm");
        assert!(
            program_count >= 2,
            "program retried after transient failure, got {program_count}"
        );
    }

    // ---- verification point 5: dismiss restores the correct page --------

    #[test]
    fn dismiss_restores_home() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(h.ringing());
        h.dispatch(Event::Button(Btn::Pressed)).unwrap();
        assert_eq!(h.screen(), &Screen::Home);
    }

    // ---- verification point 6: reminder exit -> page redraw -------------

    #[test]
    fn reminder_dismiss_survives_merge_as_visible() {
        use crate::background_outcome::BackgroundOutcome as B;
        // A reminder was shown and dismissed (VisibleChanged); no sync ran
        // (NoChange). The merged page outcome must stay VisibleChanged so
        // the page redraws.
        assert_eq!(B::NoChange.merge(B::VisibleChanged), B::VisibleChanged);
        // urgent+todo both dismiss -> VisibleChanged.
        assert_eq!(
            B::VisibleChanged.merge(B::VisibleChanged),
            B::VisibleChanged
        );
    }

    // ---- verification point 7: render kick <-> EPD completion loop ------

    #[test]
    fn render_completion_closes_loop_and_ignores_stale() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        // One render kick is pending with the fake's request id.
        assert_eq!(h.pending_renders.len(), 1);
        let rid = h.pending_renders[0].request_id.expect("render kick has id");

        // A stale/unknown completion (no matching kick) is ignored.
        assert!(!h.complete_render(rid + 999, true).unwrap());

        // The matching completion closes the loop.
        assert!(h.complete_render(rid, true).unwrap());
        assert!(h.pending_renders.is_empty(), "kick consumed");
        // And a duplicate completion for the same id is now stale.
        assert!(!h.complete_render(rid, true).unwrap());
    }

    #[test]
    fn stale_render_completion_is_ignored() {
        let mut h = Harness::new();
        // No renders at all: any completion is stale.
        assert!(!h.complete_render(7, true).unwrap());
    }

    // ---- verification point 8: side effects exactly once ----------------

    #[test]
    fn lifecycle_side_effects_each_once() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        h.dispatch(Event::Button(Btn::Pressed)).unwrap();
        // Commits were auto-completed by the fake during boot; a minute
        // tick performs the rearm.
        h.dispatch(Event::Tick(dt(9, 1))).unwrap();
        // Each key side effect fired exactly once across the whole
        // lifecycle.
        assert_eq!(h.executor.log.count("AcknowledgeRtcAlarm"), 1);
        assert_eq!(h.executor.log.count("PersistAlarms"), 1);
        assert_eq!(h.executor.log.count("StopTone"), 1);
        assert_eq!(h.executor.log.count("ProgramRtcAlarm"), 1);
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::Armed { alarm_id: 1 }
        ));
    }
}
