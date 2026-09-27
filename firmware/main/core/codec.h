// JSON for the model, byte-compatible with the Rust firmware's serde output
// (NVS blobs) and with the server's sync payloads.
#pragma once

#include <string>
#include <vector>

#include "core/model.h"

struct cJSON;

namespace codec {

cJSON* EncodeRepeat(const Repeat& repeat);
bool DecodeRepeat(const cJSON* json, Repeat* out);

cJSON* EncodeAlarm(const Alarm& alarm);
bool DecodeAlarm(const cJSON* json, Alarm* out);
cJSON* EncodeTodo(const Todo& todo);
bool DecodeTodo(const cJSON* json, Todo* out);
cJSON* EncodeInboxItem(const InboxItem& item);
bool DecodeInboxItem(const cJSON* json, InboxItem* out);

cJSON* EncodeAlarms(const std::vector<Alarm>& alarms);
bool DecodeAlarms(const cJSON* json, std::vector<Alarm>* out);
cJSON* EncodeTodos(const std::vector<Todo>& todos);
bool DecodeTodos(const cJSON* json, std::vector<Todo>* out);
cJSON* EncodeInbox(const std::vector<InboxItem>& items);
bool DecodeInbox(const cJSON* json, std::vector<InboxItem>* out);

cJSON* EncodeU64List(const std::vector<uint64_t>& ids);
bool DecodeU64List(const cJSON* json, std::vector<uint64_t>* out);
cJSON* EncodeU8List(const std::vector<uint8_t>& ids);
bool DecodeU8List(const cJSON* json, std::vector<uint8_t>* out);

// Serializes compactly (no whitespace), as serde_json::to_vec does, and frees json.
std::string Print(cJSON* json);

}  // namespace codec
