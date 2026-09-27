// Inkwash firmware entry point: bring up the board, read why the chip woke,
// then hand over to the application task loop.
#include "app/app.h"
#include "app/safe_mode.h"
#include "audio/tones.h"
#include "board.h"
#include "display.h"
#include "esp_log.h"
#include "fonts.h"
#include "pcf8563.h"
#include "power/power.h"
#include "storage/store.h"

extern "C" void app_main() {
    boot_guard::ResetKind reset = boot_guard::ResetKind::Other;
    const boot_guard::Ledger ledger = power::NoteBootAttempt(&reset);
    ESP_LOGI("inkwash", "reset: %s, failed boots in a row: %u", boot_guard::Label(reset),
             unsigned(ledger.failures));
    board::Init();
    if (ledger.Exhausted()) {
        safe_mode::Run(ledger, reset);
    }
    const power::WakeCause wake = power::ReadWakeCause();
    if (!store::Init()) {
        ESP_LOGE("inkwash", "NVS unavailable; settings will not persist");
    }
    pcf8563::Init(board::I2cBus());
    if (!display::Init()) {
        ESP_LOGE("inkwash", "display unavailable");
    }
    fonts::Init();
    tones::Init();
    app::Run(wake);
}
