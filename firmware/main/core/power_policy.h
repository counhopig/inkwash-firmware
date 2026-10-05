#pragma once
#include <cstdint>

namespace power_policy {
constexpr int64_t kInteractiveIdleMs = 180000;
constexpr int64_t kBackgroundIdleMs = 2000;

struct Work {
    bool usb = false;
    bool network = false;
    bool alarm = false;
    bool reminder = false;
    bool pairing = false;
    bool reply = false;
    bool audio = false;
    bool display = false;
    bool events = false;
    bool keys = false;
};
bool CanSleep(const Work& work);
// The Home clock must advance at every minute boundary, including midnight.
int64_t NextWakeSecs(bool clock_valid, uint64_t rtc_epoch, int64_t maintenance_secs);
int64_t IdleMs(bool background);
bool RetryPending(uint64_t utc, uint64_t deadline, uint64_t interval_secs);
}  // namespace power_policy
