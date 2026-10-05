#include "app/internal.h"

namespace app::detail {

// ---- Helpers -------------------------------------------------------------------

void ShowNotice(const std::string& text) {
    g.notice.reset(new Notice{text, NowMs() + kNoticeMs});
}

void ShowStickyNotice(const std::string& text) {
    g.notice.reset(new Notice{text, -1});
}

const char* KeyHint(ScreenKind k) {
    switch (k) {
        case ScreenKind::Home: return "HOLD UP OR DOWN FOR MENU";
        case ScreenKind::BlePairing: return "HOLD ENTER TO CANCEL PAIRING";
        case ScreenKind::AlarmRinging: return "PRESS ENTER TO STOP";
        case ScreenKind::About: return "PRESS ENTER TO GO BACK";
        case ScreenKind::Navigation: return "UP/DOWN MOVE  ENTER OPEN  HOLD ENTER BACK";
        default: return "UP/DOWN MOVE  ENTER OK  HOLD ENTER BACK";
    }
}

void LoadConfig() {
    store::WifiCreds creds;
    g.wifi_configured = store::LoadWifi(&creds);
    g.wifi_ssid = creds.ssid;
    g.wifi_has_password = !creds.password.empty();
    store::ServerConfig server;
    g.server_configured = store::LoadServer(&server);
    g.server_url = server.url;
    g.server_has_token = !server.token.empty();
    g.timezone = store::LoadTimezone();
    g.sync_interval = store::LoadSyncInterval();
}

void LoadData() {
    g.alarms = store::LoadAlarms();
    g.todos = store::LoadTodos();
    g.inbox = store::LoadInbox();
}

bool ReadClock() {
    DateTime dt;
    if (pcf8563::ReadTime(&dt) && !dt.voltage_low) {
        g.now = dt;
        g.have_clock = true;
    }
    return g.have_clock;
}

uint64_t Boundary(uint64_t unix, uint64_t period) { return unix / period; }

void SeedScheduler() {
    const uint64_t unix = g.have_clock ? g.now.ToUnix() : 0;
    uint64_t last_sync = 0;
    const bool synced = store::LoadLastSyncEpoch(&last_sync);
    g.never_synced = !synced;
    const uint64_t period = std::max<uint64_t>(1, g.sync_interval) * 60;
    // Seed from the last successful sync so a boundary slept through syncs at once.
    g.last_full_boundary = Boundary(synced ? last_sync : unix, period);
    g.last_urgent_boundary = Boundary(unix, kUrgentPeriodSecs);
}


}  // namespace app::detail
