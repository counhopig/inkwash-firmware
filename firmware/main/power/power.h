// Wake causes, deep sleep and automatic light sleep
// (rust-firmware/src/power.rs, Slate's sleep_manager).
#pragma once

#include <cstdint>

namespace power {

enum class WakeCause { PowerOn, Enter, Down, RtcAlarm, Timer, ControlledRestart };

// Reads why this boot happened. Call once, early.
WakeCause ReadWakeCause();

// Enables automatic light sleep with the keys as wake sources.
void EnableLightSleep();

// Cuts the peripheral rails, arms the wake sources and enters deep sleep.
// timer_secs < 0 arms no timer wake.
[[noreturn]] void DeepSleep(int64_t timer_secs);

// Restarts through a short deep sleep (never esp_restart, see README).
// The next boot reports WakeCause::ControlledRestart.
[[noreturn]] void Restart();

// Survives deep sleep in RTC memory.
struct Retained {
    uint32_t magic;
    uint8_t open_ble_pairing;  // restart into BLE pairing on a clean heap
    uint8_t partial_refreshes; // since the last full refresh
    uint8_t home_valid;        // the panel shows `home` (see ui::HomeSnapshot)
    uint8_t reserved;
    uint8_t home[256];         // serialized Home model shown before sleep
};
Retained& State();

}  // namespace power
