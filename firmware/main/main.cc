// Inkwash firmware entry point: bring up the board, read why the chip woke,
// then hand over to the application task loop.
#include "app/app.h"
#include "audio/tones.h"
#include "board.h"
#include "display.h"
#include "esp_log.h"
#include "fonts.h"
#include "pcf8563.h"
#include "power/power.h"
#include "storage/store.h"

extern "C" void app_main() {
    board::Init();
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
