// Inkwash firmware entry point: bring up the board, read why the chip woke,
// then hand over to the application task loop.
#include <cstdio>

#include "app/app.h"
#include "app/safe_mode.h"
#include "audio/tones.h"
#include "board.h"
#include "display.h"
#include "diagnostics/event_log.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "esp_core_dump.h"
#include "esp_log.h"
#include "fonts.h"
#include "pcf8563.h"
#include "power/power.h"
#include "storage/store.h"

namespace {

// Keep the crash image until its summary and backtrace have reached flash.
void ReportPreviousCrash() {
    if (esp_core_dump_image_check() != ESP_OK) return;
    esp_core_dump_summary_t summary = {};
    bool saved = false;
    if (esp_core_dump_get_summary(&summary) == ESP_OK) {
        saved = event_log::Critical("crash task=%.24s pc=%08lx", summary.exc_task,
                            static_cast<unsigned long>(summary.exc_pc));
        char bt[16 * 11 + 1] = {};
        size_t used = 0;
        for (uint32_t i = 0; i < summary.exc_bt_info.depth && i < 16; ++i) {
            saved = event_log::Critical("crash_bt index=%lu pc=%08lx", static_cast<unsigned long>(i),
                           static_cast<unsigned long>(summary.exc_bt_info.bt[i])) && saved;
            used += snprintf(bt + used, sizeof(bt) - used, " 0x%08lx",
                             static_cast<unsigned long>(summary.exc_bt_info.bt[i]));
        }
        ESP_LOGE("crash", "previous run crashed in task '%s' at pc 0x%08lx; backtrace:%s",
                 summary.exc_task, static_cast<unsigned long>(summary.exc_pc), bt);
    }
    // Preserve the crash image if its summary could not be persisted.
    if (saved && event_log::Flush()) esp_core_dump_image_erase();
}

}  // namespace

extern "C" void app_main() {
    boot_guard::ResetKind reset = boot_guard::ResetKind::Other;
    const boot_guard::Ledger ledger = power::NoteBootAttempt(&reset);
    ESP_LOGI("inkwash", "reset: %s, failed boots in a row: %u", boot_guard::Label(reset),
             unsigned(ledger.failures));
    board::Init();
    if (!event_log::Init()) ESP_LOGE("inkwash", "persistent event log unavailable");
    event_log::Critical("boot rev=%.32s reset=%u wake=%u failed=%u", INKWASH_GIT_REV, unsigned(esp_reset_reason()),
                        unsigned(esp_sleep_get_wakeup_cause()), unsigned(ledger.failures));
    if (esp_sleep_get_wakeup_cause() == ESP_SLEEP_WAKEUP_EXT1)
        event_log::Add("wake_mask=%llu", static_cast<unsigned long long>(esp_sleep_get_ext1_wakeup_status()));
    ReportPreviousCrash();
    if (ledger.Exhausted()) {
        power::PrintSleepTrace();
        safe_mode::Run(ledger, reset);
    }
    const power::WakeCause wake = power::ReadWakeCause();
    if (!store::Init()) {
        event_log::Critical("nvs_init_failed");
        ESP_LOGE("inkwash", "NVS unavailable; settings will not persist");
    }
    if (!pcf8563::Init(board::I2cBus())) {
        event_log::Critical("rtc_init_failed");
        ESP_LOGE("inkwash", "RTC unavailable");
    }
    if (!display::Init()) {
        event_log::Critical("display_init_failed");
        ESP_LOGE("inkwash", "display unavailable");
        power::DeepSleepUntilEnter();
    }
    fonts::Init();
    if (!tones::Init()) {
        event_log::Critical("audio_init_failed");
        ESP_LOGW("inkwash", "audio worker unavailable");
    }
    app::Run(wake);
}
