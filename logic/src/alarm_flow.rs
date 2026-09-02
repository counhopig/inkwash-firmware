//! Shared alarm-poll orchestration, host-testable.
//!
//! The firmware's `DeviceContext::poll_alarm_snapshot` and the main
//! loop's `app_runner_alarm_exit` consumption are the *same* control
//! flow on every entry point: read the AF edge, read a consistent
//! snapshot, dispatch `RtcAlarmSnapshotReady`, set an exit flag when the
//! state machine enters `AlarmRinging`, and let `main` consume the flag
//! exactly once. That flow is pure orchestration; this module
//! implements it against minimal traits so a host harness can drive the
//! *same* code the firmware runs, instead of rewriting the outcome in a
//! test.
//!
//! The firmware's `DeviceContext` implements [`AlarmDriver`] and
//! [`AlarmDispatcher`] as thin adapters over `board.rtc` and
//! `app_runner`; the harness implements them with fakes.

use crate::app::{Event, RtcAlarmSnapshot};

/// The full alarm-poll host: reads RTC facts and dispatches into the
/// state machine. One trait (not two) so a firmware adapter can borrow
/// `&mut DeviceContext` once; the harness implements it with a fake that
/// drives the same `AlarmPoll` code the firmware runs.
pub trait AlarmHost {
    /// True when the AF (alarm flag) is currently asserted. A read error
    /// is reported as `Err` so the caller keeps the edge retryable.
    fn alarm_flag(&mut self) -> Result<bool, String>;
    /// Reads a consistent (time, AF=true, AIE) snapshot. Only returned
    /// when all three facts read successfully.
    fn read_snapshot(&mut self) -> Result<RtcAlarmSnapshot, String>;
    /// Dispatch `RtcAlarmSnapshotReady`; returns true when the state
    /// machine entered `AlarmRinging`.
    fn snapshot_ready(&mut self, snapshot: RtcAlarmSnapshot) -> bool;
    /// Dispatch the ENTER press that dismisses a firing alarm
    /// (Firing -> WaitingForRearm).
    fn dismiss(&mut self);
    /// Drain any render kicks the dispatch produced (the firmware
    /// forwards them into `pending_renders`).
    fn drain_kicks(&mut self);
}

/// Shared alarm-poll state: tracks the AF edge and the sticky exit flag.
///
/// `alarm_exit` is set when an alarm interrupted the page and consumed
/// exactly once by `main` (`take_alarm_exit`). The firmware's
/// `DeviceContext::app_runner_alarm_exit` and `main`'s clearing both map
/// onto this.
#[derive(Debug, Default)]
pub struct AlarmPoll {
    /// The AF edge was consumed (a snapshot was dispatched). A read
    /// failure leaves it false so the next poll retries.
    edge_consumed: bool,
    /// Set when an alarm interrupted the current page; `main` consumes
    /// it exactly once.
    alarm_exit: bool,
}

impl AlarmPoll {
    pub fn new() -> Self {
        Self::default()
    }

    /// The firmware's `poll_alarm_snapshot`: on a fresh AF edge, read a
    /// consistent snapshot and dispatch; on success set the exit flag
    /// when ringing. Returns true when the state machine entered
    /// `AlarmRinging` (the page should unwind).
    pub fn poll<H: AlarmHost>(&mut self, host: &mut H) -> bool {
        let Ok(af) = host.alarm_flag() else {
            return false;
        };
        if !af {
            self.edge_consumed = false;
            return false;
        }
        if self.edge_consumed {
            // Same AF edge already dispatched; do not re-interpret.
            return false;
        }
        let snapshot = match host.read_snapshot() {
            Ok(s) => s,
            Err(_) => {
                // Read failure: keep the edge retryable (do not consume).
                return false;
            }
        };
        let firing = host.snapshot_ready(snapshot);
        self.edge_consumed = true;
        host.drain_kicks();
        if firing {
            self.alarm_exit = true;
        }
        firing
    }

    /// After the state machine entered `Firing` (or is already ringing
    /// from a previous edge), drive the blocking ring/dismiss loop and
    /// report back. The caller owns the physical ring screen; this only
    /// dispatches the dismiss event.
    pub fn ring_dismiss<H: AlarmHost>(&mut self, host: &mut H) {
        host.dismiss();
        host.drain_kicks();
    }

    /// The sticky flag, for the page-unwind checks.
    pub fn alarm_exit(&self) -> bool {
        self.alarm_exit
    }

    /// `main` consumes the exit flag exactly once.
    pub fn take_alarm_exit(&mut self) -> bool {
        let v = self.alarm_exit;
        self.alarm_exit = false;
        v
    }
}

/// Convenience: a page's alarm-unwind check. A nested page that cannot
/// return an alarm-specific result sets the flag via [`AlarmPoll::mark`].
impl AlarmPoll {
    /// Nested pages that must unwind set this; `main` consumes it once.
    pub fn mark_exit(&mut self) {
        self.alarm_exit = true;
    }
}

/// The event dispatch used by `AlarmDispatcher` implementations.
pub fn snapshot_event(snapshot: RtcAlarmSnapshot) -> Event {
    Event::RtcAlarmSnapshotReady(snapshot)
}

pub fn dismiss_event() -> Event {
    Event::Button(crate::button_event::ButtonEvent::Pressed)
}
