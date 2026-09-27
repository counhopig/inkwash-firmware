// Application state machine, ported from logic/src/app.rs.
//
// One task owns every piece of state. The key task, the USB reader, the BLE
// host and the network worker post events to its queue; it reacts, persists,
// redraws, and decides when to sleep.
#include "app/app.h"

#include <algorithm>
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
#include "display.h"
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

namespace app {
namespace {

constexpr char kTag[] = "app";

// Timing (logic/src/app.rs and the power changes on top of it).
constexpr int64_t kNoticeMs = 4000;
constexpr int64_t kReminderTimeoutMs = 120000;
constexpr int64_t kRingAutoSilenceMs = 300000;
constexpr int64_t kBlePairingTimeoutMs = 120000;
constexpr int64_t kDeepSleepIdleMs = 180000;
constexpr int64_t kBackgroundIdleMs = 2000;
// A run that stays up this long no longer counts as a failed boot.
constexpr int64_t kBootHealthyMs = 30 * 1000;
constexpr uint64_t kSleepWakePeriodSecs = 600;
constexpr uint64_t kSleepWakeMarginSecs = 15;
constexpr uint64_t kUrgentPeriodSecs = 30;

int64_t NowMs() { return esp_timer_get_time() / 1000; }

// ---- Events ------------------------------------------------------------------

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

QueueHandle_t g_events = nullptr;

void Post(Event* e) {
    if (xQueueSend(g_events, &e, pdMS_TO_TICKS(100)) != pdTRUE) {
        ESP_LOGW(kTag, "event queue full; dropping event");
        delete e;
    }
}

// ---- Network worker ------------------------------------------------------------

struct NetRequest {
    NetJob job;
    DateTime now;
    store::WifiCreds creds;
};
QueueHandle_t g_net = nullptr;

void NetTask(void*) {
    while (true) {
        NetRequest* req = nullptr;
        xQueueReceive(g_net, &req, portMAX_DELAY);
        auto* done = new Event{Event::Kind::NetDone};
        done->job = req->job;
        switch (req->job) {
            case NetJob::Sync: done->result = netsync::Run(req->now); break;
            case NetJob::UrgentPoll: done->result = netsync::PollUrgent(&done->urgent); break;
            case NetJob::VerifyWifi: done->result = netsync::VerifyWifi(req->creds); break;
            case NetJob::Ntp: done->result = netsync::NtpOnly(); break;
            case NetJob::None: break;
        }
        delete req;
        Post(done);
    }
}

// ---- Screens -------------------------------------------------------------------

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

size_t NavHomeIndex(NavOrigin o) {
    switch (o) {
        case NavOrigin::Home: return 0;
        case NavOrigin::Calendar: return 1;
        case NavOrigin::Inbox: return 2;
        case NavOrigin::AlarmList: return 3;
        case NavOrigin::TodoList: return 4;
        case NavOrigin::Settings: return 5;
    }
    return 0;
}

// ---- State ---------------------------------------------------------------------

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

