// Home: clock, next alarm, open todos, inbox badge, Wi-Fi and battery.
// Same layout as the Rust firmware's home.rs.
#pragma once

#include <string>

#include "board.h"
#include "lvgl.h"
#include "pcf8563.h"

namespace ui {

struct HomeModel {
    bool have_clock = false;
    DateTime clock;
    bool have_next_alarm = false;
    std::string next_alarm_time;  // "07:30"
    std::string next_alarm_date;  // empty for a daily alarm
    int next_alarm_days_left = 0;
    int todo_pending = 0;
    int todo_due_today = 0;
    int unread_inbox = 0;
    bool wifi_configured = false;
    int battery_percent = -1;
    board::Charge charge;
};

class HomeScreen {
public:
    HomeScreen();
    lv_obj_t* Screen() const { return screen_; }
    // Updates only what changed, so the panel refreshes only those areas.
    void Show(const HomeModel& model);

private:
    void ShowStatus(const HomeModel& model);
    void ShowClock(const HomeModel& model);
    void ShowCards(const HomeModel& model);

    lv_obj_t* screen_;
    lv_obj_t* status_;  // battery icon, Wi-Fi icon and inbox badge, rebuilt together
    lv_obj_t* time_;
    lv_obj_t* month_day_;
    lv_obj_t* year_weekday_;
    lv_obj_t* alarm_value_;
    lv_obj_t* alarm_caption_;
    lv_obj_t* todo_value_;
    lv_obj_t* todo_caption_;
    HomeModel shown_;
    bool first_ = true;
};

}  // namespace ui
