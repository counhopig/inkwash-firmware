// PCF8563 real-time clock on I2C0.
#pragma once

#include <cstdint>

#include "driver/i2c_master.h"

struct DateTime {
    uint16_t year = 2000;
    uint8_t month = 1;
    uint8_t day = 1;
    uint8_t weekday = 0;  // 0 = Sunday
    uint8_t hour = 0;
    uint8_t minute = 0;
    uint8_t second = 0;
    bool voltage_low = false;  // the RTC lost power; the time is not trustworthy
};

namespace pcf8563 {

bool Init(i2c_master_bus_handle_t bus);

// Reads the current time; false on an I2C error or an invalid register value.
bool ReadTime(DateTime* out);

}  // namespace pcf8563