    // Sync scheduling (logic/src/scheduler.rs).
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

State g;

// ---- Helpers -------------------------------------------------------------------

void ShowNotice(const std::string& text) {
    g.notice.reset(new Notice{text, NowMs() + kNoticeMs});
}

void ShowStickyNotice(const std::string& text) {
    g.notice.reset(new Notice{text, -1});
}

const char* KeyHint(ScreenKind k) {
    switch (k) {
        case ScreenKind::Home: return "HOLD UP OR DOWN FOR MENU";
        case ScreenKind::BlePairing: return "HOLD ENTER TO CANCEL PAIRING";
        case ScreenKind::AlarmRinging: return "PRESS ENTER TO STOP";
        case ScreenKind::About: return "PRESS ENTER TO GO BACK";
        case ScreenKind::Navigation: return "UP/DOWN MOVE  ENTER OPEN  HOLD ENTER BACK";
        default: return "UP/DOWN MOVE  ENTER OK  HOLD ENTER BACK";
    }
}

void LoadConfig() {
    store::WifiCreds creds;
    g.wifi_configured = store::LoadWifi(&creds);
    g.wifi_ssid = creds.ssid;
    g.wifi_has_password = !creds.password.empty();
    store::ServerConfig server;
    g.server_configured = store::LoadServer(&server);
    g.server_url = server.url;
    g.server_has_token = !server.token.empty();
    g.timezone = store::LoadTimezone();
    g.sync_interval = store::LoadSyncInterval();
}

void LoadData() {
    g.alarms = store::LoadAlarms();
    g.todos = store::LoadTodos();
    g.inbox = store::LoadInbox();
}

bool ReadClock() {
    DateTime dt;
    if (pcf8563::ReadTime(&dt) && !dt.voltage_low) {
        g.now = dt;
        g.have_clock = true;
    }
    return g.have_clock;
}

uint64_t Boundary(uint64_t unix, uint64_t period) { return unix / period; }

void SeedScheduler() {
    const uint64_t unix = g.have_clock ? g.now.ToUnix() : 0;
    uint64_t last_sync = 0;
    const bool synced = store::LoadLastSyncEpoch(&last_sync);
    g.never_synced = !synced;
    const uint64_t period = std::max<uint64_t>(1, g.sync_interval) * 60;
    // Seed from the last successful sync so a boundary slept through syncs at once.
    g.last_full_boundary = Boundary(synced ? last_sync : unix, period);
    g.last_urgent_boundary = Boundary(unix, kUrgentPeriodSecs);
}

// ---- Rendering -------------------------------------------------------------------

ui::HomeModel CurrentHome() {
    return ui::BuildHome(g.alarms, g.todos, g.inbox, g.have_clock, g.now, g.wifi_configured,
                         board::BatteryPercent(), board::ReadCharge());
}

void DrawScreen(lv_obj_t* s, const Screen& sc) {
    switch (sc.kind) {
        case ScreenKind::Home: ui::DrawHome(s, CurrentHome()); break;
        case ScreenKind::Navigation:
            DrawScreen(s, g.before_nav);
            ui::DrawNavigationBar(s, sc.selected);
            break;
        case ScreenKind::Settings: ui::DrawSettings(s, sc.selected); break;
        case ScreenKind::About: ui::DrawAbout(s); break;
        case ScreenKind::SyncIntervalPick: ui::DrawSyncInterval(s, sc.selected); break;
        case ScreenKind::AlarmList: ui::DrawAlarmList(s, g.alarms, sc.selected); break;
        case ScreenKind::AlarmAdd: ui::DrawNumberPick(s, sc.minute_stage, sc.value); break;
        case ScreenKind::TodoList: ui::DrawTodoList(s, g.todos, sc.selected, g.have_clock, g.now); break;
        case ScreenKind::Inbox: ui::DrawInboxList(s, g.inbox, sc.selected); break;
        case ScreenKind::InboxItem: ui::DrawInboxItem(s, g.inbox, sc.selected); break;
        case ScreenKind::Calendar:
            ui::DrawCalendar(s, sc.year, sc.month, sc.day, g.have_clock, g.now, g.todos);
            break;
        case ScreenKind::WeekView:
            ui::DrawWeek(s, g.todos, sc.year, sc.month, sc.day, g.have_clock, g.now);
            break;
        case ScreenKind::AlarmRinging: ui::DrawAlarmRinging(s); break;
        case ScreenKind::Reminder: ui::DrawReminder(s, g.reminder_urgent, g.reminder_lines); break;
        case ScreenKind::BlePairing: ui::DrawBlePairing(s, g.have_passkey, g.passkey); break;
    }
}

lv_obj_t* NewScreen() {
    lv_obj_t* s = lv_obj_create(nullptr);
    lv_obj_remove_style_all(s);
    lv_obj_set_style_bg_opa(s, LV_OPA_COVER, 0);
    lv_obj_set_style_bg_color(s, lv_color_white(), 0);
    lv_obj_remove_flag(s, LV_OBJ_FLAG_SCROLLABLE);
    return s;
}

void BuildAndLoad() {
    lv_obj_t* old = lv_screen_active();
    lv_obj_t* s = NewScreen();
    DrawScreen(s, g.screen);
    if (g.notice) ui::DrawNotice(s, g.notice->text);
    lv_screen_load(s);
    if (old) lv_obj_delete(old);
}

void Render(display::Refresh mode = display::Refresh::Auto) {
    BuildAndLoad();
    display::Update(mode);
}

// ---- Alarms ------------------------------------------------------------------------

void ProgramRtcAlarm() {
    if (!g.have_clock) return;
    const Alarm* next = schedule::NextDue(g.alarms, g.now);
    schedule::AlarmRegs regs{};
    const bool want = next && schedule::AlarmRegsFor(*next, g.now, &regs);
    if (!want) {
        if (g.programmed_valid && g.programmed_none) return;
        if (pcf8563::ClearAlarm()) {
            g.programmed_valid = true;
            g.programmed_none = true;
        }
        return;
    }
    if (g.programmed_valid && !g.programmed_none && g.programmed.minute == regs.minute &&
        g.programmed.hour == regs.hour && g.programmed.day == regs.day &&
        g.programmed.weekday == regs.weekday) {
        return;
    }
    if (pcf8563::SetAlarm(regs)) {
        g.programmed = regs;
        g.programmed_valid = true;
        g.programmed_none = false;
        ESP_LOGI(kTag, "RTC alarm programmed %02u:%02u day=%d weekday=%d", regs.hour, regs.minute,
                 regs.day, regs.weekday);
    }
}

void StartRinging() {
    if (g.screen.kind == ScreenKind::Reminder) {
        tones::Stop();
        g.before_ring = g.before_reminder;
    } else if (g.screen.kind == ScreenKind::Navigation) {
        g.before_ring = g.before_nav;
    } else {
        g.before_ring = g.screen;
    }
    g.ringing = true;
    g.ring_deadline_ms = NowMs() + kRingAutoSilenceMs;
    g.fired_minute = g.now.ToUnix() / 60;
    g.screen = Screen{ScreenKind::AlarmRinging};
    tones::Start(tones::Tone::AlarmRing);
}

void StopRinging() {
    if (!g.ringing) return;
    g.ringing = false;
    tones::Stop();
    g.screen = g.before_ring;
}

// Handles the PCF8563 alarm flag: ring when an enabled alarm is due now.
bool CheckRtcAlarm() {
    pcf8563::AlarmStatus st;
    if (!pcf8563::ReadAlarmStatus(&st) || !st.flag) return false;
    pcf8563::AckAlarm();
    if (g.ringing || !st.interrupt_enabled || !g.have_clock) return false;
    const Alarm* due = nullptr;
    for (const Alarm& a : g.alarms) {
        if (schedule::IsDueNow(a, g.now) && (!due || a.id < due->id)) due = &a;
    }
    if (!due) return false;
    const uint8_t id = due->id;
    // A one-shot alarm is used up once it rings.
    const size_t before = g.alarms.size();
    g.alarms.erase(std::remove_if(g.alarms.begin(), g.alarms.end(),
                                  [&](const Alarm& a) {
                                      return a.id == id && a.repeat.kind == Repeat::Kind::Once;
                                  }),
                   g.alarms.end());
    if (g.alarms.size() != before) store::SaveAlarms(g.alarms);
    ESP_LOGI(kTag, "alarm %u ringing", id);
    StartRinging();
    return true;
}

// ---- Reminders -------------------------------------------------------------------------

bool CheckReminders() {
    if (!g.have_clock || g.ringing || g.screen.kind == ScreenKind::Reminder ||
        g.screen.kind == ScreenKind::BlePairing) {
        return false;
    }
    std::vector<std::string> lines;
    bool urgent = false;
    std::vector<uint64_t> urgent_ids;
    for (const InboxItem& it : g.inbox) {
        if (it.IsUrgent()) {
            lines.push_back(fonts::TruncateProp(it.title, 330));
            urgent_ids.push_back(it.id);
        }
    }
    if (!lines.empty()) {
        urgent = true;
        for (uint64_t id : urgent_ids) {
            store::MarkInboxRead(id);
            for (InboxItem& it : g.inbox) {
                if (it.id == id) it.read = true;
            }
        }
    } else {
        const std::string key = schedule::ReminderDateKey(g.now);
        if (store::LoadTodoRemindedDate() == key) return false;
        for (const Todo* t : schedule::DueHighImportanceTodos(g.todos, g.now)) {
            lines.push_back(fonts::TruncateProp(t->text, 300));
        }
        if (lines.empty()) return false;
        store::SaveTodoRemindedDate(key);
    }
    g.before_reminder = g.screen.kind == ScreenKind::Navigation ? g.before_nav : g.screen;
    g.reminder_urgent = urgent;
    g.reminder_lines = lines;
    g.reminder_deadline_ms = NowMs() + kReminderTimeoutMs;
    g.screen = Screen{ScreenKind::Reminder};
    tones::Start(urgent ? tones::Tone::Siren : tones::Tone::TodoBeep);
    return true;
}

void DismissReminder() {
    if (g.screen.kind != ScreenKind::Reminder) return;
    tones::Stop();
    g.screen = g.before_reminder;
}

// ---- Commands ---------------------------------------------------------------------------

void SendReply(Channel ch, const std::string& json) {
    if (ch == Channel::Usb) usb_console::Reply(json);
    else ble::Notify(json);
}

PendingReply& Slot(Channel ch) { return ch == Channel::Usb ? g.usb_reply : g.ble_reply; }

void Resolve(Channel ch, const std::string& json) {
    SendReply(ch, json);
    Slot(ch).active = false;
}

const std::string* IdOf(const PendingReply& p) { return p.has_id ? &p.id : nullptr; }

bool StartNet(NetJob job, const store::WifiCreds* creds = nullptr) {
    if (g.net_job != NetJob::None) return false;
    auto* req = new NetRequest{job, g.now, creds ? *creds : store::WifiCreds{}};
    if (xQueueSend(g_net, &req, 0) != pdTRUE) {
        delete req;
        return false;
    }
    g.net_job = job;
    g.net_started_ms = NowMs();
    ESP_LOGI(kTag, "net job %d started", static_cast<int>(job));
    return true;
}

void EndBlePairing(const std::string& notice);

void HandleCommand(Channel ch, const std::string& line) {
    protocol::Command c;
    std::string err;
    if (!protocol::Parse(line, &c, &err)) {
        SendReply(ch, protocol::ReplyError(err, nullptr));
        return;
    }
    g.last_activity_ms = NowMs();
    g.background = false;
    const std::string* id = c.has_id ? &c.id : nullptr;
    PendingReply& slot = Slot(ch);
    if (slot.active) {
        SendReply(ch, protocol::ReplyBusy(id));
        return;
    }
    switch (c.cmd) {
        case protocol::Cmd::GetStatus: {
            protocol::Status s;
            s.wifi_configured = g.wifi_configured;
            s.server_configured = g.server_configured;
            s.wifi_connected = g.wifi_connected;
            s.has_ssid = g.wifi_configured;
            s.wifi_ssid = g.wifi_ssid;
            s.wifi_has_password = g.wifi_has_password;
            s.has_server_url = g.server_configured;
            s.server_url = g.server_url;
            s.server_has_token = g.server_has_token;
            s.timezone_offset_minutes = g.timezone;
            SendReply(ch, protocol::ReplyStatus(s, id));
            return;
        }
        case protocol::Cmd::SetRtc: {
            if (c.epoch_secs < 946684800ULL || c.epoch_secs > 4102444800ULL) {
                SendReply(ch, protocol::ReplyError("RTC timestamp must be between 2000-01-01 and 2100-01-01", id));
                return;
            }
            const DateTime local = DateTime::FromUnix(c.epoch_secs).ShiftedMinutes(g.timezone);
            if (local.year < 2000 || local.year > 2099) {
                SendReply(ch, protocol::ReplyError("RTC local time must remain between 2000-01-01 and 2099-12-31 after timezone conversion", id));
                return;
            }
            store::ClearRtcAlignEpoch();
            if (!pcf8563::WriteTime(local)) {
                SendReply(ch, protocol::ReplyError("RTC write failed", id));
                return;
            }
            g.now = local;
            g.have_clock = true;
            g.programmed_valid = false;
            ProgramRtcAlarm();
            SendReply(ch, protocol::ReplyOk(id));
            Render();
            return;
        }
        case protocol::Cmd::SetTimezone: {
            if (c.offset_minutes < store::kMinTimezone || c.offset_minutes > store::kMaxTimezone) {
                SendReply(ch, protocol::ReplyError("Timezone offset must be between -720 and 840 minutes", id));
                return;
            }
            if (!g.have_clock) {
                SendReply(ch, protocol::ReplyError("System time not available", id));
                return;
            }
            const DateTime shifted = g.now.ShiftedMinutes(c.offset_minutes - g.timezone);
            if (!pcf8563::WriteTime(shifted) || !store::SaveTimezone(c.offset_minutes)) {
                SendReply(ch, protocol::ReplyError("Failed to apply the timezone", id));
                return;
            }
            g.timezone = c.offset_minutes;
            g.now = shifted;
            g.programmed_valid = false;
            ProgramRtcAlarm();
            SendReply(ch, protocol::ReplyOk(id));
            Render();
            return;
        }
        case protocol::Cmd::SetServer: {
            store::ServerConfig cfg{c.url, c.token};
            if (!store::ValidServerUrl(c.url) || c.token.size() > store::kMaxTokenLen) {
                SendReply(ch, protocol::ReplyError("Server URL must be an HTTPS URL without embedded credentials and token must not exceed 255 bytes", id));
                return;
            }
            if (!store::SaveServer(cfg)) {
                SendReply(ch, protocol::ReplyError("Failed to save the server configuration", id));
                return;
            }
            LoadConfig();
            SendReply(ch, protocol::ReplyOk(id));
            return;
        }
        case protocol::Cmd::ClearAlarms: {
            if (g.ringing) StopRinging();
            g.alarms.clear();
            const bool ok = store::SaveAlarms(g.alarms) && pcf8563::ClearAlarm();
            g.programmed_valid = ok;
            g.programmed_none = ok;
            SendReply(ch, ok ? protocol::ReplyOk(id) : protocol::ReplyError("Failed to clear alarms", id));
            Render();
            return;
        }
        case protocol::Cmd::SetWifi: {
            store::WifiCreds creds{c.ssid, c.password};
            if (!store::ValidWifiCreds(creds)) {
                SendReply(ch, protocol::ReplyError("Wi-Fi SSID must be 1-32 bytes; password must be empty, 8-63 bytes, or a 64-digit hexadecimal PSK", id));
                return;
            }
            if (ch == Channel::Ble) {
                // The radio is busy with BLE: save now, prove the credentials
                // with a sync once pairing ends.
                if (!store::SaveWifi(creds)) {
                    SendReply(ch, protocol::ReplyError("Failed to save Wi-Fi credentials", id));
                    return;
                }
                LoadConfig();
                SendReply(ch, protocol::ReplyOk(id));
                g.ble_finish_pending = true;
                g.ble_finish_notice = "PAIRED - WI-FI SAVED";
                return;
            }
            if (!StartNet(NetJob::VerifyWifi, &creds)) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            slot = PendingReply{true, c.has_id, c.id, c.cmd};
            g.pending_wifi = creds;
            return;
        }
        case protocol::Cmd::SyncNow: {
            if (ch == Channel::Ble || ble::Active()) {
                SendReply(ch, protocol::ReplyError("Sync is unavailable while BLE pairing is active", id));
                return;
            }
            if (g.net_job != NetJob::None) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            if (!g.have_clock) {
                SendReply(ch, protocol::ReplyError("System time not available", id));
                return;
            }
            if (!StartNet(NetJob::Sync)) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            slot = PendingReply{true, c.has_id, c.id, c.cmd};
            return;
        }
    }
}

// ---- Sync ---------------------------------------------------------------------------------

void StartManualSync() {
    if (!g.wifi_configured) ShowNotice("WI-FI NOT SET UP");
    else if (!g.server_configured) ShowNotice("SERVER NOT SET UP");
    else if (g.net_job == NetJob::Sync) ShowNotice("SYNC ALREADY RUNNING");
    else if (g.net_job != NetJob::None) ShowNotice("NETWORK BUSY, TRY AGAIN");
    else if (!g.have_clock) ShowNotice("CLOCK NOT SET");
    else if (StartNet(NetJob::Sync)) {
        g.manual_sync_feedback = true;
        ShowStickyNotice("SYNCING...");
    } else {
        ShowNotice("NETWORK BUSY, TRY AGAIN");
    }
}

void ScheduleSync() {
    if (g.net_job != NetJob::None || !g.have_clock || !g.wifi_configured || !g.server_configured ||
        ble::Active()) {
        return;
    }
    const uint64_t unix = g.now.ToUnix();
    const uint64_t period = std::max<uint16_t>(1, g.sync_interval) * 60ULL;
    const bool full_due = Boundary(unix, period) != g.last_full_boundary;
    const bool urgent_due = !g.background && Boundary(unix, kUrgentPeriodSecs) != g.last_urgent_boundary;
    if (!full_due && !urgent_due) return;
    if (!full_due && g.network_failed) {
        g.last_urgent_boundary = Boundary(unix, kUrgentPeriodSecs);  // back off
        return;
    }
    if (full_due || (g.never_synced && urgent_due)) {
        g.last_full_boundary = Boundary(unix, period);
        g.last_urgent_boundary = Boundary(unix, kUrgentPeriodSecs);
        StartNet(NetJob::Sync);
        return;
    }
    g.last_urgent_boundary = Boundary(unix, kUrgentPeriodSecs);
    StartNet(NetJob::UrgentPoll);
}

void WriteNtpTime(uint64_t utc) {
    const DateTime local = DateTime::FromUnix(utc).ShiftedMinutes(g.timezone);
    if (pcf8563::WriteTime(local)) {
        g.now = local;
        g.have_clock = true;
        store::SaveRtcAlignEpoch(local.ToUnix());
        g.programmed_valid = false;
        ESP_LOGI(kTag, "RTC set from NTP: %04u-%02u-%02u %02u:%02u", local.year, local.month,
                 local.day, local.hour, local.minute);
    }
}

void ReapplyEdits(const std::vector<Alarm>& local_alarms, const std::vector<Todo>& local_todos) {
    bool alarms_changed = false;
    for (uint8_t id : g.edited_alarms) {
        auto src = std::find_if(local_alarms.begin(), local_alarms.end(), [id](const Alarm& a) { return a.id == id; });
        auto dst = std::find_if(g.alarms.begin(), g.alarms.end(), [id](const Alarm& a) { return a.id == id; });
        if (src == local_alarms.end() || dst == g.alarms.end()) continue;
        if (dst->enabled != src->enabled) {
            dst->enabled = src->enabled;
            alarms_changed = true;
        }
        store::MarkAlarmDirty(id);
    }
    if (alarms_changed) store::SaveAlarms(g.alarms);
    bool todos_changed = false;
    for (uint8_t id : g.edited_todos) {
        auto src = std::find_if(local_todos.begin(), local_todos.end(), [id](const Todo& t) { return t.id == id; });
        auto dst = std::find_if(g.todos.begin(), g.todos.end(), [id](const Todo& t) { return t.id == id; });
        if (src == local_todos.end() || dst == g.todos.end()) continue;
        if (dst->done != src->done) {
            dst->done = src->done;
            todos_changed = true;
        }
        store::MarkTodoDirty(id);
    }
    if (todos_changed) store::SaveTodos(g.todos);
    if (!g.edited_alarms.empty() || !g.edited_todos.empty()) {
        ESP_LOGI(kTag, "reapplied %u alarm and %u todo edits made during sync",
                 unsigned(g.edited_alarms.size()), unsigned(g.edited_todos.size()));
    }
}

void OnNetDone(const Event& e) {
    const NetJob job = g.net_job;
    ESP_LOGI(kTag, "net job %d done in %lld ms: %s", static_cast<int>(job),
             static_cast<long long>(NowMs() - g.net_started_ms),
             e.result.ok ? "ok" : e.result.error.c_str());
    g.net_job = NetJob::None;
    const netsync::Result& r = e.result;
    switch (job) {
        case NetJob::Sync: {
            g.network_failed = !r.ok;
            if (r.ok) {
                g.wifi_connected = true;
                g.never_synced = false;
                // The worker saved the server's lists; keep toggles made while
                // the sync ran and upload them next time.
                const std::vector<Alarm> local_alarms = g.alarms;
                const std::vector<Todo> local_todos = g.todos;
                LoadData();
                ReapplyEdits(local_alarms, local_todos);
                if (r.have_ntp) WriteNtpTime(r.ntp_utc);
                g.programmed_valid = false;
                ProgramRtcAlarm();
            } else {
                ESP_LOGW(kTag, "sync failed: %s", r.error.c_str());
            }
            g.edited_alarms.clear();
            g.edited_todos.clear();
            if (g.manual_sync_feedback) {
                g.manual_sync_feedback = false;
                ShowNotice(r.ok ? "SYNC OK" : "SYNC FAILED: " + r.error);
            }
            for (Channel ch : {Channel::Usb, Channel::Ble}) {
                PendingReply& p = Slot(ch);
                if (p.active && p.cmd == protocol::Cmd::SyncNow) {
                    Resolve(ch, r.ok ? protocol::ReplyOk(IdOf(p)) : protocol::ReplyError(r.error, IdOf(p)));
                }
            }
            break;
        }
        case NetJob::UrgentPoll:
            g.network_failed = !r.ok;
            if (r.ok && e.urgent && !g.urgent_synced) {
                StartNet(NetJob::Sync);
                g.urgent_synced = true;
            } else if (r.ok && !e.urgent) {
                g.urgent_synced = false;
            }
            break;
        case NetJob::VerifyWifi: {
            PendingReply& p = Slot(Channel::Usb);
            if (r.ok && store::SaveWifi(g.pending_wifi)) {
                LoadConfig();
                g.network_failed = false;
                if (p.active) Resolve(Channel::Usb, protocol::ReplyOk(IdOf(p)));
            } else if (p.active) {
                Resolve(Channel::Usb, protocol::ReplyError(r.ok ? "Failed to save Wi-Fi credentials" : r.error, IdOf(p)));
            }
            break;
        }
        case NetJob::Ntp:
            if (r.ok) WriteNtpTime(r.ntp_utc);
            else ESP_LOGW(kTag, "boot NTP failed: %s", r.error.c_str());
            break;
        case NetJob::None:
            break;
    }
}

// ---- BLE pairing ----------------------------------------------------------------------------

void OnBleLine(const std::string& line) {
    auto* e = new Event{Event::Kind::Command};
    e->channel = Channel::Ble;
    e->line = line;
    Post(e);
}

void OnBleEvent(ble::Event ev) {
    auto* e = new Event{Event::Kind::BleLink};
    e->ble_event = ev;
    Post(e);
}

void EnterBlePairing() {
    if (wifi::UsedThisBoot()) {
        // A torn-down Wi-Fi driver leaves the heap too fragmented for BLE:
        // restart and open pairing before Wi-Fi exists.
        power::State().open_ble_pairing = 1;
        power::Restart();
    }
    if (g.net_job != NetJob::None) {
        ShowNotice("NETWORK BUSY, TRY AGAIN");
        return;
    }
    uint32_t passkey = 0;
    if (!ble::Start(&passkey, OnBleLine, OnBleEvent)) {
        ShowNotice("BLE FAILED: could not start the BLE stack");
        g.screen = Screen{ScreenKind::Settings, ui::kSettingsBlePairing};
        return;
    }
    g.passkey = passkey;
    g.have_passkey = true;
    g.ble_deadline_ms = NowMs() + kBlePairingTimeoutMs;
    g.screen = Screen{ScreenKind::BlePairing};
}

void EndBlePairing(const std::string& notice) {
    ble::Stop();
    g.ble_finish_pending = false;
    g.ble_reply.active = false;
    g.have_passkey = false;
    g.screen = Screen{ScreenKind::Settings, ui::kSettingsBlePairing};
    if (!notice.empty()) ShowNotice(notice);
}

// ---- Keys -----------------------------------------------------------------------------------

void OpenNavigation(NavOrigin origin) {
    g.before_nav = g.screen;
    g.nav_origin = origin;
    g.screen = Screen{ScreenKind::Navigation, NavHomeIndex(origin)};
}

Screen CalendarToday() {
    Screen s{ScreenKind::Calendar};
    s.year = g.have_clock ? g.now.year : 1970;
    s.month = g.have_clock ? g.now.month : 1;
    const uint8_t day = g.have_clock ? g.now.day : 1;
    s.day = std::max<uint8_t>(1, std::min(day, DaysInMonth(s.year, s.month)));
    return s;
}

bool IsLong(const keys::Event& e, board::Key k) { return e.kind == keys::Kind::LongPressed && e.key == k; }
bool IsPress(const keys::Event& e, board::Key k) { return e.kind == keys::Kind::Pressed && e.key == k; }
bool IsLongUpDown(const keys::Event& e) {
    return e.kind == keys::Kind::LongPressed && (e.key == board::Key::Up || e.key == board::Key::Down);
}

void ToggleAlarm(size_t index) {
    Alarm& a = g.alarms[index];
    a.enabled = !a.enabled;
    if (g.net_job == NetJob::Sync) g.edited_alarms.push_back(a.id);
    if (!store::SaveAlarms(g.alarms) || !store::MarkAlarmDirty(a.id)) {
        a.enabled = !a.enabled;
        ShowNotice("SAVE FAILED");
        return;
    }
    ProgramRtcAlarm();
}

void ToggleTodo(size_t index) {
    Todo& t = g.todos[index];
    t.done = !t.done;
    if (g.net_job == NetJob::Sync) g.edited_todos.push_back(t.id);
    if (!store::SaveTodos(g.todos) || !store::MarkTodoDirty(t.id)) {
        t.done = !t.done;
        ShowNotice("SAVE FAILED");
    }
}

void HandleNavigation(const keys::Event& e) {
    Screen& s = g.screen;
    const size_t cur = s.selected;
    if (IsPress(e, board::Key::Up)) s.selected = cur == 0 ? ui::kNavDestinations - 1 : cur - 1;
    else if (IsPress(e, board::Key::Down)) s.selected = (cur + 1) % ui::kNavDestinations;
    else if (IsLong(e, board::Key::Up)) s.selected = 0;
    else if (IsLong(e, board::Key::Down)) s.selected = ui::kNavDestinations - 1;
    else if (IsLong(e, board::Key::Enter)) g.screen = g.before_nav;
    else if (IsPress(e, board::Key::Enter)) {
        if (cur == NavHomeIndex(g.nav_origin)) {
            g.screen = g.before_nav;
            return;
        }
        switch (cur) {
            case 0: g.screen = Screen{ScreenKind::Home}; break;
            case 1: g.screen = CalendarToday(); break;
            case 2: g.screen = Screen{ScreenKind::Inbox}; break;
            case 3: g.screen = Screen{ScreenKind::AlarmList}; break;
            case 4: g.screen = Screen{ScreenKind::TodoList}; break;
            default: g.screen = Screen{ScreenKind::Settings}; break;
        }
    }
}

void HandleKey(const keys::Event& e) {
    if (e.kind == keys::Kind::Released) {
        if (g.screen.kind == ScreenKind::BlePairing) g.ble_input_released = true;
        return;
    }
    Screen& s = g.screen;
    const Screen before = s;
    const size_t notices_before = g.notice ? 1 : 0;
    const std::string notice_before = g.notice ? g.notice->text : "";
    bool acted = false;  // did something that is not a screen change (a toggle)

    switch (s.kind) {
        case ScreenKind::AlarmRinging:
            if (IsPress(e, board::Key::Enter)) StopRinging();
            break;
        case ScreenKind::Reminder:
            DismissReminder();
            break;
        case ScreenKind::Home:
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::Home);
            break;
        case ScreenKind::Navigation:
            HandleNavigation(e);
            break;
        case ScreenKind::Settings:
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::Settings);
            else if (IsPress(e, board::Key::Up)) s.selected = s.selected ? s.selected - 1 : 0;
            else if (IsPress(e, board::Key::Down)) s.selected = std::min(s.selected + 1, ui::kSettingsRows - 1);
            else if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Home};
            else if (IsPress(e, board::Key::Enter)) {
                switch (s.selected) {
                    case ui::kSettingsSyncNow: StartManualSync(); break;
                    case ui::kSettingsSyncInterval: s = Screen{ScreenKind::SyncIntervalPick}; break;
                    case ui::kSettingsBlePairing:
                        EnterBlePairing();
                        g.ble_input_released = false;
                        break;
                    default: s = Screen{ScreenKind::About}; break;
                }
            }
            break;
        case ScreenKind::About:
            if (IsPress(e, board::Key::Enter) || IsLong(e, board::Key::Enter)) {
                s = Screen{ScreenKind::Settings, ui::kSettingsAbout};
            }
            break;
        case ScreenKind::SyncIntervalPick: {
            constexpr size_t n = sizeof(ui::kSyncIntervals) / sizeof(ui::kSyncIntervals[0]);
            if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Settings, ui::kSettingsSyncInterval};
            else if (IsPress(e, board::Key::Up)) s.selected = s.selected == 0 ? n - 1 : s.selected - 1;
            else if (IsPress(e, board::Key::Down)) s.selected = (s.selected + 1) % n;
            else if (IsPress(e, board::Key::Enter)) {
                const uint16_t minutes = ui::kSyncIntervals[s.selected];
                if (store::SaveSyncInterval(minutes)) {
                    g.sync_interval = minutes;
                    if (g.have_clock) g.last_full_boundary = Boundary(g.now.ToUnix(), minutes * 60ULL);
                    ShowNotice("SYNC EVERY " + std::to_string(minutes) + " MIN");
                } else {
                    ShowNotice("SAVE FAILED");
                }
                s = Screen{ScreenKind::Settings, ui::kSettingsSyncInterval};
            }
            break;
        }
        case ScreenKind::AlarmList: {
            const size_t count = g.alarms.size();
            const size_t sel = std::min(s.selected, count);
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::AlarmList);
            else if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Home};
            else if (IsPress(e, board::Key::Up)) s.selected = sel == 0 ? count : sel - 1;
            else if (IsPress(e, board::Key::Down)) s.selected = sel == count ? 0 : sel + 1;
            else if (IsPress(e, board::Key::Enter)) {
                if (sel == count) s = Screen{ScreenKind::AlarmAdd};
                else {
                    ToggleAlarm(sel);
                    acted = true;
                }
            }
            break;
        }
        case ScreenKind::AlarmAdd: {
            const int max = s.minute_stage ? 59 : 23;
            if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::AlarmList, g.alarms.size()};
            else if (IsPress(e, board::Key::Up)) s.value = s.value == 0 ? max : s.value - 1;
            else if (IsPress(e, board::Key::Down)) s.value = s.value == max ? 0 : s.value + 1;
            else if (IsPress(e, board::Key::Enter)) {
                if (!s.minute_stage) {
                    s.hour = static_cast<uint8_t>(s.value);
                    s.minute_stage = true;
                    s.value = 0;
                } else {
                    const int id = schedule::NextAlarmId(g.alarms);
                    if (id < 0) {
                        ShowNotice("NO FREE ALARM SLOT");
                        break;
                    }
                    Alarm a;
                    a.id = static_cast<uint8_t>(id);
                    a.hour = s.hour;
                    a.minute = static_cast<uint8_t>(s.value);
                    g.alarms.push_back(a);
                    if (!store::SaveAlarms(g.alarms)) {
                        g.alarms.pop_back();
                        ShowNotice("SAVE FAILED");
                        break;
                    }
                    ProgramRtcAlarm();
                    s = Screen{ScreenKind::AlarmList, g.alarms.size() - 1};
                }
            }
            break;
        }
        case ScreenKind::TodoList: {
            const size_t count = g.todos.size();
            const size_t sel = count ? std::min(s.selected, count - 1) : 0;
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::TodoList);
            else if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Home};
            else if (IsPress(e, board::Key::Up)) s.selected = count == 0 ? 0 : sel == 0 ? count - 1 : sel - 1;
            else if (IsPress(e, board::Key::Down)) s.selected = count == 0 || sel + 1 == count ? 0 : sel + 1;
            else if (IsPress(e, board::Key::Enter) && sel < count) {
                ToggleTodo(sel);
                acted = true;
            }
            break;
        }
        case ScreenKind::Inbox: {
            const size_t count = g.inbox.size();
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::Inbox);
            else if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Home};
            else if (IsPress(e, board::Key::Up)) s.selected = s.selected ? s.selected - 1 : 0;
            else if (IsPress(e, board::Key::Down)) s.selected = std::min(s.selected + 1, count ? count - 1 : 0);
            else if (IsPress(e, board::Key::Enter) && count) {
                const size_t index = std::min(s.selected, count - 1);
                InboxItem& it = g.inbox[index];
                if (!it.read) {
                    it.read = true;
                    store::MarkInboxRead(it.id);
                }
                s = Screen{ScreenKind::InboxItem, index};
            }
            break;
        }
        case ScreenKind::InboxItem:
            s = Screen{ScreenKind::Inbox, s.selected};
            break;
        case ScreenKind::Calendar:
            if (IsLongUpDown(e)) OpenNavigation(NavOrigin::Calendar);
            else if (IsLong(e, board::Key::Enter)) s = Screen{ScreenKind::Home};
            else if (IsPress(e, board::Key::Up)) s.day = std::max<uint8_t>(1, s.day - 1);
            else if (IsPress(e, board::Key::Down)) s.day = std::min<uint8_t>(s.day + 1, DaysInMonth(s.year, s.month));
            else if (IsPress(e, board::Key::Enter)) {
                Screen w{ScreenKind::WeekView};
                w.year = s.year;
                w.month = s.month;
                w.day = s.day;
                s = w;
            }
            break;
        case ScreenKind::WeekView: {
            Screen c{ScreenKind::Calendar};
            c.year = s.year;
            c.month = s.month;
            c.day = s.day;
            s = c;
            break;
        }
        case ScreenKind::BlePairing:
            if (IsLong(e, board::Key::Enter)) {
                if (g.ble_input_released) EndBlePairing("");
                else return;  // the hold that opened pairing
            }
            break;
    }

    const bool notice_changed = (g.notice ? 1u : 0u) != notices_before ||
                                (g.notice && g.notice->text != notice_before);
    if (!acted && s == before && !notice_changed) {
        ShowNotice(KeyHint(s.kind));
    }
}

