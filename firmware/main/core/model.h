// Alarm, todo and inbox models shared with the server contract.
#pragma once

#include <cstdint>
#include <string>
#include <vector>

struct Repeat {
    enum class Kind : uint8_t { Daily, Weekly, Monthly, Once };
    Kind kind = Kind::Daily;
    std::vector<uint8_t> days;  // Weekly: 0..6 (Sunday = 0); Monthly: 1..31
    uint16_t year = 0;          // Once
    uint8_t month = 0;
    uint8_t day = 0;

    bool FiresOn(uint16_t y, uint8_t m, uint8_t d, uint8_t weekday) const;
    bool operator==(const Repeat& o) const {
        return kind == o.kind && days == o.days && year == o.year && month == o.month &&
               day == o.day;
    }
};

struct Alarm {
    uint8_t id = 0;
    uint8_t hour = 0;
    uint8_t minute = 0;
    Repeat repeat;
    bool enabled = true;
    std::string label;
};

enum class Importance : uint8_t { Low = 0, Medium = 1, High = 2 };

struct TodoDue {
    uint16_t year = 0;
    uint8_t month = 0;
    uint8_t day = 0;
};

struct Todo {
    uint8_t id = 0;
    std::string text;
    bool done = false;
    Importance importance = Importance::Medium;
    bool has_due = false;
    TodoDue due;
    bool has_repeat = false;
    Repeat repeat;
};

enum class InboxKind : uint8_t { Alert, Event, Info };
enum class Priority : uint8_t { Normal, High };

struct InboxItem {
    uint64_t id = 0;
    InboxKind kind = InboxKind::Info;
    Priority priority = Priority::Normal;
    std::string title;
    std::string body;
    bool has_when = false;
    int64_t when = 0;
    bool read = false;

    bool IsUrgent() const {
        return !read && priority == Priority::High && kind == InboxKind::Alert;
    }
};
