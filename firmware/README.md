# Inkwash firmware (C++)

The C++ rewrite of the Note 4 firmware on ESP-IDF 5.5 with LVGL 9. It keeps
the Rust firmware's screens pixel for pixel (same bitmap font, icons and
coordinates), its server, USB and BLE protocols, and its NVS layout, so a
device keeps its Wi-Fi and server settings across the switch. The power
architecture follows [Slate](https://github.com/qiujun8023/slate) (MIT):
classify each wake, run only the work it needs, cut the peripheral rails and
go back to deep sleep.

The Rust firmware in `rust-firmware/` stays the shipping image until this one
reaches parity.

## Build and flash

```powershell
git submodule update --init firmware/components/lvgl
.\scripts\build-cpp.ps1                       # builds into C:\ikc
.\scripts\flash-note4.ps1 -Port COM5 -Cpp -Monitor
```

On Linux: `idf.py -C firmware build`. Flash only through the identity-checked
wrapper (see `AGENTS.md`): ESP32-S3, 16 MB, DIO, 80 MHz, the Rust firmware's
`partitions.csv`.

## Layout

| Path | Role |
|---|---|
| `main/board.*` | Power latch, rails, keys, charger, battery ADC, I2C bus |
| `main/pcf8563.*` | RTC |
| `main/display.*` | EPD driver behind LVGL: RGB565 strips to the 1 bpp frame, partial/full refresh |
| `main/fonts.*` | The 8x16 bitmap font as LVGL fonts at scales 1/2/3/5, icons as images |
| `main/ui/` | Screens |
| `main/assets/` | Generated from the Rust sources by `tools/gen_assets.py` |

## Phases

1. **Done** — board bring-up, RTC, LVGL on the EPD, Home screen, minute clock.
2. Keys and navigation: key events, GO TO bar, Settings, About, notice bar.
3. Storage (NVS, compatible keys) and the USB control protocol.
4. Wi-Fi, HTTPS sync, alarms/todos/inbox data, Calendar, lists, ringing.
5. PCF8563 alarms, ES8311 tones, reminders.
6. Sleep manager: timer/alarm/key wakes, rails off, deep sleep.
7. BLE control channel.
8. Retire `rust-firmware/`.