// ---- Sleep ------------------------------------------------------------------------------

bool SleepBlocked() {
    return usb_console::HostConnected() || g.net_job != NetJob::None || g.ringing ||
           g.screen.kind == ScreenKind::Reminder || g.screen.kind == ScreenKind::BlePairing ||
           ble::Active() || g.usb_reply.active || g.ble_reply.active;
}

int64_t TimerWakeSecs() {
    if (!g.have_clock) return static_cast<int64_t>(kSleepWakePeriodSecs);
    const uint64_t unix = g.now.ToUnix();
    int64_t secs = static_cast<int64_t>(kSleepWakePeriodSecs - unix % kSleepWakePeriodSecs + kSleepWakeMarginSecs);
    const int64_t maintenance = schedule::MaintenanceWakeupSecs(g.alarms, g.now);
    if (maintenance > 0) secs = std::min(secs, maintenance);
    return secs;
}

[[noreturn]] void GoToDeepSleep() {
    power::ClearBootLedger();  // reaching sleep is a healthy run
    g.notice.reset();
    if (g.screen.kind != ScreenKind::Home) g.screen = Screen{ScreenKind::Home};
    const ui::HomeModel home = CurrentHome();
    lv_obj_t* old = lv_screen_active();
    lv_obj_t* s = NewScreen();
    ui::DrawHome(s, home);
    lv_screen_load(s);
    if (old) lv_obj_delete(old);
    display::Update();
    power::Retained& r = power::State();
    r.home_valid = ui::PackHome(home, r.home, sizeof(r.home)) ? 1 : 0;
    r.partial_refreshes = display::PartialRefreshes();
    ProgramRtcAlarm();
    board::SetChargeLed(false);
    power::DeepSleep(TimerWakeSecs());
}

