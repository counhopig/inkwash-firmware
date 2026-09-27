// Alarm and todo scheduling rules, identical to logic/src/{alarm_schedule,
// reminder_dedup,sanitize,sync_validate}.rs.
#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "core/datetime.h"
#include "core/model.h"

namespace schedule {

struct CalDate {
    uint16_t year;
    uint8_t month;
    uint8_t day;
    uint8_t weekday;
};

// First day in [from, from + max_days) on which `repeat` fires.
bool NextMatchingDay(const Repeat& repeat, CalDate from, uint32_t max_days, CalDate* out);

// Date of the next occurrence strictly after `now` at hour:minute.
CalDate NextOccurrenceDate(const Repeat& repeat, uint8_t hour, uint8_t minute,
                           const DateTime& now);
int64_t DaysUntil(uint16_t year, uint8_t month, uint8_t day, const DateTime& now);

// Minutes from now to the alarm's next ring; INT64_MAX when it never rings again.
int64_t MinutesUntil(const Alarm& alarm, const DateTime& now);

// The enabled alarm that rings next, or nullptr.
const Alarm* NextDue(const std::vector<Alarm>& alarms, const DateTime& now);

bool IsDueNow(const Alarm& alarm, const DateTime& now);
bool IsExpiredOnce(const Alarm& alarm, const DateTime& now);

// Smallest id not in use, or -1 when all 256 are taken.
int NextAlarmId(const std::vector<Alarm>& alarms);

// PCF8563 alarm registers; day/weekday < 0 means "don't care".
struct AlarmRegs {
    uint8_t minute;
    uint8_t hour;
    int day;
    int weekday;
};
bool AlarmRegsFor(const Alarm& alarm, const DateTime& now, AlarmRegs* out);

// Seconds until the RTC must be reprogrammed for a one-shot alarm in a later
// month (the PCF8563 cannot hold a month); -1 when none.
int64_t MaintenanceWakeupSecs(const std::vector<Alarm>& alarms, const DateTime& now);

// Todo helpers.
bool TodoFiresOn(const Todo& todo, uint16_t year, uint8_t month, uint8_t day, uint8_t weekday);
bool TodoDueToday(const Todo& todo, const DateTime& now);
std::vector<const Todo*> DueHighImportanceTodos(const std::vector<Todo>& todos,
                                                const DateTime& now);
std::string ReminderDateKey(const DateTime& now);

// Clamps stored data into range, like logic/src/sanitize.rs.
bool ValidDate(uint16_t year, uint8_t month, uint8_t day);
void SanitizeAlarms(std::vector<Alarm>* alarms);
void SanitizeTodos(std::vector<Todo>* todos);

// Sync response validation; returns an empty string when valid.
std::string ValidateRepeat(const Repeat& repeat);

}  // namespace schedule
