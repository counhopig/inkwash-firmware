//! Host fake harness for the `runtime::Runtime` single-consumer loop.
//!
//! `Runtime` executes effect batches through a caller-provided
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
//! through the render-registry feed.

use std::collections::BTreeMap;

use crate::alarm_flow::{AlarmHost, AlarmPoll};
use crate::app::{
    AppState, Effect, EffectError, EffectOutput, Event, RenderIntent, RenderView, Screen,
};
use crate::background_outcome::BackgroundOutcome;
use crate::epd_registry::{FeedOutcome, RenderRegistry, RenderTerminal};
use crate::runner::{EffectCategory, EffectExecutor, EffectOutcome};
use crate::runtime::Runtime;

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
    /// Refresh intents issued in the same order as `renders`.
    pub render_intents: Vec<RenderIntent>,
    /// Render surfaces issued in the same order as `renders`.
    pub render_views: Vec<RenderView>,
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
            render_intents: Vec::new(),
            render_views: Vec::new(),
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
        Effect::ApplySyncedData(_) => "ApplySyncedData",
        Effect::ClearSyncEtag => "ClearSyncEtag",
        Effect::PersistTimezone(_) => "PersistTimezone",
        Effect::WriteRtcTime(_) => "WriteRtcTime",
        Effect::ProgramRtcAlarm(_) => "ProgramRtcAlarm",
        Effect::DisableRtcAlarm => "DisableRtcAlarm",
        Effect::AcknowledgeRtcAlarm => "AcknowledgeRtcAlarm",
        Effect::StartTone => "StartTone",
        Effect::StopTone => "StopTone",
        Effect::Reply { .. } => "Reply",
        Effect::Render(_) => "Render",
        Effect::StartSync(_) => "StartSync",
        Effect::StartSetWifi(_) => "StartSetWifi",
        Effect::StartBlePairing(_) => "StartBlePairing",
        Effect::OpenNavigationDestination { .. } => "OpenNavigationDestination",
        Effect::OpenSettingsItem { .. } => "OpenSettingsItem",
        Effect::PersistAlarmToggle { .. } => "PersistAlarmToggle",
        Effect::PersistTodoEdit { .. } => "PersistTodoEdit",
        Effect::OpenAddAlarm => "OpenAddAlarm",
        Effect::OpenInboxItem { .. } => "OpenInboxItem",
        Effect::OpenCalendarDay { .. } => "OpenCalendarDay",
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
            | Effect::PersistSyncMetadata(_)
            | Effect::ApplySyncedData(_)
            | Effect::ClearSyncEtag
            | Effect::PersistTimezone(_) => EffectCategory::Persist,
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm | Effect::WriteRtcTime(_) => {
                EffectCategory::Rtc
            }
            Effect::AcknowledgeRtcAlarm => EffectCategory::Ack,
            Effect::StartTone | Effect::StopTone => EffectCategory::Tone,
            Effect::Reply { .. } => EffectCategory::Ack,
            Effect::Render(_) => EffectCategory::Render,
            Effect::StartSync(_) | Effect::StartSetWifi(_) => EffectCategory::Sync,
            Effect::StartBlePairing(_) | Effect::StopBlePairing => EffectCategory::Sync,
            Effect::OpenNavigationDestination { .. }
            | Effect::OpenAddAlarm
            | Effect::OpenInboxItem { .. }
            | Effect::OpenCalendarDay { .. } => EffectCategory::Render,
            Effect::OpenSettingsItem { .. } => EffectCategory::Render,
            Effect::PersistAlarmToggle { .. } | Effect::PersistTodoEdit { .. } => {
                EffectCategory::Persist
            }
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
            Effect::ClearSyncEtag => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::SyncMetadata,
            ))),
            Effect::ApplySyncedData(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::SyncApply,
            ))),
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm => {
                Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
            }
            Effect::WriteRtcTime(_) => Ok(EffectOutcome::Completed(EffectOutput::RtcTimeWritten)),
            Effect::PersistTimezone(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Timezone,
            ))),
            Effect::AcknowledgeRtcAlarm => Ok(EffectOutcome::Completed(EffectOutput::AckDone)),
            Effect::StartTone | Effect::StopTone => {
                Ok(EffectOutcome::Completed(EffectOutput::ToneDone))
            }
            Effect::Reply { .. } => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::Render(req) => {
                if self.fail_render {
                    return Err((EffectCategory::Render, "render failed".into()));
                }
                let id = self.next_request_id;
                self.next_request_id += 1;
                self.renders.push((id, Some(req.generation)));
                self.render_intents.push(req.intent);
                self.render_views.push(req.view);
                Ok(EffectOutcome::AsyncWithId(id))
            }
            Effect::StartSync(_)
            | Effect::StartSetWifi(_)
            | Effect::StartBlePairing(_)
            | Effect::StopBlePairing => Ok(EffectOutcome::Async),
            Effect::OpenNavigationDestination { .. } | Effect::OpenSettingsItem { .. } => {
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::PersistAlarmToggle { .. } => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::Alarms),
            )),
            Effect::PersistTodoEdit { .. } => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::Todos),
            )),
            Effect::OpenAddAlarm => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::OpenInboxItem { .. } => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::OpenCalendarDay { .. } => {
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::EnterLightSleep(_) => {
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::EnterDeepSleep(_) => Ok(EffectOutcome::Async),
        }
    }
}

