// Inkwash firmware entry point.
//
// Phase 1 of the C++ rewrite: board bring-up, the RTC clock and the Home
// screen drawn with LVGL. Later phases add storage, sync, alarms, the other
// screens, BLE and the sleep manager (see firmware/README.md).
#include "board.h"
#include "display.h"
#include "esp_log.h"
#include "fonts.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "pcf8563.h"
#include "ui/home_screen.h"

namespace {

constexpr char kTag[] = "inkwash";

ui::HomeModel ReadHomeModel() {
    ui::HomeModel model;
    model.have_clock = pcf8563::ReadTime(&model.clock) && !model.clock.voltage_low;
    model.battery_percent = board::BatteryPercent();
    model.charge = board::ReadCharge();
    return model;
}

}  // namespace

extern "C" void app_main() {
    board::Init();
    pcf8563::Init(board::I2cBus());
    if (!display::Init()) {
        ESP_LOGE(kTag, "display unavailable");
    }
    fonts::Init();

    ui::HomeScreen home;
    lv_screen_load(home.Screen());
    ui::HomeModel model = ReadHomeModel();
    home.Show(model);
    display::Update(/*full=*/true);
    ESP_LOGI(kTag, "home shown");

    bool keys[3] = {};
    int shown_minute = model.have_clock ? model.clock.minute : -1;
    while (true) {
        vTaskDelay(pdMS_TO_TICKS(50));
        for (int i = 0; i < 3; ++i) {
            const bool down = board::KeyDown(static_cast<board::Key>(i));
            if (down != keys[i]) {
                keys[i] = down;
                ESP_LOGI(kTag, "key %d %s", i, down ? "down" : "up");
            }
        }
        DateTime now;
        if (pcf8563::ReadTime(&now) && now.minute != shown_minute) {
            shown_minute = now.minute;
            home.Show(ReadHomeModel());
            display::Update(/*full=*/false);
        }
    }
}
