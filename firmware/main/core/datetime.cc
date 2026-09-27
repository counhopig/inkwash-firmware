#include "core/datetime.h"

namespace {

const int64_t* MonthLengths(int64_t year) {
    static const int64_t kLeap[12] = {31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
    static const int64_t kCommon[12] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
    return IsLeap(year) ? kLeap : kCommon;
}

}  // namespace

bool IsLeap(int64_t year) {
    return (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
}

uint8_t DaysInMonth(uint16_t year, uint8_t month) {
    int m = static_cast<int>(month) - 1;
    m = m < 0 ? 0 : m > 11 ? 11 : m;
    return static_cast<uint8_t>(MonthLengths(year)[m]);
}

int64_t DaysSinceEpoch(uint16_t year, uint8_t month, uint8_t day) {
    int64_t days = 0;
    for (int64_t y = 1970; y < year; ++y) {
        days += IsLeap(y) ? 366 : 365;
    }
    const int64_t* lengths = MonthLengths(year);
    for (int m = 0; m + 1 < month && m < 12; ++m) {
        days += lengths[m];
    }
    return days + (day > 0 ? day - 1 : 0);
}

void DateFromDays(int64_t days, uint16_t* year, uint8_t* month, uint8_t* day) {
    int64_t y = 1970;
    while (true) {
        const int64_t dim = IsLeap(y) ? 366 : 365;
        if (days < dim) {
            break;
        }
        days -= dim;
        ++y;
    }
    const int64_t* lengths = MonthLengths(y);
    int m = 0;
    while (m < 11 && days >= lengths[m]) {
        days -= lengths[m];
        ++m;
    }
    *year = static_cast<uint16_t>(y);
    *month = static_cast<uint8_t>(m + 1);
    *day = static_cast<uint8_t>(days + 1);
}

uint8_t WeekdayFromDays(int64_t days) {
    const int64_t w = (days + 4) % 7;
    return static_cast<uint8_t>(w < 0 ? w + 7 : w);
}

uint8_t WeekdayOf(uint16_t year, uint8_t month, uint8_t day) {
    return WeekdayFromDays(DaysSinceEpoch(year, month, day));
}

DateTime DateTime::FromUnix(uint64_t epoch) {
    DateTime dt;
    const uint32_t secs = static_cast<uint32_t>(epoch % 86400);
    const int64_t days = static_cast<int64_t>(epoch / 86400);
    DateFromDays(days, &dt.year, &dt.month, &dt.day);
    dt.hour = static_cast<uint8_t>(secs / 3600);
    dt.minute = static_cast<uint8_t>((secs % 3600) / 60);
    dt.second = static_cast<uint8_t>(secs % 60);
    dt.weekday = WeekdayFromDays(days);
    return dt;
}

uint64_t DateTime::ToUnix() const {
    const uint64_t days = static_cast<uint64_t>(DaysSinceEpoch(year, month, day));
    return days * 86400 + hour * 3600ULL + minute * 60ULL + second;
}

DateTime DateTime::ShiftedMinutes(int32_t minutes) const {
    int64_t shifted = static_cast<int64_t>(ToUnix()) + static_cast<int64_t>(minutes) * 60;
    return FromUnix(shifted < 0 ? 0 : static_cast<uint64_t>(shifted));
}
