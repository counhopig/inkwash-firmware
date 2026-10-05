#pragma once
#include "driver/gpio.h"

namespace board::pins {
inline constexpr gpio_num_t PowerLatch = GPIO_NUM_17;
inline constexpr gpio_num_t Led = GPIO_NUM_3;
inline constexpr gpio_num_t Avdd = GPIO_NUM_42;  // shared audio and I2C pull-up rail
inline constexpr gpio_num_t PaEnable = GPIO_NUM_46;
inline constexpr gpio_num_t NfcPower = GPIO_NUM_21;
inline constexpr gpio_num_t KeyEnter = GPIO_NUM_0;
inline constexpr gpio_num_t KeyUp = GPIO_NUM_39;
inline constexpr gpio_num_t KeyDown = GPIO_NUM_18;
inline constexpr gpio_num_t RtcInt = GPIO_NUM_5;
inline constexpr gpio_num_t ChargeActive = GPIO_NUM_2;
inline constexpr gpio_num_t ChargeDone = GPIO_NUM_1;
inline constexpr gpio_num_t I2cSda = GPIO_NUM_47;
inline constexpr gpio_num_t I2cScl = GPIO_NUM_48;
inline constexpr gpio_num_t AudioMclk = GPIO_NUM_14;
inline constexpr gpio_num_t AudioBclk = GPIO_NUM_15;
inline constexpr gpio_num_t AudioWs = GPIO_NUM_38;
inline constexpr gpio_num_t AudioOut = GPIO_NUM_45;
}  // namespace board::pins
