#include "core/codec.h"

#include <cmath>
#include <cstring>

#include "cJSON.h"

namespace codec {
namespace {

bool GetNumber(const cJSON* obj, const char* key, double* out) {
    const cJSON* item = cJSON_GetObjectItemCaseSensitive(obj, key);
    if (!cJSON_IsNumber(item)) {
        return false;
    }
    *out = item->valuedouble;
    return true;
}

template <typename T>
bool GetInt(const cJSON* obj, const char* key, T min, T max, T* out) {
    double v = 0;
    if (!GetNumber(obj, key, &v) || v != std::floor(v) || v < static_cast<double>(min) ||
        v > static_cast<double>(max)) {
        return false;
    }
    *out = static_cast<T>(v);
    return true;
}

bool GetBool(const cJSON* obj, const char* key, bool* out) {
    const cJSON* item = cJSON_GetObjectItemCaseSensitive(obj, key);
    if (!cJSON_IsBool(item)) {
        return false;
    }
    *out = cJSON_IsTrue(item);
    return true;
}

bool GetString(const cJSON* obj, const char* key, std::string* out) {
    const cJSON* item = cJSON_GetObjectItemCaseSensitive(obj, key);
    if (!cJSON_IsString(item)) {
        return false;
    }
    *out = item->valuestring;
    return true;
}

// serde(default): a missing (or null) field keeps the default; a present
// field of the wrong type is an error.
bool Present(const cJSON* obj, const char* key) {
    const cJSON* item = cJSON_GetObjectItemCaseSensitive(obj, key);
    return item != nullptr && !cJSON_IsNull(item);
}

bool DecodeDays(const cJSON* obj, std::vector<uint8_t>* out) {
    const cJSON* days = cJSON_GetObjectItemCaseSensitive(obj, "days");
    if (!cJSON_IsArray(days)) {
        return false;
    }
    out->clear();
    const cJSON* d = nullptr;
    cJSON_ArrayForEach(d, days) {
        if (!cJSON_IsNumber(d) || d->valuedouble < 0 || d->valuedouble > 255 ||
            d->valuedouble != std::floor(d->valuedouble)) {
            return false;
        }
        out->push_back(static_cast<uint8_t>(d->valuedouble));
    }
    return true;
}

cJSON* DaysArray(const std::vector<uint8_t>& days) {
    cJSON* arr = cJSON_CreateArray();
    for (uint8_t d : days) {
        cJSON_AddItemToArray(arr, cJSON_CreateNumber(d));
    }
    return arr;
}

const char* ImportanceName(Importance i) {
    switch (i) {
        case Importance::Low: return "low";
        case Importance::Medium: return "medium";
        case Importance::High: return "high";
    }
    return "medium";
}

}  // namespace

cJSON* EncodeRepeat(const Repeat& r) {
    switch (r.kind) {
        case Repeat::Kind::Daily:
            return cJSON_CreateString("Daily");
        case Repeat::Kind::Weekly:
        case Repeat::Kind::Monthly: {
            cJSON* outer = cJSON_CreateObject();
            cJSON* inner = cJSON_CreateObject();
            cJSON_AddItemToObject(inner, "days", DaysArray(r.days));
            cJSON_AddItemToObject(outer, r.kind == Repeat::Kind::Weekly ? "Weekly" : "Monthly",
                                  inner);
            return outer;
        }
        case Repeat::Kind::Once: {
            cJSON* outer = cJSON_CreateObject();
            cJSON* inner = cJSON_CreateObject();
            cJSON_AddNumberToObject(inner, "year", r.year);
            cJSON_AddNumberToObject(inner, "month", r.month);
            cJSON_AddNumberToObject(inner, "day", r.day);
            cJSON_AddItemToObject(outer, "Once", inner);
            return outer;
        }
    }
    return cJSON_CreateString("Daily");
}

bool DecodeRepeat(const cJSON* json, Repeat* out) {
    Repeat r;
    if (cJSON_IsString(json)) {
        if (std::strcmp(json->valuestring, "Daily") != 0) {
            return false;
        }
        r.kind = Repeat::Kind::Daily;
        *out = r;
        return true;
    }
    if (!cJSON_IsObject(json) || cJSON_GetArraySize(json) != 1) {
        return false;
    }
    const cJSON* inner = json->child;
    if (std::strcmp(inner->string, "Daily") == 0 && (cJSON_IsNull(inner) || cJSON_IsObject(inner))) {
        r.kind = Repeat::Kind::Daily;
    } else if (std::strcmp(inner->string, "Weekly") == 0) {
        r.kind = Repeat::Kind::Weekly;
        if (!DecodeDays(inner, &r.days)) return false;
    } else if (std::strcmp(inner->string, "Monthly") == 0) {
        r.kind = Repeat::Kind::Monthly;
        if (!DecodeDays(inner, &r.days)) return false;
    } else if (std::strcmp(inner->string, "Once") == 0) {
        r.kind = Repeat::Kind::Once;
        if (!GetInt<uint16_t>(inner, "year", 0, 65535, &r.year) ||
            !GetInt<uint8_t>(inner, "month", 0, 255, &r.month) ||
            !GetInt<uint8_t>(inner, "day", 0, 255, &r.day)) {
            return false;
        }
    } else {
        return false;
    }
    *out = r;
    return true;
}

cJSON* EncodeAlarm(const Alarm& a) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "id", a.id);
    cJSON_AddNumberToObject(o, "hour", a.hour);
    cJSON_AddNumberToObject(o, "minute", a.minute);
    cJSON_AddItemToObject(o, "repeat", EncodeRepeat(a.repeat));
    cJSON_AddBoolToObject(o, "enabled", a.enabled);
    cJSON_AddStringToObject(o, "label", a.label.c_str());
    return o;
}

