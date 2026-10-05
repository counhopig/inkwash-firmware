#include "net/sync.h"
#include "diagnostics/event_log.h"

#include "cJSON.h"
#include "core/sync_payload.h"
#include "esp_log.h"
#include "net/wifi.h"

namespace netsync {
namespace {

constexpr char kTag[] = "sync";
constexpr size_t kResponseMax = 16384;
constexpr uint64_t kRtcAlignPeriod = 24 * 3600;

bool Replay(const sync_payload::Applied& a) {
    return store::SaveAlarms(a.data.alarms) && store::SaveTodos(a.data.todos) &&
           store::SaveInbox(a.data.inbox) && store::AckInboxReads(a.data.inbox_read_acked) &&
           store::ClearDirtyAlarms(a.uploaded_alarm_ids) &&
           store::ClearDirtyTodos(a.uploaded_todo_ids);
}

bool LoadConfig(store::WifiCreds* creds, store::ServerConfig* server, std::string* error) {
    if (!store::LoadWifi(creds)) {
        *error = "Wi-Fi not configured; use SetWifi first";
        return false;
    }
    if (!store::LoadServer(server)) {
        *error = "Server not configured; use SetServer first";
        return false;
    }
    return true;
}

}  // namespace

Result Run(const DateTime& now) {
    Result r;
    store::WifiCreds creds;
    store::ServerConfig server;
    if (!LoadConfig(&creds, &server, &r.error)) return r;

    const std::vector<Alarm> alarms = store::LoadAlarms();
    const std::vector<Todo> todos = store::LoadTodos();
    const std::vector<uint8_t> dirty_alarms = store::DirtyAlarms();
    const std::vector<uint8_t> dirty_todos = store::DirtyTodos();
    sync_payload::Upload up;
    for (const Alarm& a : alarms) {
        for (uint8_t id : dirty_alarms) {
            if (id == a.id) up.alarms.push_back({a.id, a.enabled});
        }
    }
    for (const Todo& t : todos) {
        for (uint8_t id : dirty_todos) {
            if (id == t.id) up.todos.push_back({t.id, t.done});
        }
    }
    up.inbox_read = store::PendingInboxReads();

    if (!wifi::Connect(creds, &r.error)) return r;
    std::string body;
    const bool posted = wifi::HttpsPost(server.url, server.token, nullptr,
                                        sync_payload::EncodeUpload(up), kResponseMax, &body,
                                        &r.error);
    sync_payload::Applied applied;
    if (posted) {
        r.error = sync_payload::DecodeResponse(body.data(), body.size(), &applied.data);
        if (!r.error.empty()) event_log::Critical("sync_response_decode_failed");
    }
    if (posted && r.error.empty()) {
        ESP_LOGI(kTag, "fetched %u alarms, %u todos, %u inbox",
                 unsigned(applied.data.alarms.size()), unsigned(applied.data.todos.size()),
                 unsigned(applied.data.inbox.size()));
        uint64_t aligned = 0;
        const bool align_due = !store::LoadRtcAlignEpoch(&aligned) ||
                               (now.ToUnix() > aligned ? now.ToUnix() - aligned
                                                       : aligned - now.ToUnix()) >= kRtcAlignPeriod;
        if (align_due) {
            r.have_ntp = wifi::NtpEpoch(&r.ntp_utc);
        }
    }
    wifi::Disconnect();
    if (!posted || !r.error.empty()) return r;

    applied.uploaded_alarm_ids = dirty_alarms;
    applied.uploaded_todo_ids = dirty_todos;
    if (!store::SaveSyncJournal(applied) || !Replay(applied) || !store::ClearSyncJournal()) {
        event_log::Critical("sync_apply_store_failed");
        r.error = "failed to store the sync result";
        return r;
    }
    store::SaveLastSyncEpoch(now.ToUnix());
    r.ok = true;
    return r;
}

Result PollUrgent(bool* urgent) {
    Result r;
    store::WifiCreds creds;
    store::ServerConfig server;
    if (!LoadConfig(&creds, &server, &r.error)) return r;
    if (!wifi::Connect(creds, &r.error)) return r;
    std::string body;
    const bool posted =
        wifi::HttpsPost(server.url, server.token, "x-inkwash-poll:1", "{}", 256, &body, &r.error);
    wifi::Disconnect();
    if (!posted) return r;
    cJSON* json = cJSON_ParseWithLength(body.data(), body.size());
    if (!json) {
        r.error = "urgent poll JSON decode failed";
        return r;
    }
    *urgent = cJSON_IsTrue(cJSON_GetObjectItemCaseSensitive(json, "urgent"));
    cJSON_Delete(json);
    r.ok = true;
    return r;
}

Result VerifyWifi(const store::WifiCreds& creds) {
    Result r;
    r.ok = wifi::Connect(creds, &r.error);
    wifi::Disconnect();
    return r;
}

bool RecoverJournal() {
    sync_payload::Applied applied;
    if (!store::LoadSyncJournal(&applied)) return false;
    ESP_LOGW(kTag, "finishing an interrupted sync apply");
    return Replay(applied) && store::ClearSyncJournal();
}

Result NtpOnly() {
    Result r;
    store::WifiCreds creds;
    if (!store::LoadWifi(&creds)) {
        r.error = "Wi-Fi not configured";
        return r;
    }
    if (!wifi::Connect(creds, &r.error)) return r;
    r.have_ntp = wifi::NtpEpoch(&r.ntp_utc);
    r.ok = r.have_ntp;
    if (!r.ok) r.error = "NTP sync timed out";
    wifi::Disconnect();
    return r;
}

}  // namespace netsync
