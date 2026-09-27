// NVS persistence, compatible with the Rust firmware (rust-firmware/src/
// {storage,alarms,todos,inbox,nvs_blob}.rs): same namespaces, keys, JSON
// blob shapes and decimal-string scalars, so a device keeps its data when it
// switches between the two firmwares.
#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "core/model.h"
#include "core/sync_payload.h"

namespace store {

bool Init();

struct WifiCreds {
    std::string ssid;
    std::string password;
};
struct ServerConfig {
    std::string url;
    std::string token;
};

bool ValidWifiCreds(const WifiCreds& creds);
bool ValidServerUrl(const std::string& url);
constexpr size_t kMaxTokenLen = 255;

bool LoadWifi(WifiCreds* out);
bool SaveWifi(const WifiCreds& creds);
bool LoadServer(ServerConfig* out);
bool SaveServer(const ServerConfig& cfg);

constexpr int16_t kMinTimezone = -720;
constexpr int16_t kMaxTimezone = 840;
int16_t LoadTimezone();  // 0 when unset or invalid
bool SaveTimezone(int16_t minutes);

constexpr uint16_t kDefaultSyncInterval = 60;
uint16_t LoadSyncInterval();
bool SaveSyncInterval(uint16_t minutes);

bool LoadLastSyncEpoch(uint64_t* out);
bool SaveLastSyncEpoch(uint64_t epoch);
bool LoadRtcAlignEpoch(uint64_t* out);
bool SaveRtcAlignEpoch(uint64_t epoch);
bool ClearRtcAlignEpoch();

std::string LoadTodoRemindedDate();
bool SaveTodoRemindedDate(const std::string& date);

std::vector<Alarm> LoadAlarms();
bool SaveAlarms(const std::vector<Alarm>& alarms);
bool MarkAlarmDirty(uint8_t id);
std::vector<uint8_t> DirtyAlarms();
bool ClearDirtyAlarms(const std::vector<uint8_t>& ids);

std::vector<Todo> LoadTodos();
bool SaveTodos(const std::vector<Todo>& todos);
bool MarkTodoDirty(uint8_t id);
std::vector<uint8_t> DirtyTodos();
bool ClearDirtyTodos(const std::vector<uint8_t>& ids);

constexpr size_t kMaxInboxItems = 32;
std::vector<InboxItem> LoadInbox();
std::vector<uint64_t> PendingInboxReads();
// Replaces the items, keeping reads not yet acknowledged by the server.
bool SaveInbox(const std::vector<InboxItem>& items);
bool MarkInboxRead(uint64_t id);
bool AckInboxReads(const std::vector<uint64_t>& acked);

// Sync apply journal: written before a sync result is applied.
bool SaveSyncJournal(const sync_payload::Applied& applied);
bool LoadSyncJournal(sync_payload::Applied* out);
bool ClearSyncJournal();

}  // namespace store
