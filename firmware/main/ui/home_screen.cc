#include "ui/home_screen.h"

#include <cstdio>

#include "fonts.h"
#include "ui/widgets.h"

namespace ui {
namespace {

constexpr int kWidth = 400;
constexpr const char* kMonthNames[12] = {"Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                         "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"};
constexpr const char* kWeekdays[7] = {"SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"};

constexpr int kCardTop = 151;
constexpr int kCardH = 133;
constexpr int kCardW = 176;
constexpr int kValueMaxWidth = 152;
constexpr int kCaptionY = 98;
constexpr int kRightX = 16 + kCardW + 16;

const assets::Icon& BatteryIcon(const HomeModel& m) {
    const int percent = m.battery_percent < 0 ? 0 : m.battery_percent;
    if (m.charge.charging) {
        return percent < 34 ? assets::kChargingLow
             : percent < 67 ? assets::kChargingMedium
                            : assets::kChargingHigh;
    }
    if (m.charge.full) {
        return assets::kBatteryFull;
    }
    return percent < 10 ? assets::kBatteryOutline
         : percent < 40 ? assets::kBatteryLow
         : percent < 70 ? assets::kBatteryMedium
         : percent < 95 ? assets::kBatteryHigh
                        : assets::kBatteryFull;
}

// draw_value_centered: the largest scale up to 3 that fits, centred.
void SetValue(lv_obj_t* label, int x, int y, const char* text) {
    const int scale = fonts::FitScale(text, kValueMaxWidth, 3);
    const int width = fonts::PropWidth(text, scale);
    SetText(label, x + (kValueMaxWidth - width) / 2, y, scale, text);
}

bool SameStatus(const HomeModel& a, const HomeModel& b) {
    return a.unread_inbox == b.unread_inbox && a.wifi_configured == b.wifi_configured &&
           &BatteryIcon(a) == &BatteryIcon(b);
}

}  // namespace

HomeScreen::HomeScreen() : screen_(Page()) {
    // Header: logo mark, brand and rule.
    StrokeRect(screen_, 16, 9, 14, 14, 2);
    FillRect(screen_, 21, 14, 4, 4);
    Text(screen_, 38, 8, 1, "INKWASH");
    FillRect(screen_, 16, 29, kWidth - 32, 1);
    status_ = Bare(screen_);
    lv_obj_set_pos(status_, 0, 0);
    lv_obj_set_size(status_, kWidth, 29);

    time_ = Text(screen_, 16, 46, 5, "");
    month_day_ = Text(screen_, 0, 59, 2, "");
    year_weekday_ = Text(screen_, 0, 97, 1, "");

    // NEXT ALARM card.
    StrokeRect(screen_, 16, kCardTop, kCardW, kCardH, 2);
    FillRect(screen_, 16, kCardTop, 5, kCardH);
    Text(screen_, 32, kCardTop + 14, 1, "NEXT ALARM");
    FillRect(screen_, 22, kCardTop + 33, kCardW - 7, 1);
    alarm_value_ = Text(screen_, 32, kCardTop + 44, 3, "");
    alarm_caption_ = Text(screen_, 32, kCardTop + kCaptionY, 1, "");

    // OPEN TODOS card.
    StrokeRect(screen_, kRightX, kCardTop, kCardW, kCardH, 2);
    FillRect(screen_, kRightX, kCardTop, 5, kCardH);
    Text(screen_, kRightX + 16, kCardTop + 14, 1, "OPEN TODOS");
    FillRect(screen_, kRightX + 6, kCardTop + 33, kCardW - 7, 1);
    todo_value_ = Text(screen_, kRightX + 16, kCardTop + 44, 3, "");
    todo_caption_ = Text(screen_, kRightX + 16, kCardTop + kCaptionY, 1, "");
}

void HomeScreen::Show(const HomeModel& model) {
    if (first_ || !SameStatus(model, shown_)) {
        ShowStatus(model);
    }
    ShowClock(model);
    ShowCards(model);
    shown_ = model;
    first_ = false;
}

void HomeScreen::ShowStatus(const HomeModel& model) {
    lv_obj_clean(status_);
    constexpr int kClusterGap = 8;
    const assets::Icon& battery = BatteryIcon(model);
    const int battery_x = kWidth - (battery.width + 16);
    fonts::CreateIcon(status_, battery, battery_x, 7);
    int cursor_x = battery_x;
    if (model.wifi_configured) {
        cursor_x -= kClusterGap + assets::kWifi.width;
        const int wifi_y = 7 + battery.height - assets::kWifi.height;
        fonts::CreateIcon(status_, assets::kWifi, cursor_x, wifi_y);
    }
    if (model.unread_inbox > 0) {
        char label[8];
        if (model.unread_inbox > 99) {
            std::snprintf(label, sizeof(label), "99+");
        } else {
            std::snprintf(label, sizeof(label), "%d", model.unread_inbox);
        }
        const int box_w = fonts::PropWidth(label, 1) + 12;
        const int box_x = cursor_x - (kClusterGap + box_w);
        StrokeRect(status_, box_x, 5, box_w, 20, 2);
        Text(status_, box_x + 6, 7, 1, label);
    }
}

void HomeScreen::ShowClock(const HomeModel& model) {
    if (!model.have_clock) {
        const int w = fonts::PropWidth("--:--", 3);
        SetText(time_, (kWidth - w) / 2, 52, 3, "--:--");
        lv_label_set_text(month_day_, "");
        lv_label_set_text(year_weekday_, "");
        return;
    }
    const DateTime& dt = model.clock;
    char time[8];
    std::snprintf(time, sizeof(time), "%02u:%02u", dt.hour, dt.minute);
    SetText(time_, 16, 46, 5, time);

    char md[16];
    std::snprintf(md, sizeof(md), "%s %u", kMonthNames[(dt.month - 1) % 12], dt.day);
    char yw[24];
    std::snprintf(yw, sizeof(yw), "%u \xC2\xB7 %s", dt.year, kWeekdays[dt.weekday % 7]);
    SetText(month_day_, kWidth - (fonts::PropWidth(md, 2) + 16), 59, 2, md);
    SetText(year_weekday_, kWidth - (fonts::PropWidth(yw, 1) + 16), 97, 1, yw);
}

void HomeScreen::ShowCards(const HomeModel& model) {
    char caption[48];
    if (model.have_next_alarm) {
        SetValue(alarm_value_, 32, kCardTop + 44, model.next_alarm_time.c_str());
        if (model.next_alarm_date.empty()) {
            std::snprintf(caption, sizeof(caption), "EVERY DAY");
        } else if (model.next_alarm_days_left == 0) {
            std::snprintf(caption, sizeof(caption), "TODAY");
        } else {
            std::snprintf(caption, sizeof(caption), "NEXT %s  D+%d",
                          model.next_alarm_date.c_str(), model.next_alarm_days_left);
        }
        const int w = fonts::PropWidth(caption, 1);
        SetText(alarm_caption_, 32 + (kValueMaxWidth - w) / 2, kCardTop + kCaptionY, 1, caption);
    } else {
        SetValue(alarm_value_, 32, kCardTop + 44, "NONE");
        const char* none = "NO ALARMS SET";
        const int w = fonts::PropWidth(none, 1);
        SetText(alarm_caption_, 16 + kCardW / 2 - w / 2, kCardTop + kCaptionY, 1, none);
    }

    char count[12];
    std::snprintf(count, sizeof(count), "%d", model.todo_pending);
    SetValue(todo_value_, kRightX + 16, kCardTop + 44, count);
    std::snprintf(caption, sizeof(caption), "DUE TODAY %d", model.todo_due_today);
    const int w = fonts::PropWidth(caption, 1);
    SetText(todo_caption_, kRightX + 16 + (kValueMaxWidth - w) / 2, kCardTop + kCaptionY, 1,
            caption);
}

}  // namespace ui