bool DecodeAlarm(const cJSON* json, Alarm* out) {
    Alarm a;
    if (!cJSON_IsObject(json) || !GetInt<uint8_t>(json, "id", 0, 255, &a.id) ||
        !GetInt<uint8_t>(json, "hour", 0, 255, &a.hour) ||
        !GetInt<uint8_t>(json, "minute", 0, 255, &a.minute) ||
        !DecodeRepeat(cJSON_GetObjectItemCaseSensitive(json, "repeat"), &a.repeat) ||
        !GetBool(json, "enabled", &a.enabled)) {
        return false;
    }
    if (Present(json, "label") && !GetString(json, "label", &a.label)) {
        return false;
    }
    *out = a;
    return true;
}

cJSON* EncodeTodo(const Todo& t) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "id", t.id);
    cJSON_AddStringToObject(o, "text", t.text.c_str());
    cJSON_AddBoolToObject(o, "done", t.done);
    cJSON_AddStringToObject(o, "importance", ImportanceName(t.importance));
    if (t.has_due) {
        cJSON* due = cJSON_CreateObject();
        cJSON_AddNumberToObject(due, "year", t.due.year);
        cJSON_AddNumberToObject(due, "month", t.due.month);
        cJSON_AddNumberToObject(due, "day", t.due.day);
        cJSON_AddItemToObject(o, "due_date", due);
    } else {
        cJSON_AddNullToObject(o, "due_date");
    }
    if (t.has_repeat) {
        cJSON_AddItemToObject(o, "repeat", EncodeRepeat(t.repeat));
    } else {
        cJSON_AddNullToObject(o, "repeat");
    }
    return o;
}

bool DecodeTodo(const cJSON* json, Todo* out) {
    Todo t;
    if (!cJSON_IsObject(json) || !GetInt<uint8_t>(json, "id", 0, 255, &t.id) ||
        !GetString(json, "text", &t.text) || !GetBool(json, "done", &t.done)) {
        return false;
    }
    if (Present(json, "importance")) {
        std::string imp;
        if (!GetString(json, "importance", &imp)) return false;
        if (imp == "low") t.importance = Importance::Low;
        else if (imp == "medium") t.importance = Importance::Medium;
        else if (imp == "high") t.importance = Importance::High;
        else return false;
    }
    if (Present(json, "due_date")) {
        const cJSON* due = cJSON_GetObjectItemCaseSensitive(json, "due_date");
        if (!cJSON_IsObject(due)) return false;
        if (Present(due, "year") && !GetInt<uint16_t>(due, "year", 0, 65535, &t.due.year)) {
            return false;
        }
        if (!GetInt<uint8_t>(due, "month", 0, 255, &t.due.month) ||
            !GetInt<uint8_t>(due, "day", 0, 255, &t.due.day)) {
            return false;
        }
        t.has_due = true;
    }
    if (Present(json, "repeat")) {
        if (!DecodeRepeat(cJSON_GetObjectItemCaseSensitive(json, "repeat"), &t.repeat)) {
            return false;
        }
        t.has_repeat = true;
    }
    *out = t;
    return true;
}

cJSON* EncodeInboxItem(const InboxItem& it) {
    cJSON* o = cJSON_CreateObject();
    cJSON_AddNumberToObject(o, "id", static_cast<double>(it.id));
    cJSON_AddStringToObject(o, "kind", it.kind == InboxKind::Alert   ? "alert"
                                       : it.kind == InboxKind::Event ? "event"
                                                                     : "info");
    cJSON_AddStringToObject(o, "priority", it.priority == Priority::High ? "high" : "normal");
    cJSON_AddStringToObject(o, "title", it.title.c_str());
    cJSON_AddStringToObject(o, "body", it.body.c_str());
    if (it.has_when) {
        cJSON_AddNumberToObject(o, "when", static_cast<double>(it.when));
    } else {
        cJSON_AddNullToObject(o, "when");
    }
    cJSON_AddBoolToObject(o, "read", it.read);
    return o;
}

