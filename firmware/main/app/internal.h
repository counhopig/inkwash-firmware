#pragma once

#include "app/app.h"

#include <algorithm>
#include <atomic>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

#include "audio/tones.h"
#include "board.h"
#include "control/ble.h"
#include "control/usb_console.h"
#include "core/protocol.h"
#include "core/schedule.h"
#include "core/power_policy.h"
#include "display.h"
#include "diagnostics/event_log.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "fonts.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "keys.h"
#include "net/sync.h"
#include "net/wifi.h"
#include "pcf8563.h"
#include "power/power.h"
#include "storage/store.h"
#include "ui/screens.h"

#ifndef INKWASH_BUILD_EPOCH
#define INKWASH_BUILD_EPOCH 1790000000ULL
#endif


namespace app::detail {

inline constexpr char kTag[] = "app";

// Application deadlines.
inline constexpr int64_t kNoticeMs = 4000;
inline constexpr int64_t kReminderTimeoutMs = 120000;
inline constexpr int64_t kRingAutoSilenceMs = 300000;
inline constexpr int64_t kBlePairingTimeoutMs = 120000;
// A run that stays up this long no longer counts as a failed boot.
inline constexpr int64_t kBootHealthyMs = 30 * 1000;
inline constexpr uint64_t kUrgentPeriodSecs = 30;

int64_t NowMs();

enum class Channel { Usb, Ble };

enum class NetJob { None, Sync, UrgentPoll, VerifyWifi, Ntp };

struct Event {
    enum class Kind { Key, Command, BleLink, NetDone } kind;
    keys::Event key{};
    Channel channel = Channel::Usb;
    std::string line;
    ble::Event ble_event = ble::Event::Connected;
    NetJob job = NetJob::None;
    netsync::Result result;
    bool urgent = false;
};


struct NetRequest {
    NetJob job;
    DateTime now;
    store::WifiCreds creds;
};

enum class ScreenKind {
    Home, Navigation, Settings, About, SyncIntervalPick, AlarmList, AlarmAdd, TodoList,
    Inbox, InboxItem, Calendar, WeekView, AlarmRinging, Reminder, BlePairing,
};

struct Screen {
    ScreenKind kind = ScreenKind::Home;
    size_t selected = 0;
    // Calendar/WeekView date; AlarmAdd: minute_stage, hour, value.
    uint16_t year = 0;
    uint8_t month = 0;
    uint8_t day = 0;
    bool minute_stage = false;
    uint8_t hour = 0;
    int value = 0;
    bool operator==(const Screen& o) const {
        return kind == o.kind && selected == o.selected && year == o.year && month == o.month &&
               day == o.day && minute_stage == o.minute_stage && hour == o.hour && value == o.value;
    }
};

enum class NavOrigin { Home, Settings, AlarmList, TodoList, Inbox, Calendar };


struct PendingReply {
    bool active = false;
    bool has_id = false;
    std::string id;
    protocol::Cmd cmd = protocol::Cmd::GetStatus;
};

struct Notice {
    std::string text;
    int64_t expires_ms = -1;  // -1: until replaced
};

struct State {
    bool clock_estimated = false;
    power::WakeCause wake = power::WakeCause::PowerOn;
    bool background = false;  // timer wake nobody has touched yet

    // Screens.
    Screen screen;
    Screen before_nav;  // underlying screen while Navigation is open
    NavOrigin nav_origin = NavOrigin::Home;
    Screen before_ring;
    Screen before_reminder;
    bool reminder_urgent = false;
    std::vector<std::string> reminder_lines;
    int64_t reminder_deadline_ms = 0;
    bool have_passkey = false;
    uint32_t passkey = 0;
    int64_t ble_deadline_ms = 0;
    bool ble_input_released = false;
    bool ble_finish_pending = false;  // stop BLE after the SetWifi reply went out
    std::string ble_finish_notice;
    std::unique_ptr<Notice> notice;

