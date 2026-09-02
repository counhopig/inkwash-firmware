//! Shared reminder orchestration, host-testable.
//!
//! The firmware's `reminders::poll` runs urgent inbox alerts first, then
//! due-todo reminders, and short-circuits when an RTC alarm preempted a
//! reminder (no further reminder class runs after one). That decision
//! flow is pure orchestration; this module implements it against a
//! [`ReminderHost`] trait so a host harness drives the same code the
//! firmware runs.

use crate::background_outcome::BackgroundOutcome;

/// Shows the two reminder classes. Firmware: `reminders.rs`; harness: a
/// fake that scripts dismiss vs alarm-preempt outcomes.
pub trait ReminderHost {
    /// Show urgent inbox alerts. Returns the outcome of that class
    /// (AlarmHandled when an alarm preempted it, VisibleChanged when it
    /// was dismissed, NoChange when nothing was shown).
    fn urgent(&mut self) -> BackgroundOutcome;
    /// Show due-todo reminders. Same outcome contract.
    fn todo(&mut self) -> BackgroundOutcome;
}

/// Runs the reminder chain with the firmware's exact semantics:
/// urgent first, then todo; an `AlarmHandled` short-circuits and no
/// further class runs.
pub fn run_reminders<H: ReminderHost>(host: &mut H) -> BackgroundOutcome {
    let urgent_outcome = host.urgent();
    if urgent_outcome == BackgroundOutcome::AlarmHandled {
        return BackgroundOutcome::AlarmHandled;
    }
    let todo_outcome = host.todo();
    // Stable-priority merge so a plain urgent dismissal (VisibleChanged)
    // is not lost when no todo reminder ran.
    urgent_outcome.merge(todo_outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_outcome::BackgroundOutcome as B;

    struct Script {
        urgent: B,
        todo: B,
        urgent_calls: usize,
        todo_calls: usize,
    }
    impl ReminderHost for Script {
        fn urgent(&mut self) -> B {
            self.urgent_calls += 1;
            self.urgent
        }
        fn todo(&mut self) -> B {
            self.todo_calls += 1;
            self.todo
        }
    }

    #[test]
    fn urgent_dismiss_then_todo_dismiss_is_visible() {
        let mut s = Script {
            urgent: B::VisibleChanged,
            todo: B::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), B::VisibleChanged);
        assert_eq!((s.urgent_calls, s.todo_calls), (1, 1));
    }

    #[test]
    fn urgent_only_dismiss_stays_visible() {
        let mut s = Script {
            urgent: B::VisibleChanged,
            todo: B::NoChange,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), B::VisibleChanged);
    }

    #[test]
    fn todo_only_dismiss_is_visible() {
        let mut s = Script {
            urgent: B::NoChange,
            todo: B::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), B::VisibleChanged);
    }

    #[test]
    fn urgent_alarm_short_circuits_todo() {
        let mut s = Script {
            urgent: B::AlarmHandled,
            todo: B::VisibleChanged,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), B::AlarmHandled);
        assert_eq!(
            (s.urgent_calls, s.todo_calls),
            (1, 0),
            "todo must not run after alarm"
        );
    }

    #[test]
    fn nothing_shown_is_nochange() {
        let mut s = Script {
            urgent: B::NoChange,
            todo: B::NoChange,
            urgent_calls: 0,
            todo_calls: 0,
        };
        assert_eq!(run_reminders(&mut s), B::NoChange);
    }
}
