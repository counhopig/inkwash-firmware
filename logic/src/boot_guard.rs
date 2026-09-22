//! Boot-attempt ledger.
//!
//! A firmware that hangs or panics early reboots in a loop, and nothing in the
//! application notices: the reset reason is printed and the run starts again.
//! The ledger turns that into a decision. It is a tiny record of how many
//! consecutive *failed* boot attempts preceded the current one, kept in memory
//! that survives a reset but not a power cycle, so:
//!
//! - only attributable failures count (a panic, a watchdog, a brownout, an
//!   unexpected software reset); a power-on, the reset pin, a USB-driven reset
//!   or a deep-sleep wake is an operator action and starts a fresh run;
//! - a power cycle always clears the record, because the memory it lives in
//!   loses power;
//! - the record is only cleared on purpose once the boot path has finished
//!   initializing, so a run that dies before that keeps counting.
//!
//! Reaching [`MAX_CONSECUTIVE_BOOT_FAILURES`] parks the device in the minimum
//! safe mode, which keeps USB diagnostics alive and never erases stored data.

/// Marks a ledger written by this firmware; anything else is cold memory.
pub const BOOT_LEDGER_MAGIC: u32 = 0x494E_4B57;

/// Consecutive failed attempts that park the device in minimum safe mode.
pub const MAX_CONSECUTIVE_BOOT_FAILURES: u32 = 3;

/// Why the chip restarted, as far as the reset controller can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ResetKind {
    PowerOn,
    ExternalReset,
    SoftwareReset,
    DeepSleep,
    Usb,
    Jtag,
    Panic,
    TaskWatchdog,
    InterruptWatchdog,
    Watchdog,
    Brownout,
    PowerGlitch,
    CpuLockup,
    Other,
}

impl ResetKind {
    pub const ALL: [ResetKind; 14] = [
        ResetKind::PowerOn,
        ResetKind::ExternalReset,
        ResetKind::SoftwareReset,
        ResetKind::DeepSleep,
        ResetKind::Usb,
        ResetKind::Jtag,
        ResetKind::Panic,
        ResetKind::TaskWatchdog,
        ResetKind::InterruptWatchdog,
        ResetKind::Watchdog,
        ResetKind::Brownout,
        ResetKind::PowerGlitch,
        ResetKind::CpuLockup,
        ResetKind::Other,
    ];

    /// True only for resets that mean the previous attempt ended badly. A
    /// reset the firmware cannot attribute (`Other`) is not evidence of a
    /// failure, so it never accumulates toward the limit.
    pub const fn is_failure(self) -> bool {
        match self {
            ResetKind::Panic
            | ResetKind::TaskWatchdog
            | ResetKind::InterruptWatchdog
            | ResetKind::Watchdog
            | ResetKind::Brownout
            | ResetKind::PowerGlitch
            | ResetKind::CpuLockup
            | ResetKind::SoftwareReset => true,
            ResetKind::PowerOn
            | ResetKind::ExternalReset
            | ResetKind::DeepSleep
            | ResetKind::Usb
            | ResetKind::Jtag
            | ResetKind::Other => false,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            ResetKind::PowerOn => "power-on",
            ResetKind::ExternalReset => "external",
            ResetKind::SoftwareReset => "software",
            ResetKind::DeepSleep => "deep-sleep",
            ResetKind::Usb => "usb",
            ResetKind::Jtag => "jtag",
            ResetKind::Panic => "panic",
            ResetKind::TaskWatchdog => "task-watchdog",
            ResetKind::InterruptWatchdog => "interrupt-watchdog",
            ResetKind::Watchdog => "watchdog",
            ResetKind::Brownout => "brownout",
            ResetKind::PowerGlitch => "power-glitch",
            ResetKind::CpuLockup => "cpu-lockup",
            ResetKind::Other => "unknown",
        }
    }
}

/// The raw record as stored in retained memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootLedger {
    magic: u32,
    failures: u32,
}

impl BootLedger {
    pub const fn from_raw(magic: u32, failures: u32) -> Self {
        Self { magic, failures }
    }

    pub const fn magic(self) -> u32 {
        self.magic
    }

    /// Consecutive failed attempts that preceded the attempt starting now.
    pub const fn failures(self) -> u32 {
        self.failures
    }

    /// A record this firmware wrote and can still trust. Cold memory holds
    /// whatever the previous power cycle left there, so both the marker and a
    /// plausible count are required.
    pub fn is_recorded(self) -> bool {
        self.magic == BOOT_LEDGER_MAGIC && self.failures <= MAX_CONSECUTIVE_BOOT_FAILURES
    }

    /// Ledger for the attempt that is starting now.
    pub fn note_attempt(self, reset: ResetKind) -> Self {
        let prior = if self.is_recorded() { self.failures } else { 0 };
        let failures = if reset.is_failure() {
            prior.saturating_add(1).min(MAX_CONSECUTIVE_BOOT_FAILURES)
        } else {
            0
        };
        Self {
            magic: BOOT_LEDGER_MAGIC,
            failures,
        }
    }

