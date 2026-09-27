#include "pcf8563.h"

#include "esp_log.h"

namespace pcf8563 {
namespace {

constexpr char kTag[] = "pcf8563";
constexpr uint16_t kAddress = 0x51;
constexpr int kTimeoutMs = 100;

i2c_master_dev_handle_t g_dev = nullptr;

uint8_t FromBcd(uint8_t v) {
    return static_cast<uint8_t>((v >> 4) * 10 + (v & 0x0F));
}

bool IsLeap(int year) {
    return (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
}

int DaysInMonth(int year, int month) {
    static constexpr int kDays[] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
    return month == 2 && IsLeap(year) ? 29 : kDays[month - 1];
}

}  // namespace

bool Init(i2c_master_bus_handle_t bus) {
    i2c_device_config_t cfg = {};
    cfg.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    cfg.device_address = kAddress;
    cfg.scl_speed_hz = 400000;
    if (i2c_master_bus_add_device(bus, &cfg, &g_dev) != ESP_OK) {
        ESP_LOGE(kTag, "add device failed");
        return false;
    }
    return true;
}

bool ReadTime(DateTime* out) {
    if (!g_dev) {
        return false;
    }
    const uint8_t reg = 0x02;
    uint8_t buf[7] = {};
    if (i2c_master_transmit_receive(g_dev, &reg, 1, buf, sizeof(buf), kTimeoutMs) != ESP_OK) {
        ESP_LOGW(kTag, "read failed");
        return false;
    }
    DateTime dt;
    dt.voltage_low = (buf[0] & 0x80) != 0;
    dt.second = FromBcd(buf[0] & 0x7F);
    dt.minute = FromBcd(buf[1] & 0x7F);
    dt.hour = FromBcd(buf[2] & 0x3F);
    dt.day = FromBcd(buf[3] & 0x3F);
    dt.weekday = buf[4] & 0x07;
    dt.month = FromBcd(buf[5] & 0x1F);
    dt.year = static_cast<uint16_t>(2000 + FromBcd(buf[6]));
    const bool valid = dt.month >= 1 && dt.month <= 12 && dt.day >= 1 &&
                       dt.day <= DaysInMonth(dt.year, dt.month) && dt.weekday <= 6 &&
                       dt.hour <= 23 && dt.minute <= 59 && dt.second <= 59;
    if (!valid) {
        ESP_LOGW(kTag, "invalid time registers");
        return false;
    }
    *out = dt;
    return true;
}

}  // namespace pcf8563