// ---- Main loop --------------------------------------------------------------------------

int64_t NextWaitMs() {
    int64_t wait = 60000;
    if (g.have_clock) {
        wait = (60 - g.now.second) * 1000 + 50;  // next minute boundary
        const bool urgent_polling = g.wifi_configured && g.server_configured && !g.network_failed && !g.background;
        if (urgent_polling) {
            const int64_t to_half = ((30 - g.now.second % 30) % 30 + (g.now.second % 30 == 0 ? 30 : 0)) * 1000 + 50;
            wait = std::min(wait, to_half);
        }
    }
    const int64_t now = NowMs();
    if (g.notice && g.notice->expires_ms >= 0) wait = std::min(wait, g.notice->expires_ms - now);
    if (g.ringing) wait = std::min(wait, g.ring_deadline_ms - now);
    if (g.screen.kind == ScreenKind::Reminder) wait = std::min(wait, g.reminder_deadline_ms - now);
    if (g.screen.kind == ScreenKind::BlePairing) wait = std::min(wait, g.ble_deadline_ms - now);
    if (g.ble_finish_pending) wait = std::min<int64_t>(wait, 500);
    const int64_t idle_limit = g.background ? kBackgroundIdleMs : kDeepSleepIdleMs;
    wait = std::min(wait, g.last_activity_ms + idle_limit - now);
    return std::max<int64_t>(wait, 20);
}

