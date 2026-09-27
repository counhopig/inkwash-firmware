// Server sync (rust-firmware/src/{sync,sync_apply}.rs).
#pragma once

#include <cstdint>
#include <string>

#include "core/datetime.h"
#include "storage/store.h"

namespace netsync {

struct Result {
    bool ok = false;
    std::string error;
    // Set when a daily RTC alignment was due and NTP answered.
    bool have_ntp = false;
    uint64_t ntp_utc = 0;
};

// Uploads dirty alarm/todo states and inbox reads, downloads the server's
// lists and applies them through the journal. Connects and disconnects Wi-Fi.
Result Run(const DateTime& now);

// Asks the server whether an urgent inbox item is waiting ("x-inkwash-poll").
Result PollUrgent(bool* urgent);

// Joins the network with new credentials to prove they work.
Result VerifyWifi(const store::WifiCreds& creds);

// Finishes an apply that a reset interrupted. Returns true when it replayed one.
bool RecoverJournal();

// NTP alone (boot with an RTC that lost power).
Result NtpOnly();

}  // namespace netsync