bool DecodeInboxItem(const cJSON* json, InboxItem* out) {
    InboxItem it;
    double id = 0;
    std::string kind;
    if (!cJSON_IsObject(json) || !GetNumber(json, "id", &id) || id < 0 ||
        id != std::floor(id) || !GetString(json, "kind", &kind) ||
        !GetString(json, "title", &it.title)) {
        return false;
    }
    it.id = static_cast<uint64_t>(id);
    if (kind == "alert") it.kind = InboxKind::Alert;
    else if (kind == "event") it.kind = InboxKind::Event;
    else if (kind == "info") it.kind = InboxKind::Info;
    else return false;
    if (Present(json, "priority")) {
        std::string p;
        if (!GetString(json, "priority", &p)) return false;
        if (p == "high") it.priority = Priority::High;
        else if (p == "normal") it.priority = Priority::Normal;
        else return false;
    }
    if (Present(json, "body") && !GetString(json, "body", &it.body)) return false;
    if (Present(json, "when")) {
        double w = 0;
        if (!GetNumber(json, "when", &w)) return false;
        it.has_when = true;
        it.when = static_cast<int64_t>(w);
    }
    if (Present(json, "read") && !GetBool(json, "read", &it.read)) return false;
    *out = it;
    return true;
}

namespace {

template <typename T, typename Enc>
cJSON* EncodeList(const std::vector<T>& list, Enc enc) {
    cJSON* arr = cJSON_CreateArray();
    for (const T& v : list) {
        cJSON_AddItemToArray(arr, enc(v));
    }
    return arr;
}

template <typename T, typename Dec>
bool DecodeList(const cJSON* json, std::vector<T>* out, Dec dec) {
    if (!cJSON_IsArray(json)) {
        return false;
    }
    std::vector<T> result;
    const cJSON* item = nullptr;
    cJSON_ArrayForEach(item, json) {
        T v;
        if (!dec(item, &v)) {
            return false;
        }
        result.push_back(std::move(v));
    }
    *out = std::move(result);
    return true;
}

}  // namespace

cJSON* EncodeAlarms(const std::vector<Alarm>& v) { return EncodeList(v, EncodeAlarm); }
bool DecodeAlarms(const cJSON* j, std::vector<Alarm>* o) { return DecodeList(j, o, DecodeAlarm); }
cJSON* EncodeTodos(const std::vector<Todo>& v) { return EncodeList(v, EncodeTodo); }
bool DecodeTodos(const cJSON* j, std::vector<Todo>* o) { return DecodeList(j, o, DecodeTodo); }
cJSON* EncodeInbox(const std::vector<InboxItem>& v) { return EncodeList(v, EncodeInboxItem); }
bool DecodeInbox(const cJSON* j, std::vector<InboxItem>* o) {
    return DecodeList(j, o, DecodeInboxItem);
}

cJSON* EncodeU64List(const std::vector<uint64_t>& ids) {
    return EncodeList(ids, [](uint64_t v) { return cJSON_CreateNumber(static_cast<double>(v)); });
}

bool DecodeU64List(const cJSON* json, std::vector<uint64_t>* out) {
    return DecodeList(json, out, [](const cJSON* j, uint64_t* v) {
        if (!cJSON_IsNumber(j) || j->valuedouble < 0 || j->valuedouble != std::floor(j->valuedouble)) {
            return false;
        }
        *v = static_cast<uint64_t>(j->valuedouble);
        return true;
    });
}

cJSON* EncodeU8List(const std::vector<uint8_t>& ids) {
    return EncodeList(ids, [](uint8_t v) { return cJSON_CreateNumber(v); });
}

bool DecodeU8List(const cJSON* json, std::vector<uint8_t>* out) {
    return DecodeList(json, out, [](const cJSON* j, uint8_t* v) {
        if (!cJSON_IsNumber(j) || j->valuedouble < 0 || j->valuedouble > 255 ||
            j->valuedouble != std::floor(j->valuedouble)) {
            return false;
        }
        *v = static_cast<uint8_t>(j->valuedouble);
        return true;
    });
}

std::string Print(cJSON* json) {
    std::string out;
    if (json) {
        char* text = cJSON_PrintUnformatted(json);
        if (text) {
            out = text;
            cJSON_free(text);
        }
        cJSON_Delete(json);
    }
    return out;
}

}  // namespace codec
