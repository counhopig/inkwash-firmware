#include "board.h"

#include "driver/gpio.h"
#include "esp_adc/adc_cali.h"
#include "esp_adc/adc_cali_scheme.h"
#include "esp_adc/adc_oneshot.h"
#include "esp_log.h"

namespace board {
namespace {

constexpr char kTag[] = "board";

// Pinout (see the README hardware table).
constexpr gpio_num_t kPowerLatch = GPIO_NUM_17;  // high keeps the battery switched on
constexpr gpio_num_t kLed = GPIO_NUM_3;          // green LED, low = on
constexpr gpio_num_t kAvdd = GPIO_NUM_42;        // audio + I2C pull-up rail
constexpr gpio_num_t kPaEnable = GPIO_NUM_46;    // speaker amplifier
constexpr gpio_num_t kNfcPower = GPIO_NUM_21;    // GT23SC6699 supply, unused
constexpr gpio_num_t kKeyEnter = GPIO_NUM_0;
constexpr gpio_num_t kKeyUp = GPIO_NUM_39;
constexpr gpio_num_t kKeyDown = GPIO_NUM_18;
constexpr gpio_num_t kChargeActive = GPIO_NUM_2;  // CHRG_L: low while charging
constexpr gpio_num_t kChargeDone = GPIO_NUM_1;    // STDBY_H: high when full
constexpr gpio_num_t kI2cSda = GPIO_NUM_47;
constexpr gpio_num_t kI2cScl = GPIO_NUM_48;
constexpr adc_channel_t kBatteryChannel = ADC_CHANNEL_3;  // GPIO4, VBAT / 2

i2c_master_bus_handle_t g_i2c = nullptr;
adc_oneshot_unit_handle_t g_adc = nullptr;
adc_cali_handle_t g_adc_cali = nullptr;

void Output(gpio_num_t pin, int level) {
    // Latch the level into the output register first: a pin held through
    // deep sleep drives the register value the moment the hold is released,
    // and a low glitch on the power latch would switch the device off.
    gpio_set_level(pin, level);
    gpio_hold_dis(pin);
    gpio_config_t cfg = {};
    cfg.pin_bit_mask = 1ULL << pin;
    cfg.mode = GPIO_MODE_OUTPUT;
    gpio_config(&cfg);
    gpio_set_level(pin, level);
}

void Input(gpio_num_t pin, bool pull_up) {
    gpio_config_t cfg = {};
    cfg.pin_bit_mask = 1ULL << pin;
    cfg.mode = GPIO_MODE_INPUT;
    cfg.pull_up_en = pull_up ? GPIO_PULLUP_ENABLE : GPIO_PULLUP_DISABLE;
    gpio_config(&cfg);
}

gpio_num_t KeyPin(Key key) {
    switch (key) {
        case Key::Enter: return kKeyEnter;
        case Key::Up: return kKeyUp;
        case Key::Down: return kKeyDown;
    }
    return kKeyEnter;
}

}  // namespace

void Init() {
    Output(kPowerLatch, 1);
    Output(kLed, 1);
    Output(kPaEnable, 0);
    Output(kNfcPower, 0);
    Output(kAvdd, 1);

    Input(kKeyEnter, true);
    Input(kKeyUp, true);
    Input(kKeyDown, true);
    Input(kChargeActive, false);
    Input(kChargeDone, false);

    i2c_master_bus_config_t bus = {};
    bus.i2c_port = I2C_NUM_0;
    bus.sda_io_num = kI2cSda;
    bus.scl_io_num = kI2cScl;
    bus.clk_source = I2C_CLK_SRC_DEFAULT;
    bus.glitch_ignore_cnt = 7;
    bus.flags.enable_internal_pullup = false;
    ESP_ERROR_CHECK(i2c_new_master_bus(&bus, &g_i2c));

    adc_oneshot_unit_init_cfg_t unit = {};
    unit.unit_id = ADC_UNIT_1;
    ESP_ERROR_CHECK(adc_oneshot_new_unit(&unit, &g_adc));
    adc_oneshot_chan_cfg_t chan = {};
    chan.atten = ADC_ATTEN_DB_12;
    chan.bitwidth = ADC_BITWIDTH_DEFAULT;
    ESP_ERROR_CHECK(adc_oneshot_config_channel(g_adc, kBatteryChannel, &chan));
    adc_cali_curve_fitting_config_t cali = {};
    cali.unit_id = ADC_UNIT_1;
    cali.chan = kBatteryChannel;
    cali.atten = ADC_ATTEN_DB_12;
    cali.bitwidth = ADC_BITWIDTH_DEFAULT;
    if (adc_cali_create_scheme_curve_fitting(&cali, &g_adc_cali) != ESP_OK) {
        ESP_LOGW(kTag, "ADC calibration unavailable; battery readings are raw");
        g_adc_cali = nullptr;
    }
    ESP_LOGI(kTag, "board up");
}

i2c_master_bus_handle_t I2cBus() {
    return g_i2c;
}

bool KeyDown(Key key) {
    return gpio_get_level(KeyPin(key)) == 0;
}

Charge ReadCharge() {
    Charge charge;
    const bool active = gpio_get_level(kChargeActive) == 0;
    const bool done = gpio_get_level(kChargeDone) == 1;
    charge.power_present = active || done;
    charge.charging = active && !done;
    charge.full = done && !active;
    return charge;
}

int BatteryPercent() {
    constexpr int kSamples = 10;
    int sum_mv = 0;
    for (int i = 0; i < kSamples; ++i) {
        int raw = 0;
        if (adc_oneshot_read(g_adc, kBatteryChannel, &raw) != ESP_OK) {
            return -1;
        }
        int mv = raw;
        if (g_adc_cali && adc_cali_raw_to_voltage(g_adc_cali, raw, &mv) != ESP_OK) {
            return -1;
        }
        sum_mv += mv;
    }
    const int vbat = sum_mv / kSamples * 2;
    // Same curve as the Rust firmware (board.rs battery_percent_from_mv).
    const long long percent =
        (-static_cast<long long>(vbat) * vbat + 9016LL * vbat - 19189000LL) / 10000;
    return percent < 0 ? 0 : percent > 100 ? 100 : static_cast<int>(percent);
}

}  // namespace board
