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
| `main/main.cc` | Entry: board, wake cause, NVS, RTC, display, fonts, audio, then `app::Run` |
| `main/app/` | The application task: screens, key handling, notices, commands, sync scheduling, alarms, reminders, sleep |
| `main/core/` | Hardware-free logic (dates, model, JSON codec, schedule, protocol, sync payload); host-tested by `test/run.sh` |
| `main/storage/` | NVS, with the Rust firmware's namespaces, keys and JSON |
| `main/net/` | Wi-Fi, NTP, HTTPS, the sync protocol and urgent poll |
| `main/control/` | USB `>>IW` console and the BLE control service |
| `main/audio/` | ES8311 + I2S tones (alarm, siren, todo beep) |
| `main/power/` | Wake cause, light sleep, deep sleep, RTC-retained Home frame |
| `main/board.*`, `main/keys.*`, `main/pcf8563.*` | Board rails, charger, battery, keys, RTC |
| `main/display.*` | EPD behind LVGL: frame diff picks partial or full refresh |
| `main/fonts.*`, `main/ui/` | Bitmap fonts (ASCII + HZK CJK), icons, every screen |
| `main/assets/` | Generated from the Rust sources by `tools/gen_assets.py` |

## Status

Feature parity with the Rust firmware; awaiting the on-device check. The
Rust firmware in `rust-firmware/` stays in the tree as the fallback image
until the C++ one is confirmed on hardware.
