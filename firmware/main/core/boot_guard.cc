#include "core/boot_guard.h"

namespace boot_guard {

bool IsFailure(ResetKind kind) {
    switch (kind) {
        case ResetKind::Panic:
        case ResetKind::TaskWatchdog:
        case ResetKind::InterruptWatchdog:
        case ResetKind::Watchdog:
        case ResetKind::Brownout:
        case ResetKind::PowerGlitch:
        case ResetKind::CpuLockup:
        case ResetKind::SoftwareReset:
            return true;
        default:
            return false;
    }
}

const char* Label(ResetKind kind) {
    switch (kind) {
        case ResetKind::PowerOn: return "power-on";
        case ResetKind::ExternalReset: return "external";
        case ResetKind::SoftwareReset: return "software";
        case ResetKind::DeepSleep: return "deep-sleep";
        case ResetKind::Usb: return "usb";
        case ResetKind::Jtag: return "jtag";
        case ResetKind::Panic: return "panic";
        case ResetKind::TaskWatchdog: return "task-watchdog";
        case ResetKind::InterruptWatchdog: return "interrupt-watchdog";
        case ResetKind::Watchdog: return "watchdog";
        case ResetKind::Brownout: return "brownout";
        case ResetKind::PowerGlitch: return "power-glitch";
        case ResetKind::CpuLockup: return "cpu-lockup";
        case ResetKind::Other: return "unknown";
    }
    return "unknown";
}

Ledger Ledger::NoteAttempt(ResetKind reset) const {
    const uint32_t prior = Recorded() ? failures : 0;
    uint32_t next = 0;
    if (IsFailure(reset)) {
        next = prior + 1 > kMaxBootFailures ? kMaxBootFailures : prior + 1;
    }
    return Ledger{kMagic, next};
}

}  // namespace boot_guard