/// Fake `AlarmHost` driving the shared `AlarmPoll` orchestration: a
/// scripted AF edge + snapshot, dispatching through the `Harness`'s
/// runner. A host test runs the *same* `AlarmPoll` code the firmware's
/// `DeviceContext::poll_alarm_snapshot` runs.
#[derive(Debug, Default)]
pub struct FakeAlarmHost {
    pub af: bool,
    pub snapshot: Option<crate::app::RtcAlarmSnapshot>,
    pub snapshot_error: bool,
    pub alarm_flag_error: bool,
    /// Number of snapshot_ready dispatches the harness performed.
    pub dispatches: usize,
}

impl FakeAlarmHost {
    pub fn set(&mut self, af: bool, snapshot: crate::app::RtcAlarmSnapshot) {
        self.af = af;
        self.snapshot = Some(snapshot);
    }
}

/// Bridges `AlarmPoll` to the `Harness`'s runner + `FakeAlarmHost`.
struct FakeHostAdapter<'a> {
    harness: &'a mut Harness,
    host: &'a mut FakeAlarmHost,
}

impl AlarmHost for FakeHostAdapter<'_> {
    fn alarm_flag(&mut self) -> Result<bool, String> {
        if self.host.alarm_flag_error {
            Err("af read error".into())
        } else {
            Ok(self.host.af)
        }
    }
    fn read_snapshot(&mut self) -> Result<crate::app::RtcAlarmSnapshot, String> {
        if self.host.snapshot_error {
            Err("snapshot read error".into())
        } else {
            self.host
                .snapshot
                .clone()
                .ok_or_else(|| "no snapshot".into())
        }
    }
    fn snapshot_ready(&mut self, snapshot: crate::app::RtcAlarmSnapshot) -> bool {
        self.host.dispatches += 1;
        // New-ringing detection: this snapshot must transition the state
        // machine INTO AlarmRinging. If it was already ringing (a stale
        // / duplicate edge), the state machine ignores it and we must
        // not report a fresh ring.
        let was_ringing = self.harness.ringing();
        let ok = self
            .harness
            .dispatch(Event::RtcAlarmSnapshotReady(snapshot))
            .is_ok();
        ok && self.harness.ringing() && !was_ringing
    }
    fn dismiss(&mut self) {
        let _ = self
            .harness
            .dispatch(Event::Button(crate::button_event::ButtonEvent::Pressed(
                crate::button_event::ButtonId::Enter,
            )));
    }
    fn drain_kicks(&mut self) {
        self.harness.drain_kicks();
    }
}

/// Convenience driver combining `Runtime` + `FakeExecutor`, mirroring
/// the firmware's single-consumer event loop: push events, pump, drain
/// render kicks, and feed EPD completions (including stale ones) back.
#[derive(Debug, Default)]
pub struct Harness {
    pub runtime: Runtime,
    pub executor: FakeExecutor,
    /// Shared render-request registry (the same type the firmware uses),
    /// so EPD completion tests exercise `RenderRegistry::register/feed`.
    pub pending_renders: RenderRegistry,
    /// Shared alarm-poll state; `poll_alarm` runs the same `AlarmPoll`
    /// orchestration the firmware runs.
    pub alarm_poll: AlarmPoll,
}

