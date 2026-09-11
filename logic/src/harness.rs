use std::collections::BTreeMap;

use crate::alarm_flow::{AlarmHost, AlarmPoll};
use crate::app::{AppState, Effect, EffectError, EffectOutput, Event, RenderView, Screen};
use crate::epd_registry::{FeedOutcome, RenderRegistry, RenderTerminal};
use crate::runner::{EffectCategory, EffectExecutor, EffectOutcome};
use crate::runtime::Runtime;

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

#[derive(Clone, Debug, Default)]
pub struct ScriptedFailures {
    pub per_category: BTreeMap<EffectCategory, usize>,
}

impl ScriptedFailures {
    pub fn fail_next(&mut self, cat: EffectCategory, n: usize) -> &mut Self {
        self.per_category.insert(cat, n);
        self
    }

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

#[derive(Debug)]
pub struct FakeExecutor {
    pub log: EffectLog,
    pub failures: ScriptedFailures,

    pub next_request_id: u64,

    pub renders: Vec<(u64, Option<crate::app::RenderGeneration>)>,

    pub render_views: Vec<RenderView>,

    pub fail_render: bool,
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self {
            log: EffectLog::default(),
            failures: ScriptedFailures::default(),
            next_request_id: 1,
            renders: Vec::new(),
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

pub fn effect_name(effect: &Effect) -> &'static str {
    match effect {
        Effect::PersistAlarms(_) => "PersistAlarms",
        Effect::PersistTodos(_) => "PersistTodos",
        Effect::PersistInbox(_) => "PersistInbox",
        Effect::PersistConfig(_) => "PersistConfig",
        Effect::PersistWifiCredentials(_) => "PersistWifiCredentials",
        Effect::PersistSyncMetadata(_) => "PersistSyncMetadata",
        Effect::ApplySyncedData(_) => "ApplySyncedData",
        Effect::ClearSyncEtag => "ClearSyncEtag",
        Effect::ClearRtcAlignEpoch => "ClearRtcAlignEpoch",
        Effect::PersistTimezone(_) => "PersistTimezone",
        Effect::WriteRtcTime(_) => "WriteRtcTime",
        Effect::ProgramRtcAlarm(_) => "ProgramRtcAlarm",
        Effect::DisableRtcAlarm => "DisableRtcAlarm",
        Effect::AcknowledgeRtcAlarm => "AcknowledgeRtcAlarm",
        Effect::StartTone => "StartTone",
        Effect::StartReminderTone(_) => "StartReminderTone",
        Effect::StopTone => "StopTone",
        Effect::Reply { .. } => "Reply",
        Effect::Render(_) => "Render",
        Effect::StartSync(_) => "StartSync",
        Effect::PollUrgent => "PollUrgent",
        Effect::StartSetWifi(_) => "StartSetWifi",
        Effect::StartBlePairing(_) => "StartBlePairing",
        Effect::PersistAlarmToggle { .. } => "PersistAlarmToggle",
        Effect::PersistTodoEdit { .. } => "PersistTodoEdit",
        Effect::MarkInboxRead { .. } => "MarkInboxRead",
        Effect::PersistReminder(_) => "PersistReminder",
        Effect::CollectReminderFacts(_) => "CollectReminderFacts",
        Effect::SetSyncInterval { .. } => "SetSyncInterval",
        Effect::StopBlePairing => "StopBlePairing",
        Effect::EnterLightSleep(_) => "EnterLightSleep",
        Effect::DisableLightSleep => "DisableLightSleep",
        Effect::EnterDeepSleep(_) => "EnterDeepSleep",
        Effect::PrepareSleep { .. } => "PrepareSleep",
        Effect::CommitSleep(_) => "CommitSleep",
    }
}

impl EffectExecutor for FakeExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)> {
        let cat = match effect {
            Effect::PersistAlarms(_)
            | Effect::PersistTodos(_)
            | Effect::PersistInbox(_)
            | Effect::PersistConfig(_)
            | Effect::PersistWifiCredentials(_)
            | Effect::PersistSyncMetadata(_)
            | Effect::ApplySyncedData(_)
            | Effect::ClearRtcAlignEpoch
            | Effect::ClearSyncEtag
            | Effect::PersistTimezone(_) => EffectCategory::Persist,
            Effect::CollectReminderFacts(_) => EffectCategory::Persist,
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm | Effect::WriteRtcTime(_) => {
                EffectCategory::Rtc
            }
            Effect::AcknowledgeRtcAlarm => EffectCategory::Ack,
            Effect::StartTone | Effect::StartReminderTone(_) | Effect::StopTone => {
                EffectCategory::Tone
            }
            Effect::Reply { .. } => EffectCategory::Ack,
            Effect::Render(_) => EffectCategory::Render,
            Effect::StartSync(_) | Effect::PollUrgent | Effect::StartSetWifi(_) => {
                EffectCategory::Sync
            }
            Effect::StartBlePairing(_) | Effect::StopBlePairing => EffectCategory::Ble,
            Effect::MarkInboxRead { .. }
            | Effect::PersistReminder(_)
            | Effect::SetSyncInterval { .. } => EffectCategory::Persist,
            Effect::PersistAlarmToggle { .. } | Effect::PersistTodoEdit { .. } => {
                EffectCategory::Persist
            }
            Effect::EnterLightSleep(_) | Effect::EnterDeepSleep(_) => EffectCategory::Sleep,
            Effect::DisableLightSleep => EffectCategory::Sleep,
            Effect::PrepareSleep { .. } | Effect::CommitSleep(_) => EffectCategory::Sleep,
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
            Effect::PersistWifiCredentials(_) => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::WifiCredentials),
            )),
            Effect::PersistSyncMetadata(_) => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::SyncMetadata),
            )),
            Effect::ClearSyncEtag => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::SyncMetadata,
            ))),
            Effect::ClearRtcAlignEpoch => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::ApplySyncedData(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::SyncApply,
            ))),
            Effect::ProgramRtcAlarm(_) | Effect::DisableRtcAlarm => {
                Ok(EffectOutcome::Completed(EffectOutput::RtcProgrammed))
            }
            Effect::WriteRtcTime(dt) => {
                Ok(EffectOutcome::Completed(EffectOutput::RtcTimeWritten(*dt)))
            }
            Effect::PersistTimezone(_) => Ok(EffectOutcome::Completed(EffectOutput::Persisted(
                crate::app::PersistTarget::Timezone,
            ))),
            Effect::PersistReminder(_) => {
                Ok(EffectOutcome::Completed(EffectOutput::ReminderPersisted))
            }
            Effect::CollectReminderFacts(_) => {
                Ok(EffectOutcome::Completed(EffectOutput::ReminderFacts(None)))
            }
            Effect::AcknowledgeRtcAlarm => Ok(EffectOutcome::Completed(EffectOutput::AckDone)),
            Effect::StartTone | Effect::StartReminderTone(_) | Effect::StopTone => {
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
                self.render_views.push(req.view.clone());
                Ok(EffectOutcome::AsyncWithId(id))
            }
            Effect::StartSync(_)
            | Effect::PollUrgent
            | Effect::StartSetWifi(_)
            | Effect::StartBlePairing(_)
            | Effect::StopBlePairing => Ok(EffectOutcome::Async),
            Effect::PersistAlarmToggle { .. } => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::Alarms),
            )),
            Effect::PersistTodoEdit { .. } => Ok(EffectOutcome::Completed(
                EffectOutput::Persisted(crate::app::PersistTarget::Todos),
            )),
            Effect::MarkInboxRead { .. } => Ok(EffectOutcome::Completed(EffectOutput::RenderDone)),
            Effect::SetSyncInterval { .. } => {
                Ok(EffectOutcome::Completed(EffectOutput::RenderDone))
            }
            Effect::EnterLightSleep(_) => {
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepEntered))
            }
            Effect::DisableLightSleep => {
                Ok(EffectOutcome::Completed(EffectOutput::LightSleepDisabled))
            }
            Effect::EnterDeepSleep(_) => Ok(EffectOutcome::Async),
            Effect::PrepareSleep { .. } | Effect::CommitSleep(_) => Ok(EffectOutcome::Async),
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeAlarmHost {
    pub af: bool,
    pub snapshot: Option<crate::app::RtcAlarmSnapshot>,
    pub snapshot_error: bool,
    pub alarm_flag_error: bool,

    pub dispatches: usize,
}

