// The application: one task owns all state; keys, USB, BLE and the network
// worker only post events to it.
#pragma once

#include "power/power.h"

namespace app {

[[noreturn]] void Run(power::WakeCause wake);

}  // namespace app
