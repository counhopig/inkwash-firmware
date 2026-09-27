// Key events with the Rust firmware's timing (rust-firmware/src/button.rs):
// 20 ms polls, 4-sample debounce, long press after 1 s. A short press is
// reported on release; a long press fires while held and its release is
// reported separately.
#pragma once

#include <functional>

#include "board.h"

namespace keys {

enum class Kind { Pressed, LongPressed, Released };

struct Event {
    Kind kind;
    board::Key key;
};

// Starts the key task. Keys already held at boot (the press that woke the
// device) produce no event until they are released and pressed again.
void Start(std::function<void(Event)> on_event);

}  // namespace keys
