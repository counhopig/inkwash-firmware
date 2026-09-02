//! Background-poll outcome vocabulary, shared between the host-testable
//! logic crate and the firmware blocking-page loops.
//!
//! A blocking page's background poll must distinguish "an RTC alarm
//! interrupted the page" (unwind to the unified router / main loop) from
//! ordinary visible changes (redraw / refresh data, keep the page) from
//! nothing happening. Collapsing these to a `bool` made every successful
//! sync / reminder / USB frame look like a user cancel; this enum is the
//! structured replacement, host-testable here.

/// Result of a blocking page's background poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundOutcome {
    /// An RTC alarm fired and AppRunner rang over the page. The page must
    /// unwind back to main, which re-renders per the current `Screen`.
    AlarmHandled,
    /// Something visible changed (sync receipt, reminder, USB command,
    /// clock minute) - redraw / refresh data but stay in the page.
    VisibleChanged,
    /// Nothing happened.
    NoChange,
}

impl BackgroundOutcome {
    /// Merges two outcomes by stable priority:
    /// AlarmHandled > VisibleChanged > NoChange. Used wherever two
    /// independent background sources (reminder chain, scheduled sync,
    /// USB, runtime) contribute to one page-level outcome, so a plain
    /// reminder dismissal is never lost behind a NoChange.
    pub fn merge(self, other: BackgroundOutcome) -> BackgroundOutcome {
        match (self, other) {
            (BackgroundOutcome::AlarmHandled, _) | (_, BackgroundOutcome::AlarmHandled) => {
                BackgroundOutcome::AlarmHandled
            }
            (BackgroundOutcome::VisibleChanged, _) | (_, BackgroundOutcome::VisibleChanged) => {
                BackgroundOutcome::VisibleChanged
            }
            _ => BackgroundOutcome::NoChange,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> BackgroundOutcome {
        BackgroundOutcome::AlarmHandled
    }
    fn v() -> BackgroundOutcome {
        BackgroundOutcome::VisibleChanged
    }
    fn n() -> BackgroundOutcome {
        BackgroundOutcome::NoChange
    }

    #[test]
    fn alarm_wins_over_everything() {
        for other in [a(), v(), n()] {
            assert_eq!(a().merge(other), a(), "AlarmHandled.merge({other:?})");
            assert_eq!(other.merge(a()), a(), "{other:?}.merge(AlarmHandled)");
        }
    }

    #[test]
    fn visible_beats_nochange() {
        assert_eq!(v().merge(n()), v());
        assert_eq!(n().merge(v()), v());
        assert_eq!(v().merge(v()), v());
    }

    #[test]
    fn nochange_is_identity() {
        assert_eq!(n().merge(n()), n());
    }

    #[test]
    fn merge_commutes_without_alarm() {
        // Without an alarm the merge is symmetric (VisibleChanged wins).
        for (x, y) in [(v(), n()), (n(), v()), (v(), v()), (n(), n())] {
            assert_eq!(x.merge(y), y.merge(x));
        }
    }
}
