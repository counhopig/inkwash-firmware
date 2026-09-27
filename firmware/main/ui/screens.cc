#include "ui/screens.h"

#include <algorithm>
#include <cstdio>
#include <cstdarg>
#include <cstring>

#include "core/schedule.h"
#include "fonts.h"
#include "ui/widgets.h"

#ifndef INKWASH_VERSION
#define INKWASH_VERSION "0.0.0"
#endif
#ifndef INKWASH_GIT_REV
#define INKWASH_GIT_REV "unknown"
#endif

namespace ui {
namespace {

constexpr int kWidth = 400;
constexpr int kListFirstRowY = 39;
constexpr int kListRowHeight = 37;
constexpr size_t kMaxListedItems = 7;
constexpr int kListTextX = 50;

constexpr const char* kMonthNames[12] = {"Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                         "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"};
constexpr const char* kMonthUpper[12] = {"JAN", "FEB", "MAR", "APR", "MAY", "JUN",
                                         "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"};
constexpr const char* kWeekdays[7] = {"SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"};
constexpr const char* kWeekdayShort[7] = {"SU", "MO", "TU", "WE", "TH", "FR", "SA"};
constexpr const char* kNavLabels[kNavDestinations] = {"HOME", "CALENDAR", "INBOX", "ALARMS", "TODOS", "SETTINGS"};
constexpr const char* kSettingsItems[4] = {"SYNC NOW", "SYNC INTERVAL", "BLE PAIRING", "ABOUT"};
constexpr const char* kSyncIntervalItems[5] = {"1 MIN", "5 MIN", "10 MIN", "30 MIN", "60 MIN"};

std::string Fmt(const char* fmt, ...) __attribute__((format(printf, 1, 2)));
std::string Fmt(const char* fmt, ...) {
    char buf[160];
    va_list args;
    va_start(args, fmt);
    std::vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);
    return buf;
}

const char* WeekdayShort(uint8_t d) { return d < 7 ? kWeekdayShort[d] : "??"; }
const char* MonthUpper(uint8_t m) { return m >= 1 && m <= 12 ? kMonthUpper[m - 1] : "???"; }

std::string Join(const std::vector<std::string>& parts, const char* sep) {
    std::string out;
    for (size_t i = 0; i < parts.size(); ++i) {
        if (i) out += sep;
        out += parts[i];
    }
    return out;
}

void Header(lv_obj_t* s, const std::string& title) {
    StrokeRect(s, 16, 9, 14, 14, 2);
    FillRect(s, 21, 14, 4, 4);
    Text(s, 38, 8, 1, "INKWASH");
    Text(s, 384 - fonts::PropWidth(title, 1), 8, 1, title);
    FillRect(s, 16, 29, 368, 1);
}

struct Window {
    size_t first, last, selected;
};

// logic/src/list_window.rs
Window ListWindow(size_t count, size_t selected) {
    if (count == 0) return {0, 0, 0};
    selected = std::min(selected, count - 1);
    const size_t max_first = count > kMaxListedItems ? count - kMaxListedItems : 0;
    const size_t candidate = selected >= kMaxListedItems - 1 ? selected - (kMaxListedItems - 1) : 0;
    const size_t first = std::min(candidate, max_first);
    return {first, std::min(first + kMaxListedItems, count), selected};
}

const assets::Icon& BatteryIcon(const HomeModel& m) {
    const int p = m.battery_percent < 0 ? 0 : m.battery_percent;
    if (m.charge.charging) {
        return p < 34 ? assets::kChargingLow : p < 67 ? assets::kChargingMedium : assets::kChargingHigh;
    }
    if (m.charge.full) return assets::kBatteryFull;
    return p < 10 ? assets::kBatteryOutline : p < 40 ? assets::kBatteryLow
         : p < 70 ? assets::kBatteryMedium : p < 95 ? assets::kBatteryHigh : assets::kBatteryFull;
}

void ValueCentered(lv_obj_t* s, int x, int y, int max_width, const std::string& text) {
    const int scale = fonts::FitScale(text, max_width, 3);
    Text(s, x + (max_width - fonts::PropWidth(text, scale)) / 2, y, scale, text);
}

std::string FormatAlarmRow(const Alarm& a) {
    std::string when;
    const Repeat& r = a.repeat;
    switch (r.kind) {
        case Repeat::Kind::Daily: when = Fmt("%02u:%02u DAILY", a.hour, a.minute); break;
        case Repeat::Kind::Weekly: {
            std::vector<std::string> days;
            for (uint8_t d : r.days) days.push_back(WeekdayShort(d));
            when = Fmt("%02u:%02u ", a.hour, a.minute) + Join(days, ",");
            break;
        }
        case Repeat::Kind::Monthly: {
            std::vector<std::string> days;
            for (uint8_t d : r.days) days.push_back(std::to_string(d));
            when = Fmt("%02u:%02u DAY ", a.hour, a.minute) + Join(days, ",");
            break;
        }
        case Repeat::Kind::Once:
            when = Fmt("%02u:%02u %02u/%02u", a.hour, a.minute, r.month, r.day);
            break;
    }
    std::string row = std::string(a.enabled ? "[X] " : "[ ] ") + when;
    if (!a.label.empty()) row += " " + a.label;
    return fonts::TruncateProp(row, kListTextMaxWidth);
}

std::string FormatTodoRow(const Todo& t, bool have_now, const DateTime& now) {
    const char* imp = t.importance == Importance::Low ? "" : t.importance == Importance::Medium ? "! " : "!! ";
    std::string row = std::string(t.done ? "[X] " : "[ ] ") + imp + t.text;
    if (!t.done) {
        if (have_now && schedule::TodoDueToday(t, now)) {
            row += " - DUE TODAY";
        } else if (t.has_due) {
            row += Fmt(" - %02u/%02u", t.due.month, t.due.day);
        } else if (t.has_repeat && t.repeat.kind == Repeat::Kind::Weekly) {
            std::vector<std::string> days;
            for (uint8_t d : t.repeat.days) days.push_back(WeekdayShort(d));
            row += " - " + Join(days, ",");
        }
    }
    return fonts::TruncateProp(row, kListTextMaxWidth);
}

}  // namespace

// ---- Home ----------------------------------------------------------------

namespace {
struct PackedHome {
    uint8_t version;
    uint8_t have_clock, have_next_alarm, wifi_configured;
    uint16_t year;
    uint8_t month, day, weekday, hour, minute;
    int8_t battery_percent;
    uint8_t charging, full, power_present, pad;
    int32_t next_alarm_days_left;
    int16_t todo_pending, todo_due_today, unread_inbox, pad2;
    char next_alarm_time[8];
    char next_alarm_date[8];
};
}  // namespace

bool PackHome(const HomeModel& m, uint8_t* out, size_t len) {
    if (len < sizeof(PackedHome) || m.next_alarm_time.size() >= 8 || m.next_alarm_date.size() >= 8) {
        return false;
    }
    PackedHome p = {};
    p.version = 1;
    p.have_clock = m.have_clock;
    p.have_next_alarm = m.have_next_alarm;
    p.wifi_configured = m.wifi_configured;
    p.year = m.clock.year;
    p.month = m.clock.month;
    p.day = m.clock.day;
    p.weekday = m.clock.weekday;
    p.hour = m.clock.hour;
    p.minute = m.clock.minute;
    p.battery_percent = static_cast<int8_t>(m.battery_percent);
    p.charging = m.charge.charging;
    p.full = m.charge.full;
    p.power_present = m.charge.power_present;
    p.next_alarm_days_left = m.next_alarm_days_left;
    p.todo_pending = static_cast<int16_t>(m.todo_pending);
    p.todo_due_today = static_cast<int16_t>(m.todo_due_today);
    p.unread_inbox = static_cast<int16_t>(m.unread_inbox);
    std::strcpy(p.next_alarm_time, m.next_alarm_time.c_str());
    std::strcpy(p.next_alarm_date, m.next_alarm_date.c_str());
    std::memcpy(out, &p, sizeof(p));
    return true;
}

bool UnpackHome(const uint8_t* in, size_t len, HomeModel* m) {
    PackedHome p;
    if (len < sizeof(p)) return false;
    std::memcpy(&p, in, sizeof(p));
    if (p.version != 1) return false;
    p.next_alarm_time[7] = p.next_alarm_date[7] = '\0';
    HomeModel h;
    h.have_clock = p.have_clock;
    h.have_next_alarm = p.have_next_alarm;
    h.wifi_configured = p.wifi_configured;
    h.clock.year = p.year;
    h.clock.month = p.month;
    h.clock.day = p.day;
    h.clock.weekday = p.weekday;
    h.clock.hour = p.hour;
    h.clock.minute = p.minute;
    h.battery_percent = p.battery_percent;
    h.charge.charging = p.charging;
    h.charge.full = p.full;
    h.charge.power_present = p.power_present;
    h.next_alarm_days_left = p.next_alarm_days_left;
    h.todo_pending = p.todo_pending;
    h.todo_due_today = p.todo_due_today;
    h.unread_inbox = p.unread_inbox;
    h.next_alarm_time = p.next_alarm_time;
    h.next_alarm_date = p.next_alarm_date;
    *m = h;
    return true;
}

HomeModel BuildHome(const std::vector<Alarm>& alarms, const std::vector<Todo>& todos,
                    const std::vector<InboxItem>& inbox, bool have_clock, const DateTime& now,
                    bool wifi_configured, int battery_percent, board::Charge charge) {
    HomeModel m;
    m.have_clock = have_clock;
    m.clock = now;
    m.wifi_configured = wifi_configured;
    m.battery_percent = battery_percent;
    m.charge = charge;
    if (have_clock) {
        if (const Alarm* a = schedule::NextDue(alarms, now)) {
            m.have_next_alarm = true;
            m.next_alarm_time = Fmt("%02u:%02u", a->hour, a->minute);
            if (a->repeat.kind == Repeat::Kind::Once) {
                m.next_alarm_date = Fmt("%02u/%02u", a->repeat.month, a->repeat.day);
                m.next_alarm_days_left = static_cast<int>(
                    schedule::DaysUntil(a->repeat.year, a->repeat.month, a->repeat.day, now));
            } else if (a->repeat.kind != Repeat::Kind::Daily) {
                const auto d = schedule::NextOccurrenceDate(a->repeat, a->hour, a->minute, now);
                m.next_alarm_date = Fmt("%02u/%02u", d.month, d.day);
                m.next_alarm_days_left = static_cast<int>(schedule::DaysUntil(d.year, d.month, d.day, now));
            }
        }
    }
    for (const Todo& t : todos) {
        if (t.done) continue;
        ++m.todo_pending;
        if (have_clock && schedule::TodoDueToday(t, now)) ++m.todo_due_today;
    }
    for (const InboxItem& it : inbox) {
        if (!it.read) ++m.unread_inbox;
    }
    return m;
}

void DrawHome(lv_obj_t* s, const HomeModel& m) {
    StrokeRect(s, 16, 9, 14, 14, 2);
    FillRect(s, 21, 14, 4, 4);
    Text(s, 38, 8, 1, "INKWASH");

    constexpr int kClusterGap = 8;
    const assets::Icon& battery = BatteryIcon(m);
    const int battery_x = kWidth - (battery.width + 16);
    fonts::CreateIcon(s, battery, battery_x, 7);
    int cursor_x = battery_x;
    if (m.wifi_configured) {
        cursor_x -= kClusterGap + assets::kWifi.width;
        fonts::CreateIcon(s, assets::kWifi, cursor_x, 7 + battery.height - assets::kWifi.height);
    }
    if (m.unread_inbox > 0) {
        const std::string label = m.unread_inbox > 99 ? "99+" : std::to_string(m.unread_inbox);
        const int box_w = fonts::PropWidth(label, 1) + 12;
        const int box_x = cursor_x - (kClusterGap + box_w);
        StrokeRect(s, box_x, 5, box_w, 20, 2);
        Text(s, box_x + 6, 7, 1, label);
    }
    FillRect(s, 16, 29, kWidth - 32, 1);

    if (m.have_clock) {
        const DateTime& dt = m.clock;
        Text(s, 16, 46, 5, Fmt("%02u:%02u", dt.hour, dt.minute));
        const std::string md = Fmt("%s %u", kMonthNames[(dt.month + 11) % 12], dt.day);
        const std::string yw = Fmt("%u \xC2\xB7 %s", dt.year, kWeekdays[dt.weekday % 7]);
        Text(s, kWidth - (fonts::PropWidth(md, 2) + 16), 59, 2, md);
        Text(s, kWidth - (fonts::PropWidth(yw, 1) + 16), 97, 1, yw);
    } else {
        Text(s, (kWidth - fonts::PropWidth("--:--", 3)) / 2, 52, 3, "--:--");
    }

    constexpr int kCardTop = 151, kCardH = 133, kCardW = 176, kValueMax = 152, kCaptionY = 98;
    StrokeRect(s, 16, kCardTop, kCardW, kCardH, 2);
    FillRect(s, 16, kCardTop, 5, kCardH);
    Text(s, 32, kCardTop + 14, 1, "NEXT ALARM");
    FillRect(s, 22, kCardTop + 33, kCardW - 7, 1);
    if (m.have_next_alarm) {
        ValueCentered(s, 32, kCardTop + 44, kValueMax, m.next_alarm_time);
        std::string caption;
        if (m.next_alarm_date.empty()) caption = "EVERY DAY";
        else if (m.next_alarm_days_left == 0) caption = "TODAY";
        else if (m.next_alarm_days_left > 0)
            caption = "NEXT " + m.next_alarm_date + Fmt("  D+%d", m.next_alarm_days_left);
        else caption = "EVERY DAY";
        Text(s, 32 + (kValueMax - fonts::PropWidth(caption, 1)) / 2, kCardTop + kCaptionY, 1, caption);
    } else {
        ValueCentered(s, 32, kCardTop + 44, kValueMax, "NONE");
        const char* none = "NO ALARMS SET";
        Text(s, 16 + kCardW / 2 - fonts::PropWidth(none, 1) / 2, kCardTop + kCaptionY, 1, none);
    }

    const int right_x = 16 + kCardW + 16;
    StrokeRect(s, right_x, kCardTop, kCardW, kCardH, 2);
    FillRect(s, right_x, kCardTop, 5, kCardH);
    Text(s, right_x + 16, kCardTop + 14, 1, "OPEN TODOS");
    FillRect(s, right_x + 6, kCardTop + 33, kCardW - 7, 1);
    ValueCentered(s, right_x + 16, kCardTop + 44, kValueMax, std::to_string(m.todo_pending));
    const std::string due = Fmt("DUE TODAY %d", m.todo_due_today);
    Text(s, right_x + 16 + (kValueMax - fonts::PropWidth(due, 1)) / 2, kCardTop + kCaptionY, 1, due);
}

// ---- Lists ----------------------------------------------------------------

void DrawList(lv_obj_t* s, const std::string& title, const std::vector<std::string>& items,
              size_t selected) {
    Header(s, title);
    const Window w = ListWindow(items.size(), selected);
    int y = kListFirstRowY;
    for (size_t i = w.first; i < w.last; ++i) {
        if (i == w.selected) {
            StrokeRect(s, 16, y, 368, kListRowHeight - 2, 2);
            FillRect(s, 16, y, 5, kListRowHeight - 2);
        }
        Text(s, kListTextX, y + 10, 1, items[i]);
        y += kListRowHeight;
    }
}

void DrawSettings(lv_obj_t* s, size_t selected) {
    DrawList(s, "SETTINGS", std::vector<std::string>(kSettingsItems, kSettingsItems + 4), selected);
}

void DrawSyncInterval(lv_obj_t* s, size_t selected) {
    DrawList(s, "SYNC INTERVAL", std::vector<std::string>(kSyncIntervalItems, kSyncIntervalItems + 5),
             selected);
}

void DrawAbout(lv_obj_t* s) {
    Header(s, "ABOUT");
    Text(s, 24, 64, 2, "INKWASH NOTE 4");
    Text(s, 24, 112, 1, "FIRMWARE " INKWASH_VERSION);
    Text(s, 24, 144, 1, INKWASH_GIT_REV);
}

void DrawAlarmList(lv_obj_t* s, const std::vector<Alarm>& alarms, size_t selected) {
    std::vector<std::string> items;
    for (const Alarm& a : alarms) items.push_back(FormatAlarmRow(a));
    items.push_back("+ ADD ALARM");
    DrawList(s, "ALARMS", items, selected);
}

void DrawNumberPick(lv_obj_t* s, bool minute_stage, int value) {
    Header(s, minute_stage ? "NEW ALARM - MINUTE" : "NEW ALARM - HOUR");
    const std::string label = Fmt("%02d", value);
    const int number_w = fonts::PropWidth(label, 5);
    const int box_w = number_w + 64;
    const int box_x = 200 - box_w / 2;
    constexpr int kBoxTop = 123, kBoxH = 120;
    Text(s, (400 - fonts::PropWidth("CHOOSE VALUE", 1)) / 2, 87, 1, "CHOOSE VALUE");
    StrokeRect(s, box_x, kBoxTop, box_w, kBoxH, 3);
    FillRect(s, box_x, kBoxTop, 7, kBoxH);
    Text(s, box_x + 7 + (box_w - 7 - number_w) / 2, kBoxTop + (kBoxH - 80) / 2, 5, label);
}

void DrawTodoList(lv_obj_t* s, const std::vector<Todo>& todos, size_t selected, bool have_now,
                  const DateTime& now) {
    std::vector<std::string> items;
    for (const Todo& t : todos) items.push_back(FormatTodoRow(t, have_now, now));
    DrawList(s, "TODOS", items, selected);
}

void DrawInboxList(lv_obj_t* s, const std::vector<InboxItem>& items, size_t selected) {
    std::vector<std::string> rows;
    for (const InboxItem& it : items) {
        // "•" read, "○" unread, as the Rust firmware.
        rows.push_back(fonts::TruncateProp((it.read ? "\xE2\x80\xA2 " : "\xE2\x97\x8B ") + it.title,
                                           kListTextMaxWidth));
    }
    if (rows.empty()) rows.push_back("NO MESSAGES");
    DrawList(s, "INBOX", rows, selected);
}

void DrawInboxItem(lv_obj_t* s, const std::vector<InboxItem>& items, size_t index) {
    Header(s, "INBOX");
    if (index >= items.size()) return;
    const InboxItem& item = items[index];
    const std::string title = fonts::TruncateProp(item.title, 340);
    int rule_y = 74, body_y = 82;
    if (fonts::PropWidth(title, 2) <= 368) {
        Text(s, 16, 40, 2, title);
    } else {
        const auto lines = fonts::WrapProp(title, 368);
        for (size_t i = 0; i < lines.size() && i < 2; ++i) Text(s, 16, 38 + static_cast<int>(i) * 22, 1, lines[i]);
        rule_y = 92;
        body_y = 100;
    }
    FillRect(s, 16, rule_y, 368, 1);
    int y = body_y;
    for (const std::string& line : fonts::WrapProp(item.body, 368)) {
        if (y + 16 > 282) break;
        Text(s, 16, y, 1, line);
        y += 18;
    }
}

// ---- Calendar -------------------------------------------------------------

void DrawCalendar(lv_obj_t* s, uint16_t year, uint8_t month, uint8_t selected_day,
                  bool have_now, const DateTime& now, const std::vector<Todo>& todos) {
    Header(s, "CALENDAR");
    if (!have_now) return;
    const uint8_t dim = DaysInMonth(year, month);
    int marks[32];  // highest importance due that day, -1 for none
    std::fill(std::begin(marks), std::end(marks), -1);
    for (const Todo& t : todos) {
        for (uint8_t d = 1; d <= dim; ++d) {
            if (schedule::TodoFiresOn(t, year, month, d, WeekdayOf(year, month, d))) {
                marks[d] = std::max(marks[d], static_cast<int>(t.importance));
            }
        }
    }
    const uint8_t selected = std::max<uint8_t>(1, std::min(selected_day, dim));
    Text(s, 16, 38, 2, Fmt("%04u / %02u", year, month));
    constexpr int kColW = 53, kRowH = 32, kOriginX = 18, kOriginY = 75, kMarkerY = 18;
    for (int i = 0; i < 7; ++i) Text(s, kOriginX + i * kColW, kOriginY, 1, kWeekdayShort[i]);
    FillRect(s, 16, 99, 368, 1);
    int col = WeekdayOf(year, month, 1);
    int row = 1;
    for (uint8_t day = 1; day <= dim; ++day) {
        const int x = kOriginX + col * kColW;
        const int y = kOriginY + row * kRowH;
        const std::string text = std::to_string(day);
        if (day == selected) {
            StrokeRect(s, x - 6, y - 4, 34, 30, 2);
            FillRect(s, x - 6, y - 4, 4, 30);
        }
        Text(s, x, y, 1, text);
        if (now.year == year && now.month == month && now.day == day) {
            FillRect(s, x, y + 16, fonts::PropWidth(text, 1), 1);
        }
        if (marks[day] >= 0) {
            const int size = marks[day] == static_cast<int>(Importance::High) ? 6 : 4;
            FillRect(s, x, y + kMarkerY, size, size);
        }
        if (++col > 6) {
            col = 0;
            ++row;
        }
    }
}

void DrawWeek(lv_obj_t* s, const std::vector<Todo>& todos, uint16_t year, uint8_t month,
              uint8_t day, bool have_now, const DateTime& now) {
    const int64_t start = DaysSinceEpoch(year, month, day) - WeekdayOf(year, month, day);
    uint16_t sy, ey;
    uint8_t sm, sd, em, ed;
    DateFromDays(start, &sy, &sm, &sd);
    DateFromDays(start + 6, &ey, &em, &ed);
    const std::string title = sm == em ? Fmt("%s %u-%u", MonthUpper(sm), sd, ed)
                                       : Fmt("%s %u - %s %u", MonthUpper(sm), sd, MonthUpper(em), ed);
    Header(s, title);
    constexpr int kOriginX = 16, kColW = 50, kColGap = 3, kCardTop = 38, kCardH = 40;
    constexpr int kWeekdayY = 43, kDateY = 56, kListTop = 88, kLineH = 8, kItemGap = 5, kBottom = 296;
    constexpr size_t kMaxTodoLines = 3;
    constexpr int kTextInset = 8;
    const int opened = static_cast<int>(DaysSinceEpoch(year, month, day) - start);
    for (int i = 0; i < 7; ++i) {
        uint16_t y;
        uint8_t m, d;
        DateFromDays(start + i, &y, &m, &d);
        const uint8_t weekday = WeekdayOf(y, m, d);
        const int x = kOriginX + i * (kColW + kColGap);
        if (i == opened) {
            StrokeRect(s, x, kCardTop, kColW, kCardH, 2);
            FillRect(s, x, kCardTop + kCardH - 4, kColW, 4);
        }
        if (have_now && now.year == y && now.month == m && now.day == d) {
            FillRect(s, x + kColW - 7, kCardTop + 4, 3, 3);
        }
        const char* wd = WeekdayShort(weekday);
        SmallText(s, x + (kColW - fonts::SmallWidth(wd)) / 2, kWeekdayY, wd);
        const std::string date = std::to_string(d);
        Text(s, x + (kColW - fonts::PropWidth(date, 1)) / 2, kDateY, 1, date);

        const int text_w = kColW - (kTextInset + 2);
        int y_cursor = kListTop;
        bool full = false;
        for (const Todo& t : todos) {
            if (full) break;
            if (t.done || !schedule::TodoFiresOn(t, y, m, d, weekday)) continue;
            auto lines = fonts::WrapSmall(t.text, text_w);
            const bool truncated = lines.size() > kMaxTodoLines;
            if (truncated) lines.resize(kMaxTodoLines);
            FillRect(s, x + 1, y_cursor + 2, 3, 3);
            for (size_t li = 0; li < lines.size(); ++li) {
                if (y_cursor + 7 > kBottom) {
                    full = true;
                    break;
                }
                std::string line = lines[li];
                if (truncated && li + 1 == lines.size()) {
                    const int ell = fonts::SmallWidth("...");
                    while (!line.empty() && fonts::SmallWidth(line) + ell > text_w) {
                        size_t cut = line.size() - 1;
                        while (cut > 0 && (static_cast<unsigned char>(line[cut]) & 0xC0) == 0x80) --cut;
                        line.resize(cut);
                    }
                    line += "...";
                }
                SmallText(s, x + kTextInset, y_cursor, line);
                y_cursor += kLineH;
            }
            if (full) break;
            y_cursor += kItemGap;
            if (y_cursor <= kBottom) FillRect(s, x + kTextInset, y_cursor - 2, text_w, 1);
        }
    }
}

// ---- Overlays and single-purpose screens ------------------------------------

void DrawAlarmRinging(lv_obj_t* s) {
    Header(s, "ALARM");
    Text(s, 200 - fonts::PropWidth("ALARM", 4) / 2, 92, 4, "ALARM");
    const char* hint = "ENTER = DISMISS";
    Text(s, 200 - fonts::PropWidth(hint, 1) / 2, 184, 1, hint);
}

void DrawReminder(lv_obj_t* s, bool urgent, const std::vector<std::string>& lines) {
    Header(s, urgent ? "URGENT" : "TODOS DUE");
    const size_t max_rows = urgent ? 4 : 7;
    for (size_t i = 0; i < lines.size() && i < max_rows; ++i) {
        Text(s, 16, 48 + static_cast<int>(i) * 24, 1, "!! " + fonts::TruncateProp(lines[i], 300));
    }
    if (lines.size() > max_rows) {
        Text(s, 16, 48 + static_cast<int>(max_rows) * 24, 1, urgent ? "MORE IN INBOX..." : "MORE...");
    }
}

void DrawBlePairing(lv_obj_t* s, bool have_passkey, uint32_t passkey) {
    Header(s, "BLE PAIRING");
    if (have_passkey) {
        Text(s, 8, 40, 1, "PAIRING CODE:");
        Text(s, 8, 56, 2, Fmt("%06lu", static_cast<unsigned long>(passkey)));
    } else {
        Text(s, 8, 40, 1, "CONNECTING...");
    }
    Text(s, 8, 88, 1, "Service UUID:");
    Text(s, 8, 100, 1, "d2c25e50-");
    Text(s, 8, 112, 1, "5e22-48d8...");
}

void DrawSafeMode(lv_obj_t* s, const std::string& reason) {
    Header(s, "SAFE MODE");
    Text(s, 8, 60, 1, "BOOT LOOP DETECTED");
    const std::vector<std::string> lines = fonts::WrapProp(reason, 368);
    int y = 80;
    for (size_t i = 0; i < lines.size() && i < 2; ++i, y += 16) Text(s, 8, y, 1, lines[i]);
    Text(s, 8, 140, 1, "STORED DATA UNTOUCHED");
    Text(s, 8, 156, 1, "PRESS ENTER TO RETRY");
    Text(s, 8, 172, 1, "USB CONSOLE STAYS AVAILABLE");
}

void DrawNavigationBar(lv_obj_t* s, size_t selected) {
    constexpr int kX = 16, kY = 34, kW = 176, kH = 266, kRowH = 33;
    FillRect(s, kX, kY, kW, kH, false);
    Text(s, 24, 42, 1, "GO TO");
    FillRect(s, 24, 58, kW - 16, 1);
    int y = 64;
    for (size_t i = 0; i < kNavDestinations; ++i) {
        if (i == selected) {
            StrokeRect(s, 22, y, kW - 12, kRowH - 2, 2);
            FillRect(s, 22, y, 5, kRowH - 2);
        }
        Text(s, 30, y + 10, 1, kNavLabels[i]);
        y += kRowH;
    }
    FillRect(s, kX + kW - 3, kY, 3, kH);
}

void DrawNotice(lv_obj_t* s, const std::string& text) {
    constexpr int kX = 0, kY = 262, kW = 400, kH = 38;
    FillRect(s, kX, kY, kW, kH, false);
    StrokeRect(s, kX + 8, kY + 2, kW - 16, kH - 4, 2);
    Text(s, kX + 20, kY + 11, 1, fonts::TruncateProp(text, kW - 40));
}

}  // namespace ui