// Timers and periodic work; returns true when something needs a redraw.
bool Housekeeping() {
    bool redraw = false;
    const int64_t now_ms = NowMs();
    if (!g.boot_ledger_cleared && now_ms >= kBootHealthyMs) {
        power::ClearBootLedger();
        g.boot_ledger_cleared = true;
        ESP_LOGI(kTag, "boot ledger cleared: running normally");
    }
    const DateTime previous = g.now;
    ReadClock();
    const bool minute_changed = g.have_clock && !previous.SameMinute(g.now);
    if (minute_changed) {
        redraw = true;
        if (g.screen.kind == ScreenKind::Calendar &&
            (g.screen.year != g.now.year || g.screen.month != g.now.month)) {
            g.screen.year = g.now.year;
            g.screen.month = g.now.month;
            g.screen.day = std::max<uint8_t>(1, std::min(g.screen.day, DaysInMonth(g.now.year, g.now.month)));
        }
        ProgramRtcAlarm();
    }
    if (CheckRtcAlarm()) redraw = true;
    if (g.notice && g.notice->expires_ms >= 0 && now_ms >= g.notice->expires_ms) {
        g.notice.reset();
        redraw = true;
    }
    if (g.ringing && now_ms >= g.ring_deadline_ms) {
        StopRinging();
        redraw = true;
    }
    if (g.screen.kind == ScreenKind::Reminder && now_ms >= g.reminder_deadline_ms) {
        DismissReminder();
        redraw = true;
    }
    if (g.screen.kind == ScreenKind::BlePairing && now_ms >= g.ble_deadline_ms) {
        EndBlePairing("PAIRING TIMED OUT");
        redraw = true;
    }
    if (g.ble_finish_pending) {
        vTaskDelay(pdMS_TO_TICKS(300));  // let the reply notification go out
        EndBlePairing(g.ble_finish_notice);
        if (g.wifi_configured && g.server_configured) StartManualSync();
        redraw = true;
    }
    if (minute_changed || g.last_minute == 0) {
        g.last_minute = g.now.ToUnix() / 60;
        if (CheckReminders()) redraw = true;
    }
    ScheduleSync();
    board::SetChargeLed(board::ReadCharge().charging);
    return redraw;
}

