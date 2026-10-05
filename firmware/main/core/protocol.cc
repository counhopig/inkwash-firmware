#include "core/protocol.h"

#include <cmath>
#include <cstring>
#include <limits>

#include "cJSON.h"

namespace protocol {
namespace {

bool Str(const cJSON* o, const char* key, std::string* out) {
    const cJSON* v = cJSON_GetObjectItemCaseSensitive(o, key);
    if (!cJSON_IsString(v)) return false;
    *out = v->valuestring;
    return true;
}

bool Num(const cJSON* o, const char* key, double min, double max, double* out) {
    const cJSON* v = cJSON_GetObjectItemCaseSensitive(o, key);
    if (!cJSON_IsNumber(v) || v->valuedouble != std::floor(v->valuedouble) ||
        v->valuedouble < min || v->valuedouble > max) {
        return false;
    }
    *out = v->valuedouble;
    return true;
}

std::string Finish(cJSON* o, const std::string* id) {
    if (id) cJSON_AddStringToObject(o, "id", id->c_str());
    char* text = cJSON_PrintUnformatted(o);
    std::string out = text ? text : R"({"status":"error","message":"Failed to serialize reply"})";
    if (text) cJSON_free(text);
    cJSON_Delete(o);
    return out;
}

cJSON* WithStatus(const char* status) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddStringToObject(o, "status", status);
    return o;
}

}  // namespace

bool NestingExceeds(const std::string& line, int max) {
    int depth = 0;
    bool in_string = false;
    bool escaped = false;
    for (char c : line) {
        if (in_string) {
            if (escaped) escaped = false;
            else if (c == '\\') escaped = true;
            else if (c == '"') in_string = false;
            continue;
        }
        if (c == '"') {
            in_string = true;
        } else if (c == '{' || c == '[') {
            if (++depth > max) return true;
        } else if ((c == '}' || c == ']') && depth > 0) {
            --depth;
        }
    }
    return false;
}

bool Parse(const std::string& line, Command* out, std::string* error) {
    if (NestingExceeds(line, kMaxNesting)) {
        *error = "Failed to parse command: JSON nesting exceeds 4";
        return false;
    }
    cJSON* root = cJSON_Parse(line.c_str());
    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        *error = "Failed to parse command: invalid JSON";
        return false;
    }
    Command c;
    std::string cmd;
    bool ok = Str(root, "cmd", &cmd);
    const cJSON* id = cJSON_GetObjectItemCaseSensitive(root, "id");
    if (cJSON_IsString(id)) {
        c.has_id = true;
        c.id = id->valuestring;
    }
    double n = 0;
    if (!ok) {
        *error = "Failed to parse command: missing field `cmd`";
    } else if (cmd == "set_wifi") {
        c.cmd = Cmd::SetWifi;
        ok = Str(root, "ssid", &c.ssid) && Str(root, "password", &c.password);
    } else if (cmd == "set_server") {
        c.cmd = Cmd::SetServer;
        ok = Str(root, "url", &c.url) && Str(root, "token", &c.token);
    } else if (cmd == "sync_now") {
        c.cmd = Cmd::SyncNow;
    } else if (cmd == "set_rtc") {
        c.cmd = Cmd::SetRtc;
        ok = Num(root, "epoch_secs", 0, 1.8e19, &n);
        c.epoch_secs = static_cast<uint64_t>(n);
    } else if (cmd == "get_status") {
        c.cmd = Cmd::GetStatus;
    } else if (cmd == "get_logs") {
        c.cmd = Cmd::GetLogs;
        const bool cursor = cJSON_GetObjectItemCaseSensitive(root, "cursor") != nullptr;
        const bool anchor = cJSON_GetObjectItemCaseSensitive(root, "anchor") != nullptr;
        const bool snapshot = cJSON_GetObjectItemCaseSensitive(root, "snapshot") != nullptr;
        c.log_resume = cursor || anchor || snapshot;
        if (c.log_resume) {
            std::string text;
            ok = Num(root, "cursor", 0, 8192, &n);
            c.log_cursor = static_cast<uint32_t>(n);
            ok = ok && Num(root, "anchor", 0, 8191, &n);
            c.log_anchor = static_cast<uint32_t>(n);
            ok = ok && Str(root, "snapshot", &text) && !text.empty() && text.size() <= 20;
            for (char digit : text) {
                if (digit < '0' || digit > '9' || c.log_snapshot >
                    (std::numeric_limits<uint64_t>::max() - (digit - '0')) / 10) {
                    ok = false;
                    break;
                }
                c.log_snapshot = c.log_snapshot * 10 + (digit - '0');
            }
        }
    } else if (cmd == "clear_alarms") {
        c.cmd = Cmd::ClearAlarms;
    } else if (cmd == "set_timezone") {
        c.cmd = Cmd::SetTimezone;
        ok = Num(root, "offset_minutes", -32768, 32767, &n);
        c.offset_minutes = static_cast<int16_t>(n);
    } else {
        ok = false;
        *error = "Failed to parse command: unknown variant `" + cmd + "`";
    }
    if (!ok && error->empty()) {
        *error = "Failed to parse command: missing or invalid field";
    }
    cJSON_Delete(root);
    if (ok) *out = c;
    return ok;
}

std::string ReplyOk(const std::string* id) { return Finish(WithStatus("ok"), id); }
std::string ReplyBusy(const std::string* id) { return Finish(WithStatus("busy"), id); }

std::string ReplyError(const std::string& message, const std::string* id) {
    cJSON* o = WithStatus("error");
    cJSON_AddStringToObject(o, "message", message.c_str());
    return Finish(o, id);
}

std::string ReplyStatus(const Status& s, const std::string* id) {
    cJSON* o = WithStatus("status");
    cJSON_AddBoolToObject(o, "wifi_configured", s.wifi_configured);
    cJSON_AddBoolToObject(o, "server_configured", s.server_configured);
    cJSON_AddBoolToObject(o, "wifi_connected", s.wifi_connected);
    if (s.has_ssid) cJSON_AddStringToObject(o, "wifi_ssid", s.wifi_ssid.c_str());
    else cJSON_AddNullToObject(o, "wifi_ssid");
    cJSON_AddBoolToObject(o, "wifi_has_password", s.wifi_has_password);
    if (s.has_server_url) cJSON_AddStringToObject(o, "server_url", s.server_url.c_str());
    else cJSON_AddNullToObject(o, "server_url");
    cJSON_AddBoolToObject(o, "server_has_token", s.server_has_token);
    cJSON_AddNumberToObject(o, "timezone_offset_minutes", s.timezone_offset_minutes);
    return Finish(o, id);
}

}  // namespace protocol
