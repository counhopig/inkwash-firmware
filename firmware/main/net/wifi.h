// Station-mode Wi-Fi for short sessions: connect, do the work, disconnect.
#pragma once

#include <cstdint>
#include <string>

#include "storage/store.h"

namespace wifi {

// Associated with the access point and holding an IP address.
bool IsConnected();

// Connects and waits for DHCP (20 s budget). On failure *error says why.
bool Connect(const store::WifiCreds& creds, std::string* error);

// Disconnects and stops the radio.
void Disconnect();

// True once the Wi-Fi driver has been initialized in this boot. BLE pairing
// restarts onto a clean heap when this is set (see main/app/ble.cc).
bool UsedThisBoot();

// SNTP against pool.ntp.org / ntp.aliyun.com; UTC seconds on success.
bool NtpEpoch(uint64_t* out_utc);

// HTTPS POST with the Bearer token; reads the body into *body (at most
// max_len bytes; a longer body is an error). Returns false with *error set.
bool HttpsPost(const std::string& url, const std::string& token, const char* extra_header,
               const std::string& request, size_t max_len, std::string* body,
               std::string* error);

}  // namespace wifi
