#include "core/sync_payload.h"

#include <set>

#include "cJSON.h"
#include "core/codec.h"
#include "core/schedule.h"

namespace sync_payload {
namespace {

cJSON* StateList(const std::vector<std::pair<uint8_t, bool>>& list, const char* flag) {
    cJSON* arr = cJSON_CreateArray();
    for (const auto& [id, value] : list) {
        cJSON* o = cJSON_CreateObject();
        cJSON_AddNumberToObject(o, "id", id);
        cJSON_AddBoolToObject(o, flag, value);
        cJSON_AddItemToArray(arr, o);
    }
    return arr;
}

bool DecodeFields(const cJSON* root, Response* r) {
    const cJSON* v = nullptr;
    if ((v = cJSON_GetObjectItemCaseSensitive(root, "alarms")) && !cJSON_IsNull(v) &&
        !codec::DecodeAlarms(v, &r->alarms)) {
        return false;
    }
    if ((v = cJSON_GetObjectItemCaseSensitive(root, "todos")) && !cJSON_IsNull(v) &&
        !codec::DecodeTodos(v, &r->todos)) {
        return false;
    }
    if ((v = cJSON_GetObjectItemCaseSensitive(root, "inbox")) && !cJSON_IsNull(v) &&
        !codec::DecodeInbox(v, &r->inbox)) {
        return false;
    }
    if ((v = cJSON_GetObjectItemCaseSensitive(root, "inbox_read_acked")) && !cJSON_IsNull(v) &&
        !codec::DecodeU64List(v, &r->inbox_read_acked)) {
        return false;
    }
    if ((v = cJSON_GetObjectItemCaseSensitive(root, "inbox_truncated")) && !cJSON_IsNull(v)) {
        if (!cJSON_IsBool(v)) return false;
        r->inbox_truncated = cJSON_IsTrue(v);
    }
    return true;
}

}  // namespace

std::string EncodeUpload(const Upload& upload) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddItemToObject(o, "alarms", StateList(upload.alarms, "enabled"));
    cJSON_AddItemToObject(o, "todos", StateList(upload.todos, "done"));
    cJSON_AddItemToObject(o, "inbox_read", codec::EncodeU64List(upload.inbox_read));
    return codec::Print(o);
}

std::string Validate(const Response& r) {
    std::set<uint64_t> ids;
    for (const Alarm& a : r.alarms) {
        if (!ids.insert(a.id).second) return "duplicate alarm id " + std::to_string(a.id);
        if (a.hour > 23 || a.minute > 59) {
            return "alarm " + std::to_string(a.id) + " has invalid time";
        }
        const std::string err = schedule::ValidateRepeat(a.repeat);
        if (!err.empty()) return "alarm " + std::to_string(a.id) + " has invalid repeat: " + err;
    }
    ids.clear();
    for (const Todo& t : r.todos) {
        if (!ids.insert(t.id).second) return "duplicate todo id " + std::to_string(t.id);
        if (t.has_due && !schedule::ValidDate(t.due.year, t.due.month, t.due.day)) {
            return "todo " + std::to_string(t.id) + " has invalid due date";
        }
        if (t.has_repeat) {
            if (t.repeat.kind == Repeat::Kind::Once) {
                return "todo " + std::to_string(t.id) + " uses unsupported Once repeat";
            }
            const std::string err = schedule::ValidateRepeat(t.repeat);
            if (!err.empty()) return "todo " + std::to_string(t.id) + " has invalid repeat: " + err;
        }
    }
    ids.clear();
    for (const InboxItem& it : r.inbox) {
        if (!ids.insert(it.id).second) return "duplicate inbox id " + std::to_string(it.id);
    }
    if (codec::Print(codec::EncodeAlarms(r.alarms)).size() > 1024) {
        return "alarm list exceeds device storage capacity";
    }
    if (codec::Print(codec::EncodeTodos(r.todos)).size() > 2048) {
        return "todo list exceeds device storage capacity";
    }
    return "";
}

std::string DecodeResponse(const char* body, size_t len, Response* out) {
    cJSON* root = cJSON_ParseWithLength(body, len);
    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        return "sync response JSON decode failed";
    }
    Response r;
    const bool ok = DecodeFields(root, &r);
    cJSON_Delete(root);
    if (!ok) return "sync response JSON decode failed";
    const std::string err = Validate(r);
    if (!err.empty()) return "sync response validation failed: " + err;
    *out = std::move(r);
    return "";
}

std::string EncodeApplied(const Applied& a) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddItemToObject(o, "alarms", codec::EncodeAlarms(a.data.alarms));
    cJSON_AddItemToObject(o, "todos", codec::EncodeTodos(a.data.todos));
    cJSON_AddItemToObject(o, "inbox", codec::EncodeInbox(a.data.inbox));
    cJSON_AddItemToObject(o, "inbox_read_acked", codec::EncodeU64List(a.data.inbox_read_acked));
    cJSON_AddBoolToObject(o, "inbox_truncated", a.data.inbox_truncated);
    cJSON_AddItemToObject(o, "uploaded_alarm_ids", codec::EncodeU8List(a.uploaded_alarm_ids));
    cJSON_AddItemToObject(o, "uploaded_todo_ids", codec::EncodeU8List(a.uploaded_todo_ids));
    return codec::Print(o);
}

bool DecodeApplied(const std::string& json, Applied* out) {
    cJSON* root = cJSON_Parse(json.c_str());
    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        return false;
    }
    Applied a;
    bool ok = DecodeFields(root, &a.data);
    const cJSON* v = cJSON_GetObjectItemCaseSensitive(root, "uploaded_alarm_ids");
    ok = ok && v && codec::DecodeU8List(v, &a.uploaded_alarm_ids);
    v = cJSON_GetObjectItemCaseSensitive(root, "uploaded_todo_ids");
    ok = ok && v && codec::DecodeU8List(v, &a.uploaded_todo_ids);
    cJSON_Delete(root);
    if (ok) *out = std::move(a);
    return ok;
}

}  // namespace sync_payload
