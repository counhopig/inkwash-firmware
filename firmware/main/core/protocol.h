// USB/BLE control protocol, identical to logic/src/protocol.rs and
// rust-firmware/src/control.rs: one JSON object per command with a "cmd"
// tag, one JSON object per reply with a "status" tag, echoing "id".
#pragma once

#include <cstdint>
#include <string>

namespace protocol {

enum class Cmd : uint8_t { SetWifi, SetServer, SyncNow, SetRtc, GetStatus, ClearAlarms, SetTimezone };

struct Command {
    Cmd cmd = Cmd::GetStatus;
    bool has_id = false;
    std::string id;
    std::string ssid, password;  // SetWifi
    std::string url, token;      // SetServer
    uint64_t epoch_secs = 0;     // SetRtc
    int16_t offset_minutes = 0;  // SetTimezone
};

// Parses one command line (without the USB ">>IW " prefix). On failure
// returns false and sets *error.
bool Parse(const std::string& line, Command* out, std::string* error);

struct Status {
    bool wifi_configured = false;
    bool server_configured = false;
    bool wifi_connected = false;
    bool has_ssid = false;
    std::string wifi_ssid;
    bool wifi_has_password = false;
    bool has_server_url = false;
    std::string server_url;
    bool server_has_token = false;
    int16_t timezone_offset_minutes = 0;
};

std::string ReplyOk(const std::string* id);
std::string ReplyBusy(const std::string* id);
std::string ReplyError(const std::string& message, const std::string* id);
std::string ReplyStatus(const Status& status, const std::string* id);

constexpr int kMaxNesting = 4;
bool NestingExceeds(const std::string& line, int max);

}  // namespace protocol
