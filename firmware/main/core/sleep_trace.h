#pragma once

#include <cstddef>
#include <cstdint>
#include <type_traits>

namespace sleep_trace {
constexpr size_t kCapacity = 16;
constexpr uint32_t kMagic = 0x53545231;
enum class Kind : uint8_t { Boot, Sleep, SafeSleep, Restart };

// No initializers: RTC NOINIT storage must survive reset without constructors.
struct Entry {
    uint32_t sequence;
    uint32_t uptime_ms;
    uint64_t utc_secs;
    int64_t timer_secs;
    uint64_t wake_mask;
    Kind kind;
    uint8_t reset;
    uint8_t wake;
    uint8_t reserved;
};
struct Ring {
    uint32_t magic;
    uint32_t sequence;
    uint32_t next;
    uint32_t count;
    Entry entries[kCapacity];
};
static_assert(std::is_trivial<Ring>::value, "RTC trace must not run constructors");
static_assert(sizeof(Ring) <= 1024, "RTC trace must stay within its memory budget");
bool Valid(const Ring& ring);
void Append(Ring* ring, Entry entry);
const Entry* At(const Ring& ring, size_t oldest_index);
void SetBootTime(Ring* ring, uint64_t utc_secs);
}  // namespace sleep_trace
