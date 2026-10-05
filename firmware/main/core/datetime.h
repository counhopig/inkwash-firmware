// Calendar arithmetic. The RTC keeps local time; "unix" values here are
// local wall-clock seconds since 1970-01-01 00:00 of that local calendar.
#pragma once

#include <cstdint>

struct DateTime {
    uint16_t year = 1970;
    uint8_t month = 1;
    uint8_t day = 1;
    uint8_t weekday = 4;  // 0 = Sunday; 1970-01-01 was a Thursday
    uint8_t hour = 0;
    uint8_t minute = 0;
    uint8_t second = 0;
    bool voltage_low = false;  // the RTC lost power; the time is not trustworthy

    static DateTime FromUnix(uint64_t epoch);
    uint64_t ToUnix() const;
    DateTime ShiftedMinutes(int32_t minutes) const;
    bool SameMinute(const DateTime& other) const {
        return year == other.year && month == other.month && day == other.day &&
               hour == other.hour && minute == other.minute;
    }
};

bool IsLeap(int64_t year);
uint8_t DaysInMonth(uint16_t year, uint8_t month);
int64_t DaysSinceEpoch(uint16_t year, uint8_t month, uint8_t day);
void DateFromDays(int64_t days, uint16_t* year, uint8_t* month, uint8_t* day);
uint8_t WeekdayFromDays(int64_t days);
uint8_t WeekdayOf(uint16_t year, uint8_t month, uint8_t day);
