#include "pcf8563.h"

#include "esp_log.h"

namespace pcf8563 {
namespace {

constexpr char kTag[] = "pcf8563";
constexpr uint16_t kAddress = 0x51;
constexpr int kTimeoutMs = 100;
constexpr uint8_t kRegCtrl1 = 0x00;
constexpr uint8_t kRegCtrl2 = 0x01;
constexpr uint8_t kRegTime = 0x02;
constexpr uint8_t kRegAlarm = 0x09;
constexpr uint8_t kAlarmFlag = 0x08;
constexpr uint8_t kAlarmIntEnable = 0x02;

i2c_master_dev_handle_t g_dev = nullptr;

uint8_t FromBcd(uint8_t v) { return static_cast<uint8_t>((v >> 4) * 10 + (v & 0x0F)); }
uint8_t ToBcd(uint8_t v) { return static_cast<uint8_t>(((v / 10) << 4) | (v % 10)); }

bool Read(uint8_t reg, uint8_t* buf, size_t len) {
    return g_dev && i2c_master_transmit_receive(g_dev, &reg, 1, buf, len, kTimeoutMs) == ESP_OK;
}

bool Write(uint8_t reg, const uint8_t* data, size_t len) {
    if (!g_dev || len > 8) return false;
    uint8_t buf[9];
    buf[0] = reg;
    for (size_t i = 0; i < len; ++i) buf[i + 1] = data[i];
    return i2c_master_transmit(g_dev, buf, len + 1, kTimeoutMs) == ESP_OK;
}

bool UpdateCtrl2(uint8_t clear, uint8_t set) {
    uint8_t ctrl2 = 0;
    if (!Read(kRegCtrl2, &ctrl2, 1)) return false;
    ctrl2 = static_cast<uint8_t>((ctrl2 & ~clear) | set);
    return Write(kRegCtrl2, &ctrl2, 1);
}

bool Valid(const DateTime& dt) {
    return dt.year >= 2000 && dt.year <= 2099 && dt.month >= 1 && dt.month <= 12 &&
           dt.day >= 1 && dt.day <= DaysInMonth(dt.year, dt.month) && dt.weekday <= 6 &&
           dt.hour <= 23 && dt.minute <= 59 && dt.second <= 59;
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
    uint8_t buf[7] = {};
    if (!Read(kRegTime, buf, sizeof(buf))) {
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
    if (!Valid(dt)) {
        ESP_LOGW(kTag, "invalid time registers");
        return false;
    }
    *out = dt;
    return true;
}

bool WriteTime(const DateTime& dt) {
    if (!Valid(dt)) return false;
    const uint8_t payload[7] = {
        static_cast<uint8_t>(ToBcd(dt.second) & 0x7F), static_cast<uint8_t>(ToBcd(dt.minute) & 0x7F),
        static_cast<uint8_t>(ToBcd(dt.hour) & 0x3F),   static_cast<uint8_t>(ToBcd(dt.day) & 0x3F),
        static_cast<uint8_t>(dt.weekday & 0x07),       static_cast<uint8_t>(ToBcd(dt.month) & 0x1F),
        ToBcd(static_cast<uint8_t>(dt.year % 100)),
    };
    if (!Write(kRegTime, payload, sizeof(payload))) return false;
    uint8_t ctrl1 = 0;
    if (!Read(kRegCtrl1, &ctrl1, 1)) return false;
    ctrl1 &= static_cast<uint8_t>(~0x80);
    return Write(kRegCtrl1, &ctrl1, 1);
}

bool SetAlarm(const schedule::AlarmRegs& regs) {
    const uint8_t payload[4] = {
        static_cast<uint8_t>(ToBcd(regs.minute) & 0x7F),
        static_cast<uint8_t>(ToBcd(regs.hour) & 0x3F),
        regs.day >= 0 ? static_cast<uint8_t>(ToBcd(static_cast<uint8_t>(regs.day)) & 0x3F)
                      : static_cast<uint8_t>(0x80),
        regs.weekday >= 0 ? static_cast<uint8_t>(regs.weekday & 0x07) : static_cast<uint8_t>(0x80),
    };
    return Write(kRegAlarm, payload, sizeof(payload)) && UpdateCtrl2(0, kAlarmIntEnable);
}

bool ClearAlarm() {
    const uint8_t disabled[4] = {0x80, 0x80, 0x80, 0x80};
    return Write(kRegAlarm, disabled, sizeof(disabled)) &&
           UpdateCtrl2(kAlarmFlag | kAlarmIntEnable, 0);
}

bool ReadAlarmStatus(AlarmStatus* out) {
    uint8_t ctrl2 = 0;
    if (!Read(kRegCtrl2, &ctrl2, 1)) return false;
    out->flag = (ctrl2 & kAlarmFlag) != 0;
    out->interrupt_enabled = (ctrl2 & kAlarmIntEnable) != 0;
    return true;
}

bool AckAlarm() { return UpdateCtrl2(kAlarmFlag, 0); }

}  // namespace pcf8563
