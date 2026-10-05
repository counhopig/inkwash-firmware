#pragma once
#include <cstdint>
#include <string>
#include "core/protocol.h"

namespace event_log {
bool Init();
// Call from normal task context. Event strings must contain no credentials or content.
void Add(const char* format, ...) __attribute__((format(printf, 1, 2)));
bool Critical(const char* format, ...) __attribute__((format(printf, 1, 2)));
bool LastClockEstimated();
void SetClock(uint64_t utc_secs, bool estimated);
void Poll();
bool Flush();
std::string Export(const protocol::Command& command);
}  // namespace event_log
