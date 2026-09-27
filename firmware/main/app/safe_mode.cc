#include "app/safe_mode.h"

#include <cstdio>
#include <string>

#include "board.h"
#include "control/usb_console.h"
#include "core/protocol.h"
#include "display.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "fonts.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "power/power.h"
#include "ui/screens.h"

namespace safe_mode {
namespace {

constexpr char kTag[] = "safe_mode";
// Without a USB host, give up waiting after this and sleep until Enter, so a
// boot loop does not drain the battery.
constexpr int64_t kIdleBeforeSleepMs = 5 * 60 * 1000;

int64_t NowMs() { return esp_timer_get_time() / 1000; }

void OnLine(const std::string& line) {
    protocol::Command cmd;
    std::string error;
    if (!protocol::Parse(line, &cmd, &error)) {
        usb_console::Reply(protocol::ReplyError(error, nullptr));
        return;
    }
    const std::string* id = cmd.has_id ? &cmd.id : nullptr;
    if (cmd.cmd == protocol::Cmd::GetStatus) {
        usb_console::Reply(protocol::ReplyStatus(protocol::Status{}, id));
    } else {
        usb_console::Reply(protocol::ReplyError("device is in safe mode (BOOT LOOP DETECTED); command rejected", id));
    }
}

}  // namespace

void Run(const boot_guard::Ledger& ledger, boot_guard::ResetKind last_reset) {
    char reason[96];
    std::snprintf(reason, sizeof(reason), "%u FAILED BOOTS IN A ROW, LAST: %s",
                  unsigned(ledger.failures), boot_guard::Label(last_reset));
    ESP_LOGE(kTag, "entering safe mode: %s", reason);

    usb_console::Start(OnLine);
    if (display::Init()) {
        fonts::Init();
        lv_obj_t* s = lv_obj_create(nullptr);
        lv_obj_remove_style_all(s);
        lv_obj_set_style_bg_opa(s, LV_OPA_COVER, 0);
        lv_obj_set_style_bg_color(s, lv_color_white(), 0);
        ui::DrawSafeMode(s, reason);
        lv_screen_load(s);
        display::Update(display::Refresh::Full);
    }

    // The key that may be held at boot must be released before it counts.
    bool armed = !board::KeyDown(board::Key::Enter);
    int64_t last_activity = NowMs();
    while (true) {
        vTaskDelay(pdMS_TO_TICKS(50));
        const bool down = board::KeyDown(board::Key::Enter);
        if (!armed) {
            armed = !down;
        } else if (down) {
            // A restart through deep sleep starts a fresh run (not a failure).
            ESP_LOGI(kTag, "Enter pressed; retrying a normal boot");
            power::Restart();
        }
        if (usb_console::HostConnected()) last_activity = NowMs();
        if (NowMs() - last_activity >= kIdleBeforeSleepMs) {
            // Waking with Enter is a deep-sleep reset: a fresh, normal boot.
            power::DeepSleepUntilEnter();
        }
    }
}

}  // namespace safe_mode
