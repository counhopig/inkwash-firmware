// Sync request/response bodies (rust-firmware/src/sync.rs,
// logic/src/sync_validate.rs).
#pragma once

#include <string>
#include <vector>

#include "core/model.h"

namespace sync_payload {

struct Upload {
    std::vector<std::pair<uint8_t, bool>> alarms;  // id, enabled (dirty only)
    std::vector<std::pair<uint8_t, bool>> todos;   // id, done (dirty only)
    std::vector<uint64_t> inbox_read;
};

std::string EncodeUpload(const Upload& upload);

struct Response {
    std::vector<Alarm> alarms;
    std::vector<Todo> todos;
    std::vector<InboxItem> inbox;
    std::vector<uint64_t> inbox_read_acked;
    bool inbox_truncated = false;
};

// Parses and validates; returns an empty string on success, else the reason.
std::string DecodeResponse(const char* body, size_t len, Response* out);

// Validation alone (also used on the journal copy).
std::string Validate(const Response& response);

// The applied result, journaled before it is written so a reboot mid-apply
// can finish the job (logic/src/app.rs SyncedData).
struct Applied {
    Response data;
    std::vector<uint8_t> uploaded_alarm_ids;
    std::vector<uint8_t> uploaded_todo_ids;
};
std::string EncodeApplied(const Applied& applied);
bool DecodeApplied(const std::string& json, Applied* out);

}  // namespace sync_payload
