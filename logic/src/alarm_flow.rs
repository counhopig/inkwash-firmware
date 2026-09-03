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

use crate::app::RtcAlarmSnapshot;

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
    /// Last read error (AF or snapshot), for the caller to log. Cleared
    /// by `take_error`.
    last_error: Option<String>,
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
        let af = match host.alarm_flag() {
            Ok(af) => af,
            Err(err) => {
                self.last_error = Some(err);
                return false;
            }
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
            Err(err) => {
                // Read failure: keep the edge retryable (do not consume).
                self.last_error = Some(err);
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

    /// The last read error (if any), consumed once by the caller so it
    /// can log it (the firmware warns on RTC/I2C failures).
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }
}

/// Where an alarm interrupted execution, deciding how the sticky exit
/// flag is consumed. Home is the root page: the flag must be cleared
/// right after the alarm so a later Navigation/Settings entry does not
/// treat the historical alarm as its own exit reason. A blocking page /
/// reminder must keep the flag set for its caller chain, and `main`
/// consumes it exactly once when the stack unwinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmSource {
    /// The alarm interrupted the root Home loop.
    Home,
    /// The alarm interrupted a nested page / reminder stack.
    BlockingPage,
}

impl AlarmPoll {
    /// Consume the exit flag according to the source policy. Returns
    /// true when the flag was set (Home consumes it immediately;
    /// BlockingPage reports it as pending for the caller).
    pub fn consume_for(&mut self, source: AlarmSource) -> bool {
        match source {
            AlarmSource::Home => self.take_alarm_exit(),
            AlarmSource::BlockingPage => self.alarm_exit(),
        }
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
