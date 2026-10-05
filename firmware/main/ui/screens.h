// Device screens and their pixel layouts.
#pragma once

#include <string>
#include <vector>

#include "board.h"
#include "core/datetime.h"
#include "core/model.h"
#include "lvgl.h"

namespace ui {

constexpr int kListTextMaxWidth = 334;

struct HomeModel {
    bool have_clock = false;
    DateTime clock;
    bool have_next_alarm = false;
    std::string next_alarm_time;  // "07:30"
    std::string next_alarm_date;  // "10/02"; empty for a daily alarm
    int next_alarm_days_left = 0;
    int todo_pending = 0;
    int todo_due_today = 0;
    int unread_inbox = 0;
    bool wifi_connected = false;
    int battery_percent = -1;
    board::Charge charge;
};

// Fixed-size form of HomeModel kept in RTC memory across deep sleep.
bool PackHome(const HomeModel& model, uint8_t* out, size_t len);
bool UnpackHome(const uint8_t* in, size_t len, HomeModel* model);

HomeModel BuildHome(const std::vector<Alarm>& alarms, const std::vector<Todo>& todos,
                    const std::vector<InboxItem>& inbox, bool have_clock, const DateTime& now,
                    bool wifi_connected, int battery_percent, board::Charge charge);

void DrawHome(lv_obj_t* s, const HomeModel& m);
void DrawList(lv_obj_t* s, const std::string& title, const std::vector<std::string>& items,
              size_t selected);
void DrawSettings(lv_obj_t* s, size_t selected);
void DrawSyncInterval(lv_obj_t* s, size_t selected);
void DrawAbout(lv_obj_t* s);
void DrawAlarmList(lv_obj_t* s, const std::vector<Alarm>& alarms, size_t selected);
void DrawNumberPick(lv_obj_t* s, bool minute_stage, int value);
void DrawTodoList(lv_obj_t* s, const std::vector<Todo>& todos, size_t selected, bool have_now,
                  const DateTime& now);
void DrawInboxList(lv_obj_t* s, const std::vector<InboxItem>& items, size_t selected);
void DrawInboxItem(lv_obj_t* s, const std::vector<InboxItem>& items, size_t index);
void DrawCalendar(lv_obj_t* s, uint16_t year, uint8_t month, uint8_t selected_day,
                  bool have_now, const DateTime& now, const std::vector<Todo>& todos);
void DrawWeek(lv_obj_t* s, const std::vector<Todo>& todos, uint16_t year, uint8_t month,
              uint8_t day, bool have_now, const DateTime& now);
void DrawAlarmRinging(lv_obj_t* s);
void DrawReminder(lv_obj_t* s, bool urgent, const std::vector<std::string>& lines);
void DrawBlePairing(lv_obj_t* s, bool have_passkey, uint32_t passkey);
void DrawNavigationBar(lv_obj_t* s, size_t selected);
void DrawNotice(lv_obj_t* s, const std::string& text);

constexpr size_t kSettingsRows = 4;
constexpr size_t kSettingsSyncNow = 0;
constexpr size_t kSettingsSyncInterval = 1;
constexpr size_t kSettingsBlePairing = 2;
constexpr size_t kSettingsAbout = 3;
constexpr uint16_t kSyncIntervals[5] = {1, 5, 10, 30, 60};
constexpr size_t kNavDestinations = 6;

// Minimum safe mode after repeated failed boots.
void DrawSafeMode(lv_obj_t* s, const std::string& reason);

}  // namespace ui
