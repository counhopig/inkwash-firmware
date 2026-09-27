// Zectrix Note 4 board support: power rails, keys, charger, battery.
#pragma once

#include <cstdint>

#include "driver/i2c_master.h"

namespace board {

enum class Key : uint8_t { Enter, Up, Down };

struct Charge {
    bool power_present = false;
    bool charging = false;
    bool full = false;
};

// Latches main power and brings up the rails, keys and I2C bus. Call first.
void Init();

i2c_master_bus_handle_t I2cBus();

// Debounced key state; true while the key is held.
bool KeyDown(Key key);

Charge ReadCharge();

// Green LED (GPIO3, low = on).
void SetChargeLed(bool on);

// Battery in percent from the ADC, or -1 when the read fails.
int BatteryPercent();

}  // namespace board
