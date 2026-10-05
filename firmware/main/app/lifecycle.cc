#include "app/internal.h"
#include "esp_system.h"

namespace app::detail {

// ---- Sleep ------------------------------------------------------------------------------

bool SleepBlocked() {
    power_policy::Work work;
    work.usb = usb_console::HostConnected();
    work.network = g.net_job != NetJob::None;
    work.alarm = g.ringing;
    work.reminder = g.screen.kind == ScreenKind::Reminder;
    work.pairing = g.screen.kind == ScreenKind::BlePairing || ble::Active();
    work.reply = g.usb_reply.active || g.ble_reply.active;
    work.audio = tones::Busy();
    work.display = !display::Healthy();
    work.events = uxQueueMessagesWaiting(g_events) != 0 || g_ble_link.load() >= 0;
    work.keys = board::KeyDown(board::Key::Enter) || board::KeyDown(board::Key::Down) ||
                board::KeyDown(board::Key::Up);
    return !power_policy::CanSleep(work);
}

int64_t TimerWakeSecs() {
    return power_policy::NextWakeSecs(g.have_clock, g.now.ToUnix(),
                                     schedule::MaintenanceWakeupSecs(g.alarms, g.now));
}

void GoToDeepSleep() {
    power::ClearBootLedger();  // reaching sleep is a healthy run
    const bool scene_changed = g.screen.kind != ScreenKind::Home;
    g.notice.reset();
    if (g.screen.kind != ScreenKind::Home) g.screen = Screen{ScreenKind::Home};
    const ui::HomeModel home = CurrentHome();
    lv_obj_t* old = lv_screen_active();
    lv_obj_t* s = NewScreen();
    ui::DrawHome(s, home);
    lv_screen_load(s);
    if (old) lv_obj_delete(old);
    if (!display::Update(scene_changed ? display::Refresh::Full : display::Refresh::Auto)) return;
    if (SleepBlocked()) return;
    power::Retained& r = power::State();
    r.home_valid = ui::PackHome(home, r.home, sizeof(r.home)) ? 1 : 0;
    r.partial_refreshes = display::PartialRefreshes();
    ProgramRtcAlarm();
    board::SetChargeLed(false);
    // Panel refresh and alarm programming take time; align sleep to the RTC
    // minute boundary using a fresh reading immediately before sleep.
    DateTime sleep_time;
    if (pcf8563::ReadTime(&sleep_time) && !sleep_time.voltage_low) {
        g.now = sleep_time;
        g.have_clock = true;
    }
    power::DeepSleep(TimerWakeSecs(), g.have_clock ? g.now.ShiftedMinutes(-g.timezone).ToUnix() : 0);
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
    const int64_t idle_limit = power_policy::IdleMs(g.background);
    const int64_t to_idle = g.last_activity_ms + idle_limit - now;
    // An expired idle deadline is not work: blocked sleep must not spin.
    if (to_idle > 0) wait = std::min(wait, to_idle);
    else wait = std::min<int64_t>(wait, SleepBlocked() ? 1000 : 20);
    if (g.net_job != NetJob::None || g.wifi_connected || ble::Active() || !display::Healthy() || tones::Busy()) wait = std::min<int64_t>(wait, 1000);
    return std::max<int64_t>(wait, 20);
}

// Timers and periodic work; returns true when something needs a redraw.
bool Housekeeping() {
    bool redraw = false;
    const bool connected = wifi::IsConnected();
    if (g.wifi_connected != connected) {
        g.wifi_connected = connected;
        redraw = true;
    }
    const int64_t now_ms = NowMs();
    event_log::Poll();
    static int64_t last_stack_probe_ms = -30000;
    if (now_ms - last_stack_probe_ms >= 30000) {
        last_stack_probe_ms = now_ms;
        if (g_net_task) {
            ESP_LOGI(kTag, "STACKPROBE cpp app=%u net=%u",
                     unsigned(uxTaskGetStackHighWaterMark(nullptr)),
                     unsigned(uxTaskGetStackHighWaterMark(g_net_task)));
        } else {
            ESP_LOGI(kTag, "STACKPROBE cpp app=%u", unsigned(uxTaskGetStackHighWaterMark(nullptr)));
        }
    }
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
    auto& diagnostic = power::State();
    const uint64_t utc = g.have_clock ? g.now.ShiftedMinutes(-g.timezone).ToUnix() : 0;
    const auto charge = board::ReadCharge();
    const uint8_t charge_state = unsigned(charge.power_present) | (unsigned(charge.charging) << 1) | (unsigned(charge.full) << 2);
    const bool changed = !diagnostic.log_power_valid || charge_state != diagnostic.log_charge_state;
    static int64_t last_sample_ms = -900000;
    const bool due = g.have_clock ? (!diagnostic.log_battery_utc || utc < diagnostic.log_battery_utc ||
                                    utc - diagnostic.log_battery_utc >= 900) : now_ms - last_sample_ms >= 900000;
    if (changed || due) {
        const int percent = board::BatteryPercent();
        event_log::Add("power battery_pct=%d usb=%u charging=%u full=%u heap=%u", percent,
                       unsigned(charge.power_present), unsigned(charge.charging), unsigned(charge.full),
                       unsigned(esp_get_free_heap_size()));
        if (percent >= 0 && percent <= 10) event_log::Critical("low_battery percent=%d", percent);
        diagnostic.log_battery_utc = utc;
        diagnostic.log_charge_state = charge_state;
        diagnostic.log_power_valid = 1;
        last_sample_ms = now_ms;
    }
    board::SetChargeLed(board::ReadCharge().charging);
    return redraw;
}

void Boot(power::WakeCause wake) {
    g.wake = wake;
    g.background = wake == power::WakeCause::Timer;
    g.last_activity_ms = NowMs();
    LoadConfig();
    if (netsync::RecoverJournal()) {
        event_log::Critical("sync_journal_recovered");
        ESP_LOGW(kTag, "recovered an interrupted sync apply");
    }
    LoadData();

    DateTime dt;
    if (pcf8563::ReadTime(&dt)) {
        ESP_LOGI(kTag, "RTC %04u-%02u-%02u %02u:%02u:%02u vl=%d", dt.year, dt.month, dt.day,
                 dt.hour, dt.minute, dt.second, dt.voltage_low);
        g.clock_estimated = dt.voltage_low || power::State().clock_estimated || event_log::LastClockEstimated();
        if (dt.voltage_low) event_log::Critical("rtc_voltage_low");
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
    event_log::SetClock(g.have_clock ? g.now.ShiftedMinutes(-g.timezone).ToUnix() : 0, g.clock_estimated);
    event_log::Add("clock utc=%llu quality=%s",
                   static_cast<unsigned long long>(g.have_clock ? g.now.ShiftedMinutes(-g.timezone).ToUnix() : 0),
                   g.have_clock ? (g.clock_estimated ? "estimated" : "rtc") : "unknown");
    power::State().clock_estimated = g.clock_estimated;
    event_log::Flush();
    SeedScheduler();
    power::PrintSleepTrace(g.have_clock ? g.now.ShiftedMinutes(-g.timezone).ToUnix() : 0);

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
            event_log::Add("key key=%u action=%u screen=%u", unsigned(e.key.key), unsigned(e.key.kind), unsigned(g.screen.kind));
            g.last_activity_ms = NowMs();
            g.background = false;
            HandleKey(e.key);
            return true;
        case Event::Kind::Command:
            HandleCommand(e.channel, e.line);
            return false;
        case Event::Kind::BleLink:
            event_log::Add("ble_link event=%u", unsigned(e.ble_event));
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
    const uint32_t mask = (usb_console::HostConnected() ? 1u : 0u) |
        (g.net_job != NetJob::None ? 2u : 0u) | (g.ringing ? 4u : 0u) |
        (g.screen.kind == ScreenKind::Reminder ? 8u : 0u) |
        (g.screen.kind == ScreenKind::BlePairing || ble::Active() ? 16u : 0u) |
        (g.usb_reply.active || g.ble_reply.active ? 32u : 0u) |
        (tones::Busy() ? 64u : 0u) | (!display::Healthy() ? 128u : 0u) |
        (uxQueueMessagesWaiting(g_events) != 0 || g_ble_link.load() >= 0 ? 256u : 0u) |
        (board::KeyDown(board::Key::Enter) || board::KeyDown(board::Key::Down) ||
         board::KeyDown(board::Key::Up) ? 512u : 0u);
    event_log::Add("sleep_held mask=0x%lx screen=%u", static_cast<unsigned long>(mask), unsigned(g.screen.kind));
    ESP_LOGI(kTag, "deep sleep held: usb=%d net=%d ring=%d screen=%d ble=%d replies=%d/%d",
             usb_console::HostConnected(), static_cast<int>(g.net_job), g.ringing,
             static_cast<int>(g.screen.kind), ble::Active(), g.usb_reply.active, g.ble_reply.active);
}

}  // namespace app::detail
