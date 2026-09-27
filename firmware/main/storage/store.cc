#include "storage/store.h"

#include <cctype>
#include <algorithm>
#include <cstdlib>
#include <cstring>

#include "cJSON.h"
#include "core/codec.h"
#include "core/schedule.h"
#include "esp_log.h"
#include "nvs.h"
#include "nvs_flash.h"

namespace store {
namespace {

constexpr char kTag[] = "store";

constexpr char kNsMain[] = "inkwash";
constexpr char kNsAlarms[] = "inkwash_alrm";
constexpr char kNsTodos[] = "inkwash_todo";
constexpr char kNsInbox[] = "inkwash_inbox";

constexpr size_t kWifiBlobMax = 768;
constexpr size_t kServerBlobMax = 2304;
constexpr size_t kAlarmBlobMax = 1024;
constexpr size_t kTodoBlobMax = 2048;
constexpr size_t kDirtyBlobMax = 1024;
constexpr size_t kInboxStateMax = 6144;
constexpr size_t kInboxLegacyMax = 4096;
constexpr size_t kInboxHeadroom = 256;
constexpr size_t kInboxBodyChars = 300;
constexpr size_t kJournalMax = 16384;

class Handle {
public:
    Handle(const char* ns, bool write) {
        if (nvs_open(ns, write ? NVS_READWRITE : NVS_READONLY, &h_) != ESP_OK) {
            h_ = 0;
        }
    }
    ~Handle() {
        if (h_) nvs_close(h_);
    }
    nvs_handle_t get() const { return h_; }
    explicit operator bool() const { return h_ != 0; }

private:
    nvs_handle_t h_ = 0;
};

bool ReadStr(const char* ns, const char* key, std::string* out) {
    Handle h(ns, false);
    if (!h) return false;
    size_t len = 0;
    if (nvs_get_str(h.get(), key, nullptr, &len) != ESP_OK || len == 0) return false;
    std::string s(len, '\0');
    if (nvs_get_str(h.get(), key, s.data(), &len) != ESP_OK) return false;
    s.resize(len - 1);
    *out = s;
    return true;
}

bool WriteStr(const char* ns, const char* key, const std::string& value) {
    Handle h(ns, true);
    if (!h) return false;
    const bool ok = nvs_set_str(h.get(), key, value.c_str()) == ESP_OK &&
                    nvs_commit(h.get()) == ESP_OK;
    if (!ok) ESP_LOGE(kTag, "write %s/%s failed", ns, key);
    return ok;
}

bool Erase(const char* ns, const char* key) {
    Handle h(ns, true);
    if (!h) return false;
    const esp_err_t err = nvs_erase_key(h.get(), key);
    return (err == ESP_OK || err == ESP_ERR_NVS_NOT_FOUND) && nvs_commit(h.get()) == ESP_OK;
}

// Returns the parsed JSON (caller deletes) or nullptr when missing/corrupt.
cJSON* ReadBlob(const char* ns, const char* key, size_t max) {
    Handle h(ns, false);
    if (!h) return nullptr;
    size_t len = 0;
    if (nvs_get_blob(h.get(), key, nullptr, &len) != ESP_OK || len == 0) return nullptr;
    if (len > max) {
        ESP_LOGE(kTag, "%s/%s holds %u bytes (max %u)", ns, key, unsigned(len), unsigned(max));
        return nullptr;
    }
    std::string buf(len, '\0');
    if (nvs_get_blob(h.get(), key, buf.data(), &len) != ESP_OK) return nullptr;
    cJSON* json = cJSON_ParseWithLength(buf.data(), len);
    if (!json) ESP_LOGE(kTag, "%s/%s holds undecodable data", ns, key);
    return json;
}

bool WriteBlob(const char* ns, const char* key, cJSON* json, size_t max) {
    const std::string text = codec::Print(json);
    if (text.empty() || text.size() > max) {
        ESP_LOGE(kTag, "%s/%s blob too large: %u bytes (max %u)", ns, key,
                 unsigned(text.size()), unsigned(max));
        return false;
    }
    Handle h(ns, true);
    if (!h) return false;
    const bool ok = nvs_set_blob(h.get(), key, text.data(), text.size()) == ESP_OK &&
                    nvs_commit(h.get()) == ESP_OK;
    if (!ok) ESP_LOGE(kTag, "write %s/%s failed", ns, key);
    return ok;
}

bool ReadU64(const char* key, uint64_t* out) {
    std::string s;
    if (!ReadStr(kNsMain, key, &s) || s.empty()) return false;
    char* end = nullptr;
    const unsigned long long v = std::strtoull(s.c_str(), &end, 10);
    if (*end != '\0') return false;
    *out = v;
    return true;
}

bool ReadI64(const char* key, int64_t* out) {
    std::string s;
    if (!ReadStr(kNsMain, key, &s) || s.empty()) return false;
    char* end = nullptr;
    const long long v = std::strtoll(s.c_str(), &end, 10);
    if (*end != '\0') return false;
    *out = v;
    return true;
}

// Dirty sets: JSON arrays of ids under "dirty" in each namespace.
std::vector<uint8_t> DirtyIds(const char* ns) {
    std::vector<uint8_t> ids;
    cJSON* json = ReadBlob(ns, "dirty", kDirtyBlobMax);
    if (json) {
        codec::DecodeU8List(json, &ids);
        cJSON_Delete(json);
    }
    return ids;
}

bool MarkDirty(const char* ns, uint8_t id) {
    std::vector<uint8_t> ids = DirtyIds(ns);
    if (std::find(ids.begin(), ids.end(), id) == ids.end()) ids.push_back(id);
    return WriteBlob(ns, "dirty", codec::EncodeU8List(ids), kDirtyBlobMax);
}

bool ClearDirty(const char* ns, const std::vector<uint8_t>& clear) {
    if (clear.empty()) return true;
    std::vector<uint8_t> ids = DirtyIds(ns);
    ids.erase(std::remove_if(ids.begin(), ids.end(),
                             [&](uint8_t id) {
                                 return std::find(clear.begin(), clear.end(), id) != clear.end();
                             }),
              ids.end());
    return WriteBlob(ns, "dirty", codec::EncodeU8List(ids), kDirtyBlobMax);
}

struct InboxState {
    std::vector<InboxItem> items;
    std::vector<uint64_t> pending;
};

InboxState LoadInboxState() {
    InboxState state;
    if (cJSON* json = ReadBlob(kNsInbox, "state_v1", kInboxStateMax)) {
        const cJSON* version = cJSON_GetObjectItemCaseSensitive(json, "version");
        if (cJSON_IsNumber(version) && version->valuedouble == 1) {
            codec::DecodeInbox(cJSON_GetObjectItemCaseSensitive(json, "items"), &state.items);
            codec::DecodeU64List(cJSON_GetObjectItemCaseSensitive(json, "pending_read"),
                                 &state.pending);
        } else {
            ESP_LOGE(kTag, "unsupported inbox state version");
        }
        cJSON_Delete(json);
        return state;
    }
    if (cJSON* json = ReadBlob(kNsInbox, "items", kInboxLegacyMax)) {
        codec::DecodeInbox(json, &state.items);
        cJSON_Delete(json);
    }
    if (cJSON* json = ReadBlob(kNsInbox, "pending", kInboxLegacyMax)) {
        codec::DecodeU64List(json, &state.pending);
        cJSON_Delete(json);
    }
    return state;
}

cJSON* EncodeInboxState(const InboxState& state) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "version", 1);
    cJSON_AddItemToObject(o, "items", codec::EncodeInbox(state.items));
    cJSON_AddItemToObject(o, "pending_read", codec::EncodeU64List(state.pending));
    return o;
}

