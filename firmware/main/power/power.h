// Wake causes, deep sleep and automatic light sleep.
#pragma once

#include <cstdint>

#include "core/boot_guard.h"

namespace power {

enum class WakeCause { PowerOn, Enter, Down, RtcAlarm, Timer, ControlledRestart };

// Reads why this boot happened. Call once, early.
WakeCause ReadWakeCause();

// Enables automatic light sleep with the keys as wake sources.
void EnableLightSleep();

// Cuts the peripheral rails, arms the wake sources and enters deep sleep.
// timer_secs < 0 arms no timer wake.
[[noreturn]] void DeepSleep(int64_t timer_secs, uint64_t utc_secs = 0);

// Print the retained sleep/reset history; zero time means unavailable.
void PrintSleepTrace(uint64_t boot_utc_secs = 0);

// Restarts through a short deep sleep (never esp_restart, see README).
// The next boot reports WakeCause::ControlledRestart.
[[noreturn]] void Restart();

// Records this boot attempt in the retained boot ledger (core/boot_guard.h)
// and returns the updated ledger. Call first thing in app_main.
boot_guard::Ledger NoteBootAttempt(boot_guard::ResetKind* reset);
// The boot path finished: this run no longer counts as a failed boot.
void ClearBootLedger();

// Deep sleep that only the Enter key ends (safe mode: no timer, no RTC alarm).
[[noreturn]] void DeepSleepUntilEnter();

// Survives deep sleep in RTC memory.
struct Retained {
    uint32_t magic;
    uint8_t open_ble_pairing;  // restart into BLE pairing on a clean heap
    uint8_t partial_refreshes; // since the last full refresh
    uint8_t home_valid;        // the panel shows `home` (see ui::HomeSnapshot)
    uint8_t reserved;
    uint64_t network_retry_utc; // offline backoff survives minute wakes
    uint8_t home[256];         // serialized Home model shown before sleep
};
Retained& State();

}  // namespace power