    /// Consecutive failures reached the limit: the device parks in safe mode
    /// instead of running the same failing initialization again.
    pub fn exhausted(self) -> bool {
        self.is_recorded() && self.failures >= MAX_CONSECUTIVE_BOOT_FAILURES
    }

    /// The boot path finished initializing, so the run no longer counts as
    /// failed. The marker is kept deliberately: the next attempt is still
    /// recognizable as part of the same power session.
    pub fn cleared(self) -> Self {
        Self {
            magic: BOOT_LEDGER_MAGIC,
            failures: 0,
        }
    }

    /// Message for the boot log and the safe-mode panel: what was counted and
    /// how to leave safe mode, with no device data in it.
    pub fn reason(self) -> String {
        format!(
            "boot loop: {} failed boots in a row (reset or power cycle to retry)",
            self.failures
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_memory_starts_a_fresh_run() {
        for raw in [
            BootLedger::from_raw(0, 0),
            BootLedger::from_raw(0xDEAD_BEEF, 2),
            BootLedger::from_raw(BOOT_LEDGER_MAGIC, MAX_CONSECUTIVE_BOOT_FAILURES + 1),
            BootLedger::from_raw(BOOT_LEDGER_MAGIC.wrapping_add(1), 1),
        ] {
            assert!(!raw.is_recorded(), "{raw:?} is not a ledger we wrote");
            assert_eq!(raw.note_attempt(ResetKind::Panic).failures(), 1);
            assert!(!raw.exhausted());
        }
    }

    #[test]
    fn consecutive_failures_reach_the_limit_and_saturate() {
        let mut ledger = BootLedger::from_raw(0, 0).note_attempt(ResetKind::PowerOn);
        assert_eq!(ledger.failures(), 0);
        assert!(!ledger.exhausted());

        let mut seen = Vec::new();
        for _ in 0..6 {
            ledger = ledger.note_attempt(ResetKind::Panic);
            seen.push((ledger.failures(), ledger.exhausted()));
        }
        assert_eq!(
            seen,
            vec![
                (1, false),
                (2, false),
                (MAX_CONSECUTIVE_BOOT_FAILURES, true),
                (MAX_CONSECUTIVE_BOOT_FAILURES, true),
                (MAX_CONSECUTIVE_BOOT_FAILURES, true),
                (MAX_CONSECUTIVE_BOOT_FAILURES, true),
            ],
            "the third consecutive failure must stop the run, and the count must not overflow"
        );
    }

    #[test]
    fn a_watchdog_or_brownout_loop_is_counted_like_a_panic() {
        let start = BootLedger::from_raw(0, 0).note_attempt(ResetKind::PowerOn);
        for reset in [
            ResetKind::TaskWatchdog,
            ResetKind::InterruptWatchdog,
            ResetKind::Watchdog,
            ResetKind::Brownout,
            ResetKind::PowerGlitch,
            ResetKind::CpuLockup,
            ResetKind::SoftwareReset,
        ] {
            assert!(reset.is_failure(), "{reset:?} is an abnormal reset");
            assert_eq!(start.note_attempt(reset).failures(), 1);
        }
    }

    #[test]
    fn a_power_cycle_or_operator_reset_clears_an_exhausted_run() {
        let exhausted = BootLedger::from_raw(BOOT_LEDGER_MAGIC, MAX_CONSECUTIVE_BOOT_FAILURES);
        assert!(exhausted.exhausted());

        for reset in [
            ResetKind::PowerOn,
            ResetKind::ExternalReset,
            ResetKind::DeepSleep,
            ResetKind::Usb,
            ResetKind::Jtag,
            ResetKind::Other,
        ] {
            assert!(!reset.is_failure(), "{reset:?} is not an abnormal reset");
            let next = exhausted.note_attempt(reset);
            assert_eq!(next.failures(), 0);
            assert!(!next.exhausted(), "{reset:?} must always allow a retry");
        }
    }

    #[test]
    fn clearing_after_initialization_only_ends_the_current_run() {
        let running = BootLedger::from_raw(BOOT_LEDGER_MAGIC, 2);
        let cleared = running.cleared();
        assert!(cleared.is_recorded());
        assert_eq!(cleared.failures(), 0);
        assert!(!cleared.exhausted());
        assert_eq!(
            cleared.note_attempt(ResetKind::Panic).failures(),
            1,
            "a fresh run starts counting from zero after a successful boot"
        );
    }

    #[test]
    fn the_reason_names_the_count_and_fits_the_panel() {
        let reason =
            BootLedger::from_raw(BOOT_LEDGER_MAGIC, MAX_CONSECUTIVE_BOOT_FAILURES).reason();
        assert!(reason.contains('3'), "`{reason}` must state the count");
        assert!(reason.is_ascii());
        assert!(reason.chars().count() <= 80);
    }

    #[test]
    fn labels_are_distinct_and_ascii() {
        let mut labels: Vec<&str> = ResetKind::ALL.iter().map(|kind| kind.label()).collect();
        for label in &labels {
            assert!(label.is_ascii(), "`{label}` is written to an ASCII log");
        }
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), ResetKind::ALL.len());
    }
}