bool SaveInboxState(const InboxState& state) {
    return WriteBlob(kNsInbox, "state_v1", EncodeInboxState(state), kInboxStateMax);
}

bool Contains(const std::vector<uint64_t>& v, uint64_t x) {
    return std::find(v.begin(), v.end(), x) != v.end();
}

// Truncates to `chars` UTF-8 code points and appends an ellipsis.
std::string TruncateChars(const std::string& s, size_t chars) {
    size_t count = 0;
    for (size_t i = 0; i < s.size(); ++i) {
        if ((static_cast<unsigned char>(s[i]) & 0xC0) != 0x80) {
            if (count == chars) return s.substr(0, i) + "\xE2\x80\xA6";
            ++count;
        }
    }
    return s;
}

}  // namespace

bool Init() {
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        // Never erase silently: that would drop the user's settings. Log and
        // run without persistence instead.
        ESP_LOGE(kTag, "NVS unusable (0x%x); settings will not persist", err);
        return false;
    }
    return err == ESP_OK;
}

bool ValidWifiCreds(const WifiCreds& c) {
    const size_t p = c.password.size();
    bool hex = p == 64;
    for (char ch : c.password) {
        if (!std::isxdigit(static_cast<unsigned char>(ch))) hex = false;
    }
    const bool valid_password = p == 0 || (p >= 8 && p <= 63) || hex;
    return !c.ssid.empty() && c.ssid.size() <= 32 && valid_password;
}