impl Harness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one event into the queue and pump the runtime to quiescence
    /// (high-priority tier first, merged ticks last), then register any
    /// newly-issued render kicks.
    pub fn dispatch(&mut self, event: Event) -> Result<(), String> {
        self.runtime.push(event);
        self.runtime.pump(&mut self.executor)?;
        self.drain_kicks();
        Ok(())
    }

    /// Move newly-issued render kicks into the shared `RenderRegistry`.
    pub fn drain_kicks(&mut self) {
        for kick in self.runtime.take_kicks() {
            if kick.is_render() {
                self.pending_renders.register(kick);
            }
        }
    }

    /// Run the shared `AlarmPoll` orchestration through a fake host.
    /// Returns true when the state machine entered `AlarmRinging`.
    pub fn poll_alarm(&mut self, host: &mut FakeAlarmHost) -> bool {
        let mut poll = std::mem::take(&mut self.alarm_poll);
        let mut adapter = FakeHostAdapter {
            harness: self,
            host,
        };
        let firing = poll.poll(&mut adapter);
        self.alarm_poll = poll;
        firing
    }

    /// `main` consumes the sticky exit flag exactly once.
    pub fn take_alarm_exit(&mut self) -> bool {
        self.alarm_poll.take_alarm_exit()
    }

    pub fn alarm_exit(&self) -> bool {
        self.alarm_poll.alarm_exit()
    }

    /// Drive one full alarm lifecycle through the shared `AlarmPoll`
    /// orchestration the firmware runs: AF edge -> snapshot -> dispatch
    /// -> ring -> dismiss -> drain kicks. After this the state machine is
    /// back on Home in `WaitingForRearm` (both commits were
    /// auto-completed by the fake), and the AF edge is consumed. Callers
    /// then clear AF and advance the minute to rearm before the next
    /// source.
    pub fn complete_alarm_lifecycle(&mut self, host: &mut FakeAlarmHost) {
        assert!(
            self.poll_alarm(host),
            "AlarmPoll must ring on a fresh AF edge"
        );
        assert!(self.ringing(), "state machine entered AlarmRinging");
        // Dismiss: Firing -> WaitingForRearm.
        let mut poll = std::mem::take(&mut self.alarm_poll);
        let mut adapter = FakeHostAdapter {
            harness: self,
            host,
        };
        poll.ring_dismiss(&mut adapter);
        self.alarm_poll = poll;
        self.drain_kicks();
        assert_eq!(self.screen(), &Screen::Home, "dismiss restores Home");
        assert!(
            matches!(
                self.state().alarm_runtime,
                crate::app::AlarmRuntimeState::WaitingForRearm { .. }
            ),
            "dismiss -> WaitingForRearm"
        );
    }

    /// Feed an EPD completion for `request_id` back into the runtime via
    /// the shared `RenderRegistry::feed` (the same type the firmware
    /// uses). Completed/Failed push an EffectCompleted/EffectFailed event
    /// and pump; Superseded only terminates it; Ignored (stale/duplicate)
    /// does nothing. Returns true when a matching kick reached a terminal.
    pub fn complete_render(&mut self, request_id: u64, ok: bool) -> Result<bool, String> {
        let outcome = self.pending_renders.feed(request_id, ok, false);
        match outcome {
            FeedOutcome::Matched(_kick, RenderTerminal::Superseded) => {
                // Superseded: only terminate (never fed as RenderDone).
                Ok(true)
            }
            FeedOutcome::Matched(kick, terminal) => {
                let failure = if terminal == RenderTerminal::Failed {
                    Some(EffectError::Render("epd failed".into()))
                } else {
                    None
                };
                let completion = crate::app::EffectCompletion {
                    batch_id: kick.batch_id,
                    effect_id: kick.effect_id,
                    operation_id: kick.operation_id,
                    render_generation: kick.render_generation,
                    output: EffectOutput::RenderDone,
                };
                match failure {
                    Some(error) => {
                        self.runtime
                            .push(Event::EffectFailed(crate::app::EffectFailure {
                                batch_id: kick.batch_id,
                                effect_id: kick.effect_id,
                                operation_id: kick.operation_id,
                                render_generation: kick.render_generation,
                                error,
                            }));
                    }
                    None => self.runtime.push(Event::EffectCompleted(completion)),
                }
                self.runtime.pump(&mut self.executor)?;
                self.drain_kicks();
                Ok(true)
            }
            FeedOutcome::Ignored => Ok(false),
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.runtime.state().screen
    }

    pub fn state(&self) -> &AppState {
        self.runtime.state()
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
            status: crate::app::DeviceStatus::default(),
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
    use crate::button_event::ButtonId;
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

        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
        assert!(!h.ringing(), "dismiss leaves ringing screen");
        assert_eq!(h.screen(), &Screen::Home, "dismiss restores Home");
        assert_eq!(h.executor.log.count("StopTone"), 1);
        assert_eq!(
            h.executor
                .render_intents
                .iter()
                .filter(|intent| **intent == crate::app::RenderIntent::Full)
                .count(),
            1,
            "alarm dismiss submits exactly one full Home render"
        );
        assert_eq!(
            h.executor.render_intents.last(),
            Some(&crate::app::RenderIntent::Full),
            "alarm dismiss is the final render request"
        );
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
    }

    #[test]
    fn minute_tick_keeps_partial_render_intent() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        h.dispatch(Event::Tick(dt(9, 0))).unwrap();
        assert_eq!(
            h.executor.render_intents,
            vec![
                crate::app::RenderIntent::Partial,
                crate::app::RenderIntent::Partial
            ],
            "boot and ordinary minute ticks remain partial renders"
        );
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
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
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
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
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
        let rid = h
            .pending_renders
            .first_request_id()
            .expect("render kick has id");

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
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
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

    // ---- shared AlarmPoll orchestration (firmware's poll_alarm_snapshot
    // runs this exact code) -----------------------------------------------

    #[test]
    fn alarm_poll_shared_orchestration_rings_via_fake_host() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        assert!(h.poll_alarm(&mut host), "shared AlarmPoll must ring");
        assert_eq!(host.dispatches, 1);
        assert!(h.alarm_exit(), "exit flag set after ring");
        // A duplicate edge (still af=true) is not re-dispatched.
        assert!(!h.poll_alarm(&mut host), "edge consumed -> no re-ring");
        assert_eq!(host.dispatches, 1);
        // main consumes the flag exactly once.
        assert!(h.take_alarm_exit());
        assert!(!h.take_alarm_exit(), "flag consumed once");
    }

    #[test]
    fn alarm_poll_read_failure_keeps_edge_retryable() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let mut host = FakeAlarmHost {
            af: true,
            snapshot_error: true,
            ..FakeAlarmHost::default()
        };
        // First read fails: edge not consumed, error recorded for the
        // caller to log.
        assert!(!h.poll_alarm(&mut host));
        assert!(
            h.alarm_poll.take_error().is_some(),
            "read failure must surface an error for logging"
        );
        assert_eq!(h.alarm_poll.take_error(), None, "error consumed once");
        // Retry with a good snapshot: rings.
        host.snapshot_error = false;
        host.snapshot = Some(snapshot(dt(9, 0), true, true));
        assert!(h.poll_alarm(&mut host));
        assert_eq!(host.dispatches, 1);
    }

    #[test]
    fn alarm_poll_af_clear_resets_edge() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        assert!(h.poll_alarm(&mut host));
        // AF clears (ack): the edge resets; polling with AF low is a
        // no-op (not a fresh ring).
        host.af = false;
        assert!(!h.poll_alarm(&mut host));
        // Dismiss back to Home; the state machine leaves Firing.
        let mut poll = std::mem::take(&mut h.alarm_poll);
        let mut adapter = FakeHostAdapter {
            harness: &mut h,
            host: &mut host,
        };
        poll.ring_dismiss(&mut adapter);
        h.alarm_poll = poll;
        assert_eq!(h.screen(), &Screen::Home);
        assert!(h.take_alarm_exit());
        // The edge was consumed by the first ring; AF re-asserting after
        // the clear is a *new* edge, so the dispatcher is called again
        // (the state machine sees no due alarm at 9:01 -> residue ACK,
        // not a ring - that is correct: re-ring needs a due minute).
        host.set(true, snapshot(dt(9, 1), true, true));
        let _ = h.poll_alarm(&mut host);
        assert_eq!(host.dispatches, 2, "new AF edge re-dispatches");
        assert!(!h.ringing(), "no due alarm at 9:01, residue only");
    }

    // ---- shared reminder orchestration (firmware's reminders::poll) -----

    use crate::reminder_flow::{run_reminders, ReminderHost};

    struct ScriptReminder {
        urgent: BackgroundOutcome,
        todo: BackgroundOutcome,
        urgent_calls: usize,
        todo_calls: usize,
    }
    impl ReminderHost for ScriptReminder {
        fn urgent(&mut self) -> BackgroundOutcome {
            self.urgent_calls += 1;
            self.urgent
        }
        fn todo(&mut self) -> BackgroundOutcome {
            self.todo_calls += 1;
            self.todo
        }
    }

    #[test]
    fn reminder_chain_urgent_alarm_short_circuits_todo() {
        let mut s = ScriptReminder {
            urgent: BackgroundOutcome::AlarmHandled,
            todo: BackgroundOutcome::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), BackgroundOutcome::AlarmHandled);
        assert_eq!(s.todo_calls, 0, "todo must not run after alarm");
    }

    #[test]
    fn reminder_chain_dismiss_variants() {
        // urgent-only dismiss -> VisibleChanged (page redraws).
        let mut s = ScriptReminder {
            urgent: BackgroundOutcome::VisibleChanged,
            todo: BackgroundOutcome::NoChange,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), BackgroundOutcome::VisibleChanged);
        // todo-only dismiss -> VisibleChanged.
        let mut s = ScriptReminder {
            urgent: BackgroundOutcome::NoChange,
            todo: BackgroundOutcome::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), BackgroundOutcome::VisibleChanged);
        // urgent+todo dismiss -> VisibleChanged.
        let mut s = ScriptReminder {
            urgent: BackgroundOutcome::VisibleChanged,
            todo: BackgroundOutcome::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), BackgroundOutcome::VisibleChanged);
    }

    // ---- shared EPD registry (superseded terminal rule) ------------------

    use crate::epd_registry::{FeedOutcome, RenderRegistry, RenderTerminal};

    #[test]
    fn epd_registry_superseded_then_replacement() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let rid = h.pending_renders.first_request_id().unwrap();
        // Simulate the EPD single-slot: a newer render supersedes the
        // pending one. Both must end with exactly one terminal each.
        let mut reg = RenderRegistry::new();
        let pending_kicks = h.pending_renders.drain_all();
        let first = pending_kicks[0].clone();
        reg.register(first);
        let replacement = {
            let mut k = pending_kicks[0].clone();
            k.request_id = Some(rid + 1);
            k
        };
        reg.supersede_then_register(rid, replacement);
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.feed(rid, true, false),
            FeedOutcome::Ignored,
            "superseded id already terminal"
        );
        assert!(matches!(
            reg.feed(rid + 1, true, false),
            FeedOutcome::Matched(_, RenderTerminal::Completed)
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn epd_registry_each_id_one_terminal() {
        let mut reg = RenderRegistry::new();
        let k = crate::runner::AsyncKick {
            batch_id: crate::app::EffectBatchId(0),
            effect_id: crate::app::EffectId(1),
            operation_id: crate::app::OperationId(1),
            render_generation: Some(crate::app::RenderGeneration(1)),
            effect: Effect::Render(crate::app::RenderRequest {
                generation: crate::app::RenderGeneration(1),
                intent: crate::app::RenderIntent::Partial,
                view: crate::app::RenderView::Home,
            }),
            request_id: Some(5),
        };
        reg.register(k);
        assert!(matches!(
            reg.feed(5, false, false),
            FeedOutcome::Matched(_, RenderTerminal::Failed)
        ));
        assert_eq!(
            reg.feed(5, true, true),
            FeedOutcome::Ignored,
            "duplicate ignored"
        );
        assert_eq!(reg.len(), 0);
    }

    // ---- page unwind: alarm_exit consumed exactly once by main ----------

    #[test]
    fn page_unwind_flag_consumed_once_by_main() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        assert!(h.poll_alarm(&mut host));
        // A nested page sees the flag pending.
        assert!(h.alarm_exit());
        // main consumes it once; a second consume is a no-op.
        assert!(h.take_alarm_exit());
        assert!(!h.take_alarm_exit());
        assert!(!h.alarm_exit());
    }

    // ---- main/background routing: Home vs blocking consumption ------

    use crate::alarm_flow::AlarmSource;

    fn boot_clean(h: &mut Harness) {
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
    }

    /// Start a fresh AF edge for the next alarm at `minute`.
    fn fresh_edge(h: &mut Harness, host: &mut FakeAlarmHost, minute: u8) {
        host.af = false;
        assert!(!h.poll_alarm(host), "AF clear resets the edge");
        host.set(true, snapshot(dt(minute, 0), true, true));
    }

    /// After `complete_alarm_lifecycle`, advance the minute so the Daily
    /// alarm re-arms (WaitingForRearm -> Armed) and the next source can
    /// start from a clean armed state.
    fn rearm_after_dismiss(h: &mut Harness) {
        // Commits were auto-completed by the fake; advance the minute.
        h.dispatch(Event::Tick(dt(10, 0))).unwrap();
        assert!(matches!(
            h.state().alarm_runtime,
            crate::app::AlarmRuntimeState::Armed { .. }
        ));
    }

    /// Home alarm: full lifecycle, then the root consumes the flag at once.
    fn home_alarm_lifecycle(h: &mut Harness) {
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "AlarmPoll sets the flag on ring");
        // Home is the root: consume immediately via the shared policy.
        assert!(h.alarm_poll.consume_for(AlarmSource::Home));
        assert!(!h.alarm_exit(), "Home alarm flag cleared at once");
        // AF clear + rearm for the next source.
        fresh_edge(h, &mut host, 9);
        rearm_after_dismiss(h);
    }

    #[test]
    fn home_alarm_consumes_exit_flag_immediately() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "AlarmPoll sets the flag on ring");
        assert!(h.alarm_poll.consume_for(AlarmSource::Home));
        assert!(!h.alarm_exit(), "Home alarm flag cleared at once");
        assert_eq!(h.screen(), &Screen::Home);
    }

    #[test]
    fn blocking_alarm_keeps_flag_until_main_consumes() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        // Blocking source: flag stays for the caller chain.
        assert!(h.alarm_poll.consume_for(AlarmSource::BlockingPage));
        assert!(h.alarm_exit(), "blocking alarm keeps flag pending");
        // main consumes when the stack unwinds.
        assert!(h.take_alarm_exit());
        assert!(!h.alarm_exit());
    }

    #[test]
    fn home_then_blocking_do_not_leak_flags() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        home_alarm_lifecycle(&mut h);
        // Fresh edge for the blocking alarm after the Home lifecycle
        // cleared AF and rearmed.
        let mut host = FakeAlarmHost::default();
        fresh_edge(&mut h, &mut host, 9);
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "blocking alarm flag set after Home alarm");
        assert!(h.alarm_poll.consume_for(AlarmSource::BlockingPage));
        assert!(h.alarm_exit(), "blocking keeps flag until main consumes");
        assert!(h.take_alarm_exit());
        assert!(!h.alarm_exit());
    }

    #[test]
    fn blocking_then_home_do_not_leak_flags() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        // Blocking alarm lifecycle.
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit());
        assert!(h.take_alarm_exit(), "main consumes after stack unwinds");
        assert!(!h.alarm_exit());
        // Clean AF + rearm, then a Home alarm.
        fresh_edge(&mut h, &mut host, 9);
        rearm_after_dismiss(&mut h);
        home_alarm_lifecycle(&mut h);
        assert!(!h.alarm_exit());
    }

    #[test]
    fn full_route_home_alarm_then_navigation_entry() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        // Home alarm fires, full ring/dismiss lifecycle, root consumes.
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_poll.consume_for(AlarmSource::Home));
        assert!(!h.alarm_exit(), "no sticky flag leaked");
        assert_eq!(h.screen(), &Screen::Home);
        // Navigation/Settings entry check (alarm_exit_pending) is false.
        assert!(!h.alarm_exit(), "Navigation entry sees no pending alarm");
        // A reminder in that page, dismissed, propagates VisibleChanged.
        use crate::reminder_flow::{run_reminders, ReminderHost};
        struct NoAlarm;
        impl ReminderHost for NoAlarm {
            fn urgent(&mut self) -> BackgroundOutcome {
                BackgroundOutcome::NoChange
            }
            fn todo(&mut self) -> BackgroundOutcome {
                BackgroundOutcome::VisibleChanged
            }
        }
        assert_eq!(
            run_reminders(&mut NoAlarm),
            BackgroundOutcome::VisibleChanged
        );
    }

    #[test]
    fn blocking_route_alarm_unwind_then_epd_completion() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        // Blocking page alarm: full lifecycle.
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "blocking alarm pending for page unwind");
        // Page stack unwinds: consume via the shared policy.
        assert!(h.alarm_poll.consume_for(AlarmSource::BlockingPage));
        assert!(h.alarm_exit());
        assert!(h.take_alarm_exit());
        assert!(!h.alarm_exit());
        // The dismiss produced render kicks; complete them through the
        // shared RenderRegistry path (each request id gets one terminal).
        for rid in h.pending_renders.request_ids() {
            assert!(h.complete_render(rid, true).unwrap());
        }
        assert!(h.pending_renders.is_empty(), "no render left in flight");
        assert_eq!(h.screen(), &Screen::Home);
    }
}