void Boot(power::WakeCause wake) {
    g.wake = wake;
    g.background = wake == power::WakeCause::Timer;
    g.last_activity_ms = NowMs();
    LoadConfig();
    if (netsync::RecoverJournal()) ESP_LOGW(kTag, "recovered an interrupted sync apply");
    LoadData();

    DateTime dt;
    if (pcf8563::ReadTime(&dt)) {
        ESP_LOGI(kTag, "RTC %04u-%02u-%02u %02u:%02u:%02u vl=%d", dt.year, dt.month, dt.day,
                 dt.hour, dt.minute, dt.second, dt.voltage_low);
        if (dt.voltage_low) {
            // The RTC lost power: start from the build time and ask NTP.
            const DateTime seeded = DateTime::FromUnix(INKWASH_BUILD_EPOCH).ShiftedMinutes(g.timezone);
            if (pcf8563::WriteTime(seeded)) dt = seeded;
            store::ClearRtcAlignEpoch();
            g.need_boot_ntp = g.wifi_configured;
        }
        g.now = dt;
        g.have_clock = !dt.voltage_low || pcf8563::ReadTime(&g.now);
    }
    SeedScheduler();

    // The panel still shows the Home screen drawn before deep sleep: rebuild
    // that exact frame so this wake refreshes only what changed.
    power::Retained& r = power::State();
    ui::HomeModel shown;
    if (wake != power::WakeCause::PowerOn && r.home_valid &&
        ui::UnpackHome(r.home, sizeof(r.home), &shown)) {
        lv_obj_t* s = NewScreen();
        ui::DrawHome(s, shown);
        lv_screen_load(s);
        display::AdoptPanelContent(r.partial_refreshes);
    }
    r.home_valid = 0;

    // Alarm first: an RTC alarm wake must ring before anything else.
    pcf8563::AlarmStatus st;
    if (pcf8563::ReadAlarmStatus(&st) && st.flag) {
        CheckRtcAlarm();
    } else {
        ProgramRtcAlarm();
    }
    if (r.open_ble_pairing) {
        r.open_ble_pairing = 0;
        if (!g.ringing) {
            EnterBlePairing();
            g.ble_input_released = true;
        }
    }
    if (!g.ringing && g.screen.kind != ScreenKind::BlePairing) CheckReminders();
    g.last_minute = g.have_clock ? g.now.ToUnix() / 60 : 1;
    Render(wake == power::WakeCause::PowerOn ? display::Refresh::Full : display::Refresh::Auto);
    if (g.need_boot_ntp) StartNet(NetJob::Ntp);
}