bool ValidServerUrl(const std::string& url) {
    if (url.size() > 240 || url.rfind("https://", 0) != 0) return false;
    const std::string rest = url.substr(8);
    const size_t end = rest.find_first_of("/?#");
    const std::string authority = rest.substr(0, end);
    if (authority.empty() || authority.find('@') != std::string::npos) return false;
    for (char ch : authority) {
        if (std::isspace(static_cast<unsigned char>(ch))) return false;
    }
    return true;
}

bool LoadWifi(WifiCreds* out) {
    WifiCreds c;
    if (cJSON* json = ReadBlob(kNsMain, "wifi_cfg_v1", kWifiBlobMax)) {
        const cJSON* v = cJSON_GetObjectItemCaseSensitive(json, "version");
        const cJSON* ssid = cJSON_GetObjectItemCaseSensitive(json, "ssid");
        const cJSON* pass = cJSON_GetObjectItemCaseSensitive(json, "password");
        const bool ok = cJSON_IsNumber(v) && v->valuedouble == 1 && cJSON_IsString(ssid) &&
                        cJSON_IsString(pass);
        if (ok) {
            c.ssid = ssid->valuestring;
            c.password = pass->valuestring;
        }
        cJSON_Delete(json);
        if (!ok || !ValidWifiCreds(c)) {
            ESP_LOGE(kTag, "stored Wi-Fi config is unusable");
            return false;
        }
        *out = c;
        return true;
    }
    if (!ReadStr(kNsMain, "wifi_ssid", &c.ssid)) return false;
    ReadStr(kNsMain, "wifi_pass", &c.password);
    if (!ValidWifiCreds(c)) return false;
    *out = c;
    return true;
}

bool SaveWifi(const WifiCreds& c) {
    if (!ValidWifiCreds(c)) return false;
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "version", 1);
    cJSON_AddStringToObject(o, "ssid", c.ssid.c_str());
    cJSON_AddStringToObject(o, "password", c.password.c_str());
    return WriteBlob(kNsMain, "wifi_cfg_v1", o, kWifiBlobMax);
}

bool LoadServer(ServerConfig* out) {
    ServerConfig c;
    if (cJSON* json = ReadBlob(kNsMain, "server_cfg_v1", kServerBlobMax)) {
        const cJSON* v = cJSON_GetObjectItemCaseSensitive(json, "version");
        const cJSON* url = cJSON_GetObjectItemCaseSensitive(json, "server_url");
        const cJSON* token = cJSON_GetObjectItemCaseSensitive(json, "auth_token");
        const bool ok = cJSON_IsNumber(v) && v->valuedouble == 1 && cJSON_IsString(url) &&
                        cJSON_IsString(token);
        if (ok) {
            c.url = url->valuestring;
            c.token = token->valuestring;
        }
        cJSON_Delete(json);
        if (!ok || !ValidServerUrl(c.url) || c.token.size() > kMaxTokenLen) {
            ESP_LOGE(kTag, "stored server config is unusable");
            return false;
        }
        *out = c;
        return true;
    }
    if (!ReadStr(kNsMain, "server_url", &c.url)) return false;
    ReadStr(kNsMain, "auth_token", &c.token);
    if (!ValidServerUrl(c.url) || c.token.size() > kMaxTokenLen) return false;
    *out = c;
    return true;
}

