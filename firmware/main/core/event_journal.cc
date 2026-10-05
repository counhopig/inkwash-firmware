#include "core/event_journal.h"

#include <cstring>
#include <limits>

namespace event_journal {
namespace {
uint32_t Crc(const Record& r) {
    uint32_t crc = 0xffffffff;
    const auto* bytes = reinterpret_cast<const uint8_t*>(&r);
    for (size_t i = 8; i < sizeof(r); ++i) {
        crc ^= bytes[i];
        for (int bit = 0; bit < 8; ++bit) crc = (crc >> 1) ^ (0xedb88320u & (0u - (crc & 1u)));
    }
    return ~crc;
}
bool Empty(const Record& r) {
    const auto* bytes = reinterpret_cast<const uint8_t*>(&r);
    for (size_t i = 0; i < sizeof(r); ++i) if (bytes[i] != 0xff) return false;
    return true;
}
}  // namespace

bool Valid(const Record& r) {
    return r.magic == kMagic && r.sequence != 0 && r.reserved == 0 &&
           (r.flags & ~(kTimeValid | kTimeEstimated)) == 0 &&
           std::memchr(r.event, 0, sizeof(r.event)) != nullptr && r.crc == Crc(r);
}
bool Journal::Read(size_t slot, Record* record) {
    return slot < Capacity() && flash_.Read(slot * sizeof(Record), record, sizeof(*record));
}
bool Journal::Init() {
    ready_ = false;
    next_ = 0;
    sequence_ = 0;
    invalid_ = 0;
    clock_flags_ = 0;
    uint64_t clock_sequence = 0;
    if (flash_.Size() < 2 * kSectorBytes || flash_.Size() % kSectorBytes != 0) return false;
    Record sector[kSectorBytes / sizeof(Record)];
    for (size_t offset = 0; offset < flash_.Size(); offset += sizeof(sector)) {
        if (!flash_.Read(offset, sector, sizeof(sector))) return false;
        for (size_t i = 0; i < kSectorBytes / sizeof(Record); ++i) {
            const Record& r = sector[i];
            if (Valid(r)) {
                if (r.flags && r.utc_secs && r.sequence > clock_sequence) {
                    clock_sequence = r.sequence;
                    clock_flags_ = r.flags;
                }
                if (r.sequence > sequence_) {
                    sequence_ = r.sequence;
                    next_ = (offset / sizeof(Record) + i + 1) % Capacity();
                }
            } else if (!Empty(r)) {
                ++invalid_;
            }
        }
    }
    ready_ = true;
    return true;
}
bool Journal::Append(Record r) {
    if (!ready_ || sequence_ == std::numeric_limits<uint64_t>::max()) return false;
    r.magic = kMagic;
    r.sequence = sequence_ + 1;
    r.reserved = 0;
    r.event[sizeof(r.event) - 1] = 0;
    r.crc = Crc(r);
    // Only reclaim the sector at its boundary. A torn tail is skipped, never
    // rewritten, so programming cannot damage earlier committed records.
    for (size_t tries = 0; tries < Capacity(); ++tries) {
        const size_t offset = next_ * sizeof(Record);
        if (offset % kSectorBytes == 0) {
            if (!flash_.Erase(offset, kSectorBytes)) return false;
        } else {
            Record previous;
            if (!Read(next_, &previous)) return false;
            if (!Empty(previous)) {
                next_ = (next_ + 1) % Capacity();
                continue;
            }
        }
        // The commit marker is programmed last. CRC rejects a torn body or
        // partially programmed commit marker after a power failure.
        const auto* bytes = reinterpret_cast<const uint8_t*>(&r);
        // Reserve the sequence before programming: an I/O error can be
        // reported after bytes reached flash, so retries must not reuse it.
        sequence_ = r.sequence;
        const bool body = flash_.Write(offset + 4, bytes + 4, sizeof(r) - 4);
        const bool committed = body && flash_.Write(offset, bytes, 4);
        next_ = (next_ + 1) % Capacity();
        if (!committed) return false;
        if (r.flags && r.utc_secs) clock_flags_ = r.flags;
        return true;
    }
    return false;
}
}  // namespace event_journal
