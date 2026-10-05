#include "core/power_policy.h"
#include <algorithm>

namespace power_policy {
bool CanSleep(const Work& w) {
    return !(w.usb || w.network || w.alarm || w.reminder || w.pairing || w.reply ||
             w.audio || w.display || w.events || w.keys);
}
int64_t NextWakeSecs(bool valid, uint64_t epoch, int64_t maintenance) {
    int64_t delay = valid ? 60 - static_cast<int64_t>(epoch % 60) : 60;
    if (maintenance > 0) delay = std::min(delay, maintenance);
    return delay;
}
bool RetryPending(uint64_t utc, uint64_t deadline, uint64_t interval) {
    // A clock moved backward beyond the retry interval invalidates the deadline.
    return deadline > utc && deadline - utc <= interval;
}
int64_t IdleMs(bool background) {
    return background ? kBackgroundIdleMs : kInteractiveIdleMs;
}
}  // namespace power_policy
