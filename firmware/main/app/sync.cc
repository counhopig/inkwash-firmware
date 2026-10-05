#include "app/internal.h"

namespace app::detail {

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
    const uint64_t utc = g.now.ShiftedMinutes(-g.timezone).ToUnix();
    if (power_policy::RetryPending(utc, power::State().network_retry_utc, period)) return;
    power::State().network_retry_utc = 0;
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
    if (job == NetJob::Sync || job == NetJob::UrgentPoll || job == NetJob::Ntp) {
        ReadClock();
        const uint64_t utc = g.now.ShiftedMinutes(-g.timezone).ToUnix();
        power::State().network_retry_utc = r.ok ? 0 :
            utc + std::max<uint16_t>(1, g.sync_interval) * 60ULL;
    }
    switch (job) {
        case NetJob::Sync: {
            g.network_failed = !r.ok;
            if (r.ok) {
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


}  // namespace app::detail
