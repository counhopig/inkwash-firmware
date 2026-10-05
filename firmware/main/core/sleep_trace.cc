#include "core/sleep_trace.h"

#include <cstring>

namespace sleep_trace {
bool Valid(const Ring& ring) {
    if (ring.magic != kMagic || ring.next >= kCapacity || ring.count > kCapacity) return false;
    const size_t start = (ring.next + kCapacity - ring.count) % kCapacity;
    for (size_t i = 0; i < ring.count; ++i) {
        const Entry& e = ring.entries[(start + i) % kCapacity];
        if (static_cast<uint8_t>(e.kind) > static_cast<uint8_t>(Kind::Restart) ||
            e.sequence != ring.sequence - static_cast<uint32_t>(ring.count - 1 - i)) return false;
    }
    return true;
}
void Append(Ring* ring, Entry entry) {
    if (!Valid(*ring)) std::memset(ring, 0, sizeof(*ring));
    ring->magic = kMagic;
    entry.sequence = ++ring->sequence;
    ring->entries[ring->next] = entry;
    ring->next = (ring->next + 1) % kCapacity;
    if (ring->count < kCapacity) ++ring->count;
}
const Entry* At(const Ring& ring, size_t oldest_index) {
    if (!Valid(ring) || oldest_index >= ring.count) return nullptr;
    return &ring.entries[(ring.next + kCapacity - ring.count + oldest_index) % kCapacity];
}
void SetBootTime(Ring* ring, uint64_t utc_secs) {
    if (!Valid(*ring) || ring->count == 0) return;
    Entry& e = ring->entries[(ring->next + kCapacity - 1) % kCapacity];
    if (e.kind == Kind::Boot) e.utc_secs = utc_secs;
}
}  // namespace sleep_trace