bool SaveServer(const ServerConfig& c) {
    if (!ValidServerUrl(c.url) || c.token.size() > kMaxTokenLen) return false;
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "version", 1);
    cJSON_AddStringToObject(o, "server_url", c.url.c_str());
    cJSON_AddStringToObject(o, "auth_token", c.token.c_str());
    return WriteBlob(kNsMain, "server_cfg_v1", o, kServerBlobMax);
}

int16_t LoadTimezone() {
    int64_t v = 0;
    if (!ReadI64("timezone_min", &v) || v < kMinTimezone || v > kMaxTimezone) return 0;
    return static_cast<int16_t>(v);
}

bool SaveTimezone(int16_t minutes) {
    if (minutes < kMinTimezone || minutes > kMaxTimezone) return false;
    return WriteStr(kNsMain, "timezone_min", std::to_string(minutes));
}

uint16_t LoadSyncInterval() {
    int64_t v = 0;
    if (!ReadI64("sync_interval", &v) || v < 1 || v > 1440) return kDefaultSyncInterval;
    return static_cast<uint16_t>(v);
}

bool SaveSyncInterval(uint16_t minutes) {
    if (minutes < 1 || minutes > 1440) return false;
    return WriteStr(kNsMain, "sync_interval", std::to_string(minutes));
}

bool LoadLastSyncEpoch(uint64_t* out) { return ReadU64("last_sync_epoch", out); }
bool SaveLastSyncEpoch(uint64_t e) { return WriteStr(kNsMain, "last_sync_epoch", std::to_string(e)); }
bool LoadRtcAlignEpoch(uint64_t* out) { return ReadU64("rtc_align_epoch", out); }
bool SaveRtcAlignEpoch(uint64_t e) { return WriteStr(kNsMain, "rtc_align_epoch", std::to_string(e)); }
bool ClearRtcAlignEpoch() { return Erase(kNsMain, "rtc_align_epoch"); }

std::string LoadTodoRemindedDate() {
    std::string s;
    ReadStr(kNsMain, "todo_rem_date", &s);
    return s;
}

bool SaveTodoRemindedDate(const std::string& date) {
    return WriteStr(kNsMain, "todo_rem_date", date);
}

std::vector<Alarm> LoadAlarms() {
    std::vector<Alarm> alarms;
    if (cJSON* json = ReadBlob(kNsAlarms, "alarms", kAlarmBlobMax)) {
        if (!codec::DecodeAlarms(json, &alarms)) {
            ESP_LOGE(kTag, "stored alarms are corrupt");
            alarms.clear();
        }
        cJSON_Delete(json);
    }
    schedule::SanitizeAlarms(&alarms);
    return alarms;
}

bool SaveAlarms(const std::vector<Alarm>& a) {
    return WriteBlob(kNsAlarms, "alarms", codec::EncodeAlarms(a), kAlarmBlobMax);
}
bool MarkAlarmDirty(uint8_t id) { return MarkDirty(kNsAlarms, id); }
std::vector<uint8_t> DirtyAlarms() { return DirtyIds(kNsAlarms); }
bool ClearDirtyAlarms(const std::vector<uint8_t>& ids) { return ClearDirty(kNsAlarms, ids); }

std::vector<Todo> LoadTodos() {
    std::vector<Todo> todos;
    if (cJSON* json = ReadBlob(kNsTodos, "todos", kTodoBlobMax)) {
        if (!codec::DecodeTodos(json, &todos)) {
            ESP_LOGE(kTag, "stored todos are corrupt");
            todos.clear();
        }
        cJSON_Delete(json);
    }
    schedule::SanitizeTodos(&todos);
    return todos;
}

