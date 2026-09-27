// PCF8563 real-time clock on I2C0 (rust-firmware/src/rtc.rs).
#pragma once

#include <cstdint>

#include "core/datetime.h"
#include "core/schedule.h"
#include "driver/i2c_master.h"

namespace pcf8563 {

bool Init(i2c_master_bus_handle_t bus);

// Reads the current time; false on an I2C error or an invalid register value.
bool ReadTime(DateTime* out);
// Writes the time and clears the voltage-low flag.
bool WriteTime(const DateTime& dt);

bool SetAlarm(const schedule::AlarmRegs& regs);  // also enables the interrupt
bool ClearAlarm();                               // disables every field and the interrupt

struct AlarmStatus {
    bool flag = false;               // AF: the alarm matched
    bool interrupt_enabled = false;  // AIE
};
bool ReadAlarmStatus(AlarmStatus* out);
bool AckAlarm();  // clears AF, keeps AIE

}  // namespace pcf8563