impl FakeAlarmHost {
    pub fn set(&mut self, af: bool, snapshot: crate::app::RtcAlarmSnapshot) {
        self.af = af;
        self.snapshot = Some(snapshot);
    }
}

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

#[derive(Debug, Default)]
pub struct Harness {
    pub runtime: Runtime,
    pub executor: FakeExecutor,

    pub pending_renders: RenderRegistry,

    pub alarm_poll: AlarmPoll,
}

impl Harness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dispatch(&mut self, event: Event) -> Result<(), String> {
        self.runtime
            .try_push(event)
            .map_err(|_| "application event queue saturated".to_string())?;
        self.runtime
            .pump(&mut self.executor)
            .map_err(|error| error.to_string())?;
        self.drain_kicks();
        Ok(())
    }

    pub fn drain_kicks(&mut self) {
        for kick in self.runtime.take_kicks() {
            if kick.is_render() {
                self.pending_renders
                    .register(kick)
                    .expect("host render registry capacity");
            }
        }
    }

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

    pub fn take_alarm_exit(&mut self) -> bool {
        self.alarm_poll.take_alarm_exit()
    }

    pub fn alarm_exit(&self) -> bool {
        self.alarm_poll.alarm_exit()
    }

    pub fn complete_alarm_lifecycle(&mut self, host: &mut FakeAlarmHost) {
        assert!(
            self.poll_alarm(host),
            "AlarmPoll must ring on a fresh AF edge"
        );
        assert!(self.ringing(), "state machine entered AlarmRinging");

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

    pub fn complete_render(&mut self, request_id: u64, ok: bool) -> Result<bool, String> {
        let outcome = self.pending_renders.feed(request_id, ok, false);
        match outcome {
            FeedOutcome::Matched(_kick, RenderTerminal::Superseded) => Ok(true),
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
                        let _ =
                            self.runtime
                                .try_push(Event::EffectFailed(crate::app::EffectFailure {
                                    batch_id: kick.batch_id,
                                    effect_id: kick.effect_id,
                                    operation_id: kick.operation_id,
                                    render_generation: kick.render_generation,
                                    error,
                                }));
                    }
                    None => self
                        .runtime
                        .try_push(Event::EffectCompleted(completion))
                        .map_err(|_| "application event queue saturated".to_string())?,
                }
                self.runtime
                    .pump(&mut self.executor)
                    .map_err(|error| error.to_string())?;
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

    pub fn ringing(&self) -> bool {
        matches!(self.screen(), Screen::AlarmRinging)
    }
}

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
            h.executor.log.count("Render"),
            2,
            "boot render (1) + alarm-dismiss render (1) = 2"
        );
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
    }

    #[test]
    fn minute_tick_keeps_emitting_renders() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        let renders_after_boot = h.executor.log.count("Render");
        h.dispatch(Event::Tick(dt(9, 0))).unwrap();

        assert_eq!(
            h.executor.log.count("Render"),
            renders_after_boot + 1,
            "boot and the minute-change tick each emit one render request"
        );
    }

    #[test]
    fn ble_pairing_remains_serviceable_while_worker_start_is_pending() {
        let mut h = Harness::new();
        h.dispatch(Event::Boot(boot(Some(dt(8, 0)), false, true, vec![])))
            .unwrap();

        h.dispatch(Event::Button(Btn::LongPressed(ButtonId::Up)))
            .unwrap();
        h.dispatch(Event::Button(Btn::LongPressed(ButtonId::Down)))
            .unwrap();
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Down)))
            .unwrap();
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Down)))
            .unwrap();
        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
        assert_eq!(h.executor.log.count("StartBlePairing"), 1);

        h.dispatch(Event::Tick(dt(8, 1))).unwrap();
        h.dispatch(Event::Button(Btn::Released(ButtonId::Enter)))
            .unwrap();
        h.dispatch(Event::Button(Btn::LongPressed(ButtonId::Enter)))
            .unwrap();
        assert_eq!(h.screen(), &Screen::Settings { selected: 2 });
        assert_eq!(h.executor.log.count("StopBlePairing"), 1);

        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
        assert_eq!(h.executor.log.count("StartBlePairing"), 2);
    }

    #[test]
    fn runtime_alarm_rings_from_home() {
        let mut h = Harness::new();

        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(!h.ringing());

        h.dispatch(Event::RtcAlarmSnapshotReady(snapshot(dt(9, 0), true, true)))
            .unwrap();
        assert!(h.ringing(), "runtime AF must ring");
    }

    #[test]
    fn two_consecutive_alarms_each_ring_once() {
        let mut h = Harness::new();

        h.dispatch(Event::Boot(boot(
            Some(dt(9, 0)),
            true,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();
        assert!(h.ringing());

        h.dispatch(Event::RtcAlarmSnapshotReady(snapshot(dt(9, 0), true, true)))
            .unwrap();
        assert_eq!(
            h.executor.log.count("AcknowledgeRtcAlarm"),
            1,
            "no double ACK"
        );

        h.dispatch(Event::Button(Btn::Pressed(ButtonId::Enter)))
            .unwrap();
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::WaitingForRearm { .. }
        ));
        h.dispatch(Event::Tick(dt(9, 1))).unwrap();
        assert_eq!(h.executor.log.count("ProgramRtcAlarm"), 1);

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

        assert!(
            h.ringing(),
            "still firing (commit failed but not abandoned)"
        );

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

        h.executor.failures.fail_next(EffectCategory::Rtc, 1);
        h.dispatch(Event::Boot(boot(
            Some(dt(8, 59)),
            false,
            true,
            vec![alarm(1, 9, 0)],
        )))
        .unwrap();

        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::Degraded { .. }
        ));

        h.dispatch(Event::Tick(dt(9, 0))).unwrap();
        let program_count = h.executor.log.count("ProgramRtcAlarm");
        assert!(
            program_count >= 2,
            "program retried after transient failure, got {program_count}"
        );
    }

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

        assert_eq!(h.pending_renders.len(), 1);
        let rid = h
            .pending_renders
            .first_request_id()
            .expect("render kick has id");

        assert!(!h.complete_render(rid + 999, true).unwrap());

        assert!(h.complete_render(rid, true).unwrap());
        assert!(h.pending_renders.is_empty(), "kick consumed");

        assert!(!h.complete_render(rid, true).unwrap());
    }

    #[test]
    fn stale_render_completion_is_ignored() {
        let mut h = Harness::new();

        assert!(!h.complete_render(7, true).unwrap());
    }

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

        h.dispatch(Event::Tick(dt(9, 1))).unwrap();

        assert_eq!(h.executor.log.count("AcknowledgeRtcAlarm"), 1);
        assert_eq!(h.executor.log.count("PersistAlarms"), 1);
        assert_eq!(h.executor.log.count("StopTone"), 1);
        assert_eq!(h.executor.log.count("ProgramRtcAlarm"), 1);
        assert!(matches!(
            h.state().alarm_runtime,
            AlarmRuntimeState::Armed { alarm_id: 1 }
        ));
    }

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

        assert!(!h.poll_alarm(&mut host), "edge consumed -> no re-ring");
        assert_eq!(host.dispatches, 1);

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

        assert!(!h.poll_alarm(&mut host));
        assert!(
            h.alarm_poll.take_error().is_some(),
            "read failure must surface an error for logging"
        );
        assert_eq!(h.alarm_poll.take_error(), None, "error consumed once");

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

        host.af = false;
        assert!(!h.poll_alarm(&mut host));

        let mut poll = std::mem::take(&mut h.alarm_poll);
        let mut adapter = FakeHostAdapter {
            harness: &mut h,
            host: &mut host,
        };
        poll.ring_dismiss(&mut adapter);
        h.alarm_poll = poll;
        assert_eq!(h.screen(), &Screen::Home);
        assert!(h.take_alarm_exit());

        host.set(true, snapshot(dt(9, 1), true, true));
        let _ = h.poll_alarm(&mut host);
        assert_eq!(host.dispatches, 2, "new AF edge re-dispatches");
        assert!(!h.ringing(), "no due alarm at 9:01, residue only");
    }

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

        let mut reg = RenderRegistry::new();
        let pending_kicks = h.pending_renders.drain_all();
        let first = pending_kicks[0].clone();
        reg.register(first).unwrap();
        let replacement = {
            let mut k = pending_kicks[0].clone();
            k.request_id = Some(rid + 1);
            k
        };
        reg.supersede_then_register(rid, replacement).unwrap();
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
                view: crate::app::RenderView::Home,
                view_model: crate::render_plan::ViewModel::home(crate::app::RenderGeneration(1)),
            }),
            request_id: Some(5),
        };
        reg.register(k).unwrap();
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

        assert!(h.alarm_exit());

        assert!(h.take_alarm_exit());
        assert!(!h.take_alarm_exit());
        assert!(!h.alarm_exit());
    }

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

    fn fresh_edge(h: &mut Harness, host: &mut FakeAlarmHost, minute: u8) {
        host.af = false;
        assert!(!h.poll_alarm(host), "AF clear resets the edge");
        host.set(true, snapshot(dt(minute, 0), true, true));
    }

    fn rearm_after_dismiss(h: &mut Harness) {
        h.dispatch(Event::Tick(dt(10, 0))).unwrap();
        assert!(matches!(
            h.state().alarm_runtime,
            crate::app::AlarmRuntimeState::Armed { .. }
        ));
    }

    fn home_alarm_lifecycle(h: &mut Harness) {
        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "AlarmPoll sets the flag on ring");

        assert!(h.alarm_poll.consume_for(AlarmSource::Home));
        assert!(!h.alarm_exit(), "Home alarm flag cleared at once");

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

        assert!(h.alarm_poll.consume_for(AlarmSource::BlockingPage));
        assert!(h.alarm_exit(), "blocking alarm keeps flag pending");

        assert!(h.take_alarm_exit());
        assert!(!h.alarm_exit());
    }

    #[test]
    fn home_then_blocking_do_not_leak_flags() {
        let mut h = Harness::new();
        boot_clean(&mut h);
        home_alarm_lifecycle(&mut h);

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

        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit());
        assert!(h.take_alarm_exit(), "main consumes after stack unwinds");
        assert!(!h.alarm_exit());

        fresh_edge(&mut h, &mut host, 9);
        rearm_after_dismiss(&mut h);
        home_alarm_lifecycle(&mut h);
        assert!(!h.alarm_exit());
    }

    #[test]
    fn full_route_home_alarm_then_navigation_entry() {
        let mut h = Harness::new();
        boot_clean(&mut h);

        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_poll.consume_for(AlarmSource::Home));
        assert!(!h.alarm_exit(), "no sticky flag leaked");
        assert_eq!(h.screen(), &Screen::Home);

        assert!(!h.alarm_exit(), "Navigation entry sees no pending alarm");
    }

    #[test]
    fn blocking_route_alarm_unwind_then_epd_completion() {
        let mut h = Harness::new();
        boot_clean(&mut h);

        let mut host = FakeAlarmHost::default();
        host.set(true, snapshot(dt(9, 0), true, true));
        h.complete_alarm_lifecycle(&mut host);
        assert!(h.alarm_exit(), "blocking alarm pending for page unwind");

        assert!(h.alarm_poll.consume_for(AlarmSource::BlockingPage));
        assert!(h.alarm_exit());
        assert!(h.take_alarm_exit());
        assert!(!h.alarm_exit());

        for rid in h.pending_renders.request_ids() {
            assert!(h.complete_render(rid, true).unwrap());
        }
        assert!(h.pending_renders.is_empty(), "no render left in flight");
        assert_eq!(h.screen(), &Screen::Home);
    }
}
