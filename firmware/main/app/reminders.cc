#include "app/internal.h"

namespace app::detail {

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


}  // namespace app::detail
