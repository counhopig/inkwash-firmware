#pragma once

#include <cstddef>
#include <cstdint>

namespace event_journal {
constexpr size_t kSectorBytes = 4096;
constexpr uint32_t kMagic = 0x494c4731;  // ILG1, format 1
constexpr uint32_t kTimeValid = 1;
constexpr uint32_t kTimeEstimated = 2;
struct Record {
    uint32_t magic;
    uint32_t crc;
    uint64_t sequence;
    uint64_t boot;
    uint64_t utc_secs;
    uint64_t uptime_ms;
    uint32_t flags;
    uint32_t reserved;
    char event[80];
};
static_assert(sizeof(Record) == 128, "fixed flash log format");
bool Valid(const Record& record);

class Flash {
 public:
    virtual ~Flash() = default;
    virtual size_t Size() const = 0;
    virtual bool Read(size_t offset, void* data, size_t size) = 0;
    virtual bool Write(size_t offset, const void* data, size_t size) = 0;
    virtual bool Erase(size_t offset, size_t size) = 0;
};

class Journal {
 public:
    explicit Journal(Flash& flash) : flash_(flash) {}
    bool Init();
    bool Append(Record record);
    bool Read(size_t slot, Record* record);
    size_t Capacity() const { return flash_.Size() / sizeof(Record); }
    size_t Next() const { return next_; }
    uint64_t Sequence() const { return sequence_; }
    uint32_t InvalidRecords() const { return invalid_; }
    uint32_t LastClockFlags() const { return clock_flags_; }
 private:
    Flash& flash_;
    size_t next_ = 0;
    uint64_t sequence_ = 0;
    uint32_t invalid_ = 0;
    uint32_t clock_flags_ = 0;
    bool ready_ = false;
};
}  // namespace event_journal
