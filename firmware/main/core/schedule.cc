#include "core/schedule.h"

#include <algorithm>
#include <climits>
#include <cstdio>

bool Repeat::FiresOn(uint16_t y, uint8_t m, uint8_t d, uint8_t weekday) const {
    switch (kind) {
        case Kind::Daily: return true;
        case Kind::Weekly: return std::find(days.begin(), days.end(), weekday) != days.end();
        case Kind::Monthly: return std::find(days.begin(), days.end(), d) != days.end();
        case Kind::Once: return year == y && month == m && day == d;
    }
    return false;
}

namespace schedule {

bool NextMatchingDay(const Repeat& repeat, CalDate from, uint32_t max_days, CalDate* out) {
    const int64_t from_days = DaysSinceEpoch(from.year, from.month, from.day);
    for (uint32_t offset = 0; offset < max_days; ++offset) {
        const int64_t days = from_days + offset;
        CalDate c;
        DateFromDays(days, &c.year, &c.month, &c.day);
        c.weekday = WeekdayFromDays(days);
        if (repeat.FiresOn(c.year, c.month, c.day, c.weekday)) {
            *out = c;
            return true;
        }
    }
    return false;
}

namespace {

CalDate StartDay(uint8_t hour, uint8_t minute, const DateTime& now) {
    const int64_t occurrence = hour * 60 + minute;
    const int64_t now_minutes = now.hour * 60 + now.minute;
    const int64_t from_days =
        DaysSinceEpoch(now.year, now.month, now.day) + (occurrence > now_minutes ? 0 : 1);
    CalDate c;
    DateFromDays(from_days, &c.year, &c.month, &c.day);
    c.weekday = WeekdayFromDays(from_days);
    return c;
}

}  // namespace

CalDate NextOccurrenceDate(const Repeat& repeat, uint8_t hour, uint8_t minute,
                           const DateTime& now) {
    CalDate found;
    if (NextMatchingDay(repeat, StartDay(hour, minute, now), 370, &found)) {
        return found;
    }
    return CalDate{now.year, now.month, now.day, now.weekday};
}

int64_t DaysUntil(uint16_t year, uint8_t month, uint8_t day, const DateTime& now) {
    return DaysSinceEpoch(year, month, day) - DaysSinceEpoch(now.year, now.month, now.day);
}

int64_t MinutesUntil(const Alarm& alarm, const DateTime& now) {
    const int64_t now_minutes = now.hour * 60 + now.minute;
    const int64_t alarm_minutes = alarm.hour * 60 + alarm.minute;
    switch (alarm.repeat.kind) {
        case Repeat::Kind::Daily: {
            int64_t delta = alarm_minutes - now_minutes;
            return delta < 0 ? delta + 24 * 60 : delta;
        }
        case Repeat::Kind::Weekly:
        case Repeat::Kind::Monthly: {
            CalDate next;
            if (!NextMatchingDay(alarm.repeat, StartDay(alarm.hour, alarm.minute, now), 370,
                                 &next)) {
                return INT64_MAX;
            }
            const int64_t offset = DaysSinceEpoch(next.year, next.month, next.day) -
                                   DaysSinceEpoch(now.year, now.month, now.day);
            return offset * 24 * 60 + (alarm_minutes - now_minutes);
        }
        case Repeat::Kind::Once: {
            const int64_t delta = (DaysSinceEpoch(alarm.repeat.year, alarm.repeat.month,
                                                  alarm.repeat.day) -
                                   DaysSinceEpoch(now.year, now.month, now.day)) *
                                      24 * 60 +
                                  (alarm_minutes - now_minutes);
            return delta < 0 ? INT64_MAX : delta;
        }
    }
    return INT64_MAX;
}

const Alarm* NextDue(const std::vector<Alarm>& alarms, const DateTime& now) {
    const Alarm* best = nullptr;
    int64_t best_minutes = INT64_MAX;
    for (const Alarm& a : alarms) {
        if (!a.enabled) continue;
        const int64_t m = MinutesUntil(a, now);
        if (m == INT64_MAX) continue;
        if (best == nullptr || m < best_minutes) {
            best = &a;
            best_minutes = m;
        }
    }
    return best;
}

bool IsDueNow(const Alarm& alarm, const DateTime& now) {
    return alarm.enabled && alarm.hour == now.hour && alarm.minute == now.minute &&
           alarm.repeat.FiresOn(now.year, now.month, now.day, now.weekday);
}

bool IsExpiredOnce(const Alarm& alarm, const DateTime& now) {
    if (alarm.repeat.kind != Repeat::Kind::Once) return false;
    const int64_t m = MinutesUntil(alarm, now);
    return m == INT64_MAX || m <= 0;
}

int NextAlarmId(const std::vector<Alarm>& alarms) {
    for (int candidate = 0; candidate <= 255; ++candidate) {
        bool used = false;
        for (const Alarm& a : alarms) {
            if (a.id == candidate) {
                used = true;
                break;
            }
        }
        if (!used) return candidate;
    }
    return -1;
}

bool AlarmRegsFor(const Alarm& alarm, const DateTime& now, AlarmRegs* out) {
    AlarmRegs regs{alarm.minute, alarm.hour, -1, -1};
    switch (alarm.repeat.kind) {
        case Repeat::Kind::Daily:
            break;
        case Repeat::Kind::Weekly:
            regs.weekday = NextOccurrenceDate(alarm.repeat, alarm.hour, alarm.minute, now).weekday;
            break;
        case Repeat::Kind::Monthly:
            regs.day = NextOccurrenceDate(alarm.repeat, alarm.hour, alarm.minute, now).day;
            break;
        case Repeat::Kind::Once:
            if (now.year != alarm.repeat.year || now.month != alarm.repeat.month) {
                return false;
            }
            regs.day = alarm.repeat.day;
            break;
    }
    *out = regs;
    return true;
}

int64_t MaintenanceWakeupSecs(const std::vector<Alarm>& alarms, const DateTime& now) {
    const int64_t now_epoch = static_cast<int64_t>(now.ToUnix());
    int64_t best = -1;
    for (const Alarm& a : alarms) {
        if (!a.enabled || a.repeat.kind != Repeat::Kind::Once) continue;
        const Repeat& r = a.repeat;
        const bool later = r.year > now.year || (r.year == now.year && r.month > now.month);
        if (!later || r.month < 1 || r.month > 12) continue;
        DateTime first;
        first.year = r.year;
        first.month = r.month;
        first.day = 1;
        const int64_t target = static_cast<int64_t>(first.ToUnix());
        if (best < 0 || target < best) best = target;
    }
    if (best < 0) return -1;
    const int64_t delay = best - (now_epoch + 60);
    return delay < 1 ? 1 : delay;
}

bool TodoFiresOn(const Todo& todo, uint16_t year, uint8_t month, uint8_t day, uint8_t weekday) {
    if (todo.has_repeat) {
        return todo.repeat.FiresOn(year, month, day, weekday);
    }
    return todo.has_due && todo.due.year == year && todo.due.month == month && todo.due.day == day;
}

bool TodoDueToday(const Todo& todo, const DateTime& now) {
    return TodoFiresOn(todo, now.year, now.month, now.day, now.weekday);
}

std::vector<const Todo*> DueHighImportanceTodos(const std::vector<Todo>& todos,
                                                const DateTime& now) {
    std::vector<const Todo*> due;
    for (const Todo& t : todos) {
        if (!t.done && t.importance == Importance::High && TodoDueToday(t, now)) {
            due.push_back(&t);
        }
    }
    return due;
}

std::string ReminderDateKey(const DateTime& now) {
    char buf[16];
    std::snprintf(buf, sizeof(buf), "%04u%02u%02u", now.year, now.month, now.day);
    return buf;
}

bool ValidDate(uint16_t year, uint8_t month, uint8_t day) {
    if (year < 2000 || year > 2099 || month < 1 || month > 12) return false;
    return day >= 1 && day <= DaysInMonth(year, month);
}

namespace {

void SanitizeRepeat(Repeat* r) {
    switch (r->kind) {
        case Repeat::Kind::Daily:
            break;
        case Repeat::Kind::Weekly:
        case Repeat::Kind::Monthly: {
            const bool weekly = r->kind == Repeat::Kind::Weekly;
            std::vector<uint8_t> kept;
            for (uint8_t d : r->days) {
                if (weekly ? d <= 6 : (d >= 1 && d <= 31)) kept.push_back(d);
            }
            std::sort(kept.begin(), kept.end());
            kept.erase(std::unique(kept.begin(), kept.end()), kept.end());
            r->days = kept;
            break;
        }
        case Repeat::Kind::Once:
            if (!ValidDate(r->year, r->month, r->day)) {
                r->month = std::min<uint8_t>(12, std::max<uint8_t>(1, r->month));
                const uint8_t dim = std::max<uint8_t>(1, DaysInMonth(r->year, r->month));
                r->day = std::min<uint8_t>(dim, std::max<uint8_t>(1, r->day));
            }
            break;
    }
}

}  // namespace

void SanitizeAlarms(std::vector<Alarm>* alarms) {
    for (Alarm& a : *alarms) {
        a.hour = std::min<uint8_t>(a.hour, 23);
        a.minute = std::min<uint8_t>(a.minute, 59);
        SanitizeRepeat(&a.repeat);
    }
}

void SanitizeTodos(std::vector<Todo>* todos) {
    for (Todo& t : *todos) {
        if (t.has_due && !ValidDate(t.due.year, t.due.month, t.due.day)) {
            t.has_due = false;
        }
        if (t.has_repeat) {
            SanitizeRepeat(&t.repeat);
        }
    }
}

std::string ValidateRepeat(const Repeat& r) {
    switch (r.kind) {
        case Repeat::Kind::Daily:
            return "";
        case Repeat::Kind::Weekly:
            if (r.days.empty()) return "weekly days must be non-empty and within 0..=6";
            for (uint8_t d : r.days) {
                if (d > 6) return "weekly days must be non-empty and within 0..=6";
            }
            return "";
        case Repeat::Kind::Monthly:
            if (r.days.empty()) return "monthly days must be non-empty and within 1..=31";
            for (uint8_t d : r.days) {
                if (d < 1 || d > 31) return "monthly days must be non-empty and within 1..=31";
            }
            return "";
        case Repeat::Kind::Once:
            return ValidDate(r.year, r.month, r.day) ? "" : "invalid date";
    }
    return "";
}

}  // namespace schedule
