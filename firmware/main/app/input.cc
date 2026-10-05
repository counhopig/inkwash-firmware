#include "app/internal.h"

namespace app::detail {

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


}  // namespace app::detail
