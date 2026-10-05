#include "app/internal.h"

namespace app::detail {

// ---- BLE pairing ----------------------------------------------------------------------------

bool OnBleLine(const std::string& line) {
    auto* e = new Event{Event::Kind::Command};
    e->channel = Channel::Ble;
    e->line = line;
    return Post(e, 0);
}

void OnBleEvent(ble::Event ev) {
    // Link state is coalesced; the NimBLE task must never wait on the app
    // while the app is stopping NimBLE.
    g_ble_link.store(static_cast<int>(ev));
}

void EnterBlePairing() {
    if (wifi::UsedThisBoot()) {
        // A torn-down Wi-Fi driver leaves the heap too fragmented for BLE:
        // restart and open pairing before Wi-Fi exists.
        power::State().open_ble_pairing = 1;
        power::Restart();
    }
    if (g.net_job != NetJob::None) {
        ShowNotice("NETWORK BUSY, TRY AGAIN");
        return;
    }
    uint32_t passkey = 0;
    if (!ble::Start(&passkey, OnBleLine, OnBleEvent)) {
        ShowNotice("BLE FAILED: could not start the BLE stack");
        g.screen = Screen{ScreenKind::Settings, ui::kSettingsBlePairing};
        return;
    }
    g.passkey = passkey;
    g.have_passkey = true;
    g.ble_deadline_ms = NowMs() + kBlePairingTimeoutMs;
    g.screen = Screen{ScreenKind::BlePairing};
}

void EndBlePairing(const std::string& notice) {
    ble::Stop();
    g.ble_finish_pending = false;
    g.ble_reply.active = false;
    g.have_passkey = false;
    g.screen = Screen{ScreenKind::Settings, ui::kSettingsBlePairing};
    if (!notice.empty()) ShowNotice(notice);
}


}  // namespace app::detail