    // Data.
    std::vector<Alarm> alarms;
    std::vector<Todo> todos;
    std::vector<InboxItem> inbox;
    bool have_clock = false;
    DateTime now;
    bool wifi_configured = false;
    bool wifi_has_password = false;
    std::string wifi_ssid;
    bool server_configured = false;
    std::string server_url;
    bool server_has_token = false;
    bool wifi_connected = false;
    int16_t timezone = 0;
    uint16_t sync_interval = store::kDefaultSyncInterval;

    // Alarm runtime.
    bool ringing = false;
    int64_t ring_deadline_ms = 0;
    uint64_t fired_minute = 0;
    bool programmed_valid = false;
    schedule::AlarmRegs programmed{};
    bool programmed_none = false;

    // Synchronization runtime.
    NetJob net_job = NetJob::None;
    bool manual_sync_feedback = false;
    uint64_t last_full_boundary = 0;
    uint64_t last_urgent_boundary = 0;
    bool never_synced = true;
    bool network_failed = false;
    bool urgent_synced = false;
    bool boot_ledger_cleared = false;
    int64_t net_started_ms = 0;
    std::vector<uint8_t> edited_alarms;  // toggled while a sync ran
    std::vector<uint8_t> edited_todos;
    bool need_boot_ntp = false;

    // Replies owed per channel.
    PendingReply usb_reply;
    PendingReply ble_reply;
    store::WifiCreds pending_wifi;

    // Activity.
    int64_t last_activity_ms = 0;
    bool reminder_checked_this_minute = false;
    uint64_t last_minute = 0;
};


extern State g;
extern QueueHandle_t g_events;
extern QueueHandle_t g_net;
extern TaskHandle_t g_net_task;
extern std::atomic<int> g_ble_link;

bool Post(Event* e, TickType_t timeout = portMAX_DELAY);
void NetTask(void*);
size_t NavHomeIndex(NavOrigin o);
void ShowNotice(const std::string& text);
void ShowStickyNotice(const std::string& text);
const char* KeyHint(ScreenKind k);
void LoadConfig();
void LoadData();
bool ReadClock();
uint64_t Boundary(uint64_t unix, uint64_t period);
void SeedScheduler();
ui::HomeModel CurrentHome();
void DrawScreen(lv_obj_t* s, const Screen& sc);
lv_obj_t* NewScreen();
void BuildAndLoad();
void Render(display::Refresh mode = display::Refresh::Auto);
void ProgramRtcAlarm();
void StartRinging();
void StopRinging();
bool CheckRtcAlarm();
bool CheckReminders();
void DismissReminder();
void SendReply(Channel ch, const std::string& json);
PendingReply& Slot(Channel ch);
void Resolve(Channel ch, const std::string& json);
const std::string* IdOf(const PendingReply& p);
bool StartNet(NetJob job, const store::WifiCreds* creds = nullptr);
void EndBlePairing(const std::string& notice);

void HandleCommand(Channel ch, const std::string& line);
void StartManualSync();
void ScheduleSync();
void WriteNtpTime(uint64_t utc);
void ReapplyEdits(const std::vector<Alarm>& local_alarms, const std::vector<Todo>& local_todos);
void OnNetDone(const Event& e);
bool OnBleLine(const std::string& line);
void OnBleEvent(ble::Event ev);
void EnterBlePairing();
void EndBlePairing(const std::string& notice);
void OpenNavigation(NavOrigin origin);
Screen CalendarToday();
bool IsLong(const keys::Event& e, board::Key k);
bool IsPress(const keys::Event& e, board::Key k);
bool IsLongUpDown(const keys::Event& e);
void ToggleAlarm(size_t index);
void ToggleTodo(size_t index);
void HandleNavigation(const keys::Event& e);
void HandleKey(const keys::Event& e);
bool SleepBlocked();
int64_t TimerWakeSecs();
void GoToDeepSleep();
int64_t NextWaitMs();
bool Housekeeping();
void Boot(power::WakeCause wake);
bool HandleEvent(const Event& e);
void LogSleepBlock();

}  // namespace app::detail