bool SaveTodos(const std::vector<Todo>& t) {
    return WriteBlob(kNsTodos, "todos", codec::EncodeTodos(t), kTodoBlobMax);
}
bool MarkTodoDirty(uint8_t id) { return MarkDirty(kNsTodos, id); }
std::vector<uint8_t> DirtyTodos() { return DirtyIds(kNsTodos); }
bool ClearDirtyTodos(const std::vector<uint8_t>& ids) { return ClearDirty(kNsTodos, ids); }

std::vector<InboxItem> LoadInbox() { return LoadInboxState().items; }
std::vector<uint64_t> PendingInboxReads() { return LoadInboxState().pending; }

bool SaveInbox(const std::vector<InboxItem>& items) {
    const InboxState current = LoadInboxState();
    InboxState state;
    // Reads not yet acknowledged survive only for items still unread upstream.
    for (uint64_t seq : current.pending) {
        for (const InboxItem& it : items) {
            if (it.id == seq && !it.read) {
                state.pending.push_back(seq);
                break;
            }
        }
    }
    state.items.assign(items.begin(), items.begin() + std::min(items.size(), kMaxInboxItems));
    for (InboxItem& it : state.items) {
        if (Contains(state.pending, it.id)) it.read = true;
        it.body = TruncateChars(it.body, kInboxBodyChars);
    }
    const size_t budget = kInboxStateMax - kInboxHeadroom;
    while (state.items.size() > 1 && codec::Print(EncodeInboxState(state)).size() > budget) {
        state.items.pop_back();
    }
    state.pending.erase(std::remove_if(state.pending.begin(), state.pending.end(),
                                       [&](uint64_t seq) {
                                           for (const InboxItem& it : state.items) {
                                               if (it.id == seq) return false;
                                           }
                                           return true;
                                       }),
                        state.pending.end());
    if (codec::Print(EncodeInboxState(state)).size() > budget) {
        ESP_LOGE(kTag, "inbox state exceeds storage budget");
        return false;
    }
    return SaveInboxState(state);
}

bool MarkInboxRead(uint64_t id) {
    InboxState state = LoadInboxState();
    bool found = false;
    for (InboxItem& it : state.items) {
        if (it.id == id) {
            it.read = true;
            found = true;
        }
    }
    if (!found) return false;
    if (!Contains(state.pending, id)) state.pending.push_back(id);
    return SaveInboxState(state);
}

bool AckInboxReads(const std::vector<uint64_t>& acked) {
    InboxState state = LoadInboxState();
    state.pending.erase(std::remove_if(state.pending.begin(), state.pending.end(),
                                       [&](uint64_t seq) { return Contains(acked, seq); }),
                        state.pending.end());
    return SaveInboxState(state);
}

bool SaveSyncJournal(const sync_payload::Applied& applied) {
    const std::string text = sync_payload::EncodeApplied(applied);
    if (text.size() > kJournalMax) return false;
    Handle h(kNsMain, true);
    if (!h) return false;
    return nvs_set_blob(h.get(), "sync_jrn_v1", text.data(), text.size()) == ESP_OK &&
           nvs_commit(h.get()) == ESP_OK;
}

bool LoadSyncJournal(sync_payload::Applied* out) {
    Handle h(kNsMain, false);
    if (!h) return false;
    size_t len = 0;
    if (nvs_get_blob(h.get(), "sync_jrn_v1", nullptr, &len) != ESP_OK || len == 0 ||
        len > kJournalMax) {
        return false;
    }
    std::string buf(len, '\0');
    if (nvs_get_blob(h.get(), "sync_jrn_v1", buf.data(), &len) != ESP_OK) return false;
    return sync_payload::DecodeApplied(buf, out);
}

bool ClearSyncJournal() { return Erase(kNsMain, "sync_jrn_v1"); }

}  // namespace store