// Returns true when the event needs a redraw.
bool HandleEvent(const Event& e) {
    switch (e.kind) {
        case Event::Kind::Key:
            ESP_LOGI(kTag, "key %d %s on screen %d", static_cast<int>(e.key.key),
                     e.key.kind == keys::Kind::Pressed       ? "press"
                     : e.key.kind == keys::Kind::LongPressed ? "hold"
                                                             : "release",
                     static_cast<int>(g.screen.kind));
            g.last_activity_ms = NowMs();
            g.background = false;
            HandleKey(e.key);
            return true;
        case Event::Kind::Command:
            HandleCommand(e.channel, e.line);
            return false;
        case Event::Kind::BleLink:
            g.last_activity_ms = NowMs();
            if (e.ble_event == ble::Event::Connected || e.ble_event == ble::Event::Encrypted) {
                g.ble_deadline_ms = NowMs() + kBlePairingTimeoutMs;
            }
            return false;
        case Event::Kind::NetDone:
            OnNetDone(e);
            return true;
    }
    return false;
}

// Logs why deep sleep is being held off, at most every 30 s.
void LogSleepBlock() {
    static int64_t last_ms = -30000;
    const int64_t now = NowMs();
    if (now - last_ms < 30000) return;
    last_ms = now;
    ESP_LOGI(kTag, "deep sleep held: usb=%d net=%d ring=%d screen=%d ble=%d replies=%d/%d",
             usb_console::HostConnected(), static_cast<int>(g.net_job), g.ringing,
             static_cast<int>(g.screen.kind), ble::Active(), g.usb_reply.active, g.ble_reply.active);
}

}  // namespace

