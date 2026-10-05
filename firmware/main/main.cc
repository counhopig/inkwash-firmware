// Inkwash firmware entry point: bring up the board, read why the chip woke,
// then hand over to the application task loop.
#include <cstdio>

#include "app/app.h"
#include "app/safe_mode.h"
#include "audio/tones.h"
#include "board.h"
#include "display.h"
#include "esp_core_dump.h"
#include "esp_log.h"
#include "fonts.h"
#include "pcf8563.h"
#include "power/power.h"
#include "storage/store.h"

namespace {

// Prints the previous crash (if any) so a serial log is enough to find it,
// then erases it so the next report is fresh.
void ReportPreviousCrash() {
    if (esp_core_dump_image_check() != ESP_OK) return;
    esp_core_dump_summary_t summary = {};
    if (esp_core_dump_get_summary(&summary) == ESP_OK) {
        char bt[16 * 11 + 1] = {};
        size_t used = 0;
        for (uint32_t i = 0; i < summary.exc_bt_info.depth && i < 16; ++i) {
            used += snprintf(bt + used, sizeof(bt) - used, " 0x%08lx",
                             static_cast<unsigned long>(summary.exc_bt_info.bt[i]));
        }
        ESP_LOGE("crash", "previous run crashed in task '%s' at pc 0x%08lx; backtrace:%s",
                 summary.exc_task, static_cast<unsigned long>(summary.exc_pc), bt);
    }
    esp_core_dump_image_erase();
}

}  // namespace

extern "C" void app_main() {
    boot_guard::ResetKind reset = boot_guard::ResetKind::Other;
    const boot_guard::Ledger ledger = power::NoteBootAttempt(&reset);
    ESP_LOGI("inkwash", "reset: %s, failed boots in a row: %u", boot_guard::Label(reset),
             unsigned(ledger.failures));
    board::Init();
    ReportPreviousCrash();
    if (ledger.Exhausted()) {
        power::PrintSleepTrace();
        safe_mode::Run(ledger, reset);
    }
    const power::WakeCause wake = power::ReadWakeCause();
    if (!store::Init()) {
        ESP_LOGE("inkwash", "NVS unavailable; settings will not persist");
    }
    if (!pcf8563::Init(board::I2cBus())) ESP_LOGE("inkwash", "RTC unavailable");
    if (!display::Init()) {
        ESP_LOGE("inkwash", "display unavailable");
        power::DeepSleepUntilEnter();
    }
    fonts::Init();
    if (!tones::Init()) ESP_LOGW("inkwash", "audio worker unavailable");
    app::Run(wake);
}
