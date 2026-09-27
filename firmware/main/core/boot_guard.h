// Boot-attempt ledger (logic/src/boot_guard.rs).
//
// A firmware that panics early reboots in a loop and drains the battery. The
// ledger counts consecutive *failed* boots (panic, watchdog, brownout,
// unexpected software reset) in memory that survives a reset but not a power
// cycle. A power-on, the reset pin, USB or a deep-sleep wake starts a fresh
// run. Reaching kMaxBootFailures parks the device in safe mode.
#pragma once

#include <cstdint>

namespace boot_guard {

constexpr uint32_t kMagic = 0x494E4B57;  // "INKW"
constexpr uint32_t kMaxBootFailures = 3;

enum class ResetKind : uint8_t {
    PowerOn, ExternalReset, SoftwareReset, DeepSleep, Usb, Jtag, Panic,
    TaskWatchdog, InterruptWatchdog, Watchdog, Brownout, PowerGlitch, CpuLockup, Other,
};

bool IsFailure(ResetKind kind);
const char* Label(ResetKind kind);

struct Ledger {
    uint32_t magic = 0;
    uint32_t failures = 0;

    // Written by this firmware (cold memory holds anything).
    bool Recorded() const { return magic == kMagic && failures <= kMaxBootFailures; }
    // The ledger for the attempt starting now.
    Ledger NoteAttempt(ResetKind reset) const;
    bool Exhausted() const { return Recorded() && failures >= kMaxBootFailures; }
    // Boot finished; the run no longer counts as failed.
    Ledger Cleared() const { return Ledger{kMagic, 0}; }
};

}  // namespace boot_guard