void Run(power::WakeCause wake) {
    g_events = xQueueCreate(16, sizeof(Event*));
    g_net = xQueueCreate(2, sizeof(NetRequest*));
    // TLS handshakes need a deep stack (the Rust sync task used 16 KiB).
    xTaskCreate(NetTask, "net", 16384, nullptr, 4, nullptr);

    Boot(wake);

    usb_console::Start([](const std::string& line) {
        auto* e = new Event{Event::Kind::Command};
        e->channel = Channel::Usb;
        e->line = line;
        Post(e);
    });
    keys::Start([](keys::Event k) {
        auto* e = new Event{Event::Kind::Key};
        e->key = k;
        Post(e);
    });
    power::EnableLightSleep();

    while (true) {
        Event* e = nullptr;
        bool redraw = false;
        if (xQueueReceive(g_events, &e, pdMS_TO_TICKS(NextWaitMs())) == pdTRUE) {
            // Handle everything already queued (keys pressed during the last
            // panel refresh) before drawing, so one refresh covers them all.
            do {
                std::unique_ptr<Event> owned(e);
                if (HandleEvent(*e)) redraw = true;
            } while (xQueueReceive(g_events, &e, 0) == pdTRUE);
        }
        if (Housekeeping()) redraw = true;
        if (redraw) Render();

        const int64_t idle_limit = g.background ? kBackgroundIdleMs : kDeepSleepIdleMs;
        if (NowMs() - g.last_activity_ms >= idle_limit) {
            if (!SleepBlocked()) GoToDeepSleep();
            LogSleepBlock();
        }
    }
}

}  // namespace app
