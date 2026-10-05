#include "app/internal.h"

namespace app::detail {

// ---- Rendering -------------------------------------------------------------------

ui::HomeModel CurrentHome() {
    return ui::BuildHome(g.alarms, g.todos, g.inbox, g.have_clock, g.now, g.wifi_connected,
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

void Render(display::Refresh mode) {
    BuildAndLoad();
    static ScreenKind shown = ScreenKind::Home;
    if (shown != g.screen.kind) mode = display::Refresh::Full;
    if (display::Update(mode)) shown = g.screen.kind;
}


}  // namespace app::detail
