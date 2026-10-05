# Inkwash — Zectrix Note 4 firmware

Offline calendar, alarms, todos and notifications for the Zectrix Note 4.
The firmware uses C++17, ESP-IDF 5.5.5 and LVGL 9 on an ESP32-S3 with a
4.2-inch, 400 × 300 monochrome e-paper display.

Content comes from `inkwash-server`. The device stores it in NVS, runs alarms
through the PCF8563 RTC, and uploads locally changed completion, enable and
read states during synchronization. `inkwash-desktop` configures the device
through USB Serial/JTAG or BLE.

## Features

- Home clock, next alarm, todo counts, unread notifications and battery state.
- Calendar and week views, alarm and todo lists, notification details.
- Offline RTC alarms, audio reminders and button navigation.
- HTTPS synchronization and urgent notification polling.
- USB and authenticated BLE configuration.
- Automatic light sleep, deep sleep and boot-loop safe mode.

The Wi-Fi icon indicates an active connection with an IP address. Network
sessions end after their work completes, so the icon hides when Wi-Fi disconnects.

## Power and display

Home updates its clock at each RTC minute boundary while running on battery.
After three minutes without interaction, the device enters deep sleep. A timer
wake updates Home and returns to sleep after two seconds of background work.
Enter, Down and the RTC alarm also wake the device. All three keys wake automatic
light sleep while the application is active.

Clock updates use partial refreshes. Scene changes and recovery from a failed
refresh establish a full baseline. Pending commands, network work, audio,
pressed keys and display recovery prevent deep sleep. Audio and network hardware
are initialized on demand; Wi-Fi disconnects at the end of each session. Failed
background networking waits for the configured sync interval before retrying.

## Build

Install ESP-IDF **5.5.5**, including the ESP32-S3 toolchain, then initialize LVGL:

```sh
git submodule update --init firmware/components/lvgl
./scripts/build/build-cpp.sh
```

Set `IDF_PATH` for a nonstandard ESP-IDF installation. The default output is
`firmware/build/`; `INKWASH_CPP_BUILD_DIR` selects another directory.

On Windows, run `scripts\build\build-cpp.ps1` from PowerShell. It defaults to the
short build path `C:\ikc`; use `-BuildDir` to select another path.

## Flash

The authorized target is the Note 4 with MAC **20:6E:F1:B4:7D:E4**. A serial
port name alone does not identify the board. Use ESP32-S3, 16 MB Flash,
DIO mode, 80 MHz and `firmware/partitions.csv`. Never flash this image to a
Note 4C or another ESP32 board. Keep a full 16 MB backup before flashing.

From an ESP-IDF shell:

```sh
./scripts/device/flash-note4.sh --port /dev/cu.usbmodem101
```

On Windows:

```powershell
.\scripts\device\backup-flash.ps1 -Port COM5
.\scripts\device\flash-note4.ps1 -Port COM5 -BuildDir C:\ikc
```

The wrappers verify chip, MAC and Flash capacity before writing. The partition
layout preserves NVS settings. Configuration is stored unencrypted; Secure
Boot and Flash encryption are disabled in the standard image.

## Tests and tools

```sh
IDF_PATH=/path/to/esp-idf bash firmware/test/run.sh
./scripts/checks/check-boot-ledger.sh firmware/build/inkwash.elf
```

CI runs the C++ core tests, builds the ESP32-S3 image and checks that loadable
image segments exclude the retained boot ledger. Hardware acceptance covers
USB, BLE, synchronization, alarm audio, button response and sleep/wake behavior.

| Script | Purpose |
| --- | --- |
| `scripts/build/build-cpp.sh`, `scripts/build/build-cpp.ps1` | Build the C++ firmware |
| `scripts/device/flash-note4.sh`, `scripts/device/flash-note4.ps1` | Verify board identity and flash the image |
| `scripts/device/backup-flash.ps1` | Back up device Flash on Windows |
| `scripts/checks/check-boot-ledger.sh` | Verify retained boot memory is outside image segments |
| `scripts/checks/check-git-rev.sh` | Verify the embedded Git revision |
| `scripts/device/capture-serial.py` | Record USB logs |
| `scripts/checks/smoke-note4.py` | Exercise USB commands and observe stability |
| `scripts/device/set-wifi.py` | Configure device Wi-Fi over USB |
| `scripts/assets/generate_cjk_font.py` | Generate CJK bitmap assets with Pillow |
| `scripts/release/release.sh vX.Y.Z` | Build, check, package and publish a tagged GitHub release from a clean checkout |

Opening USB Serial/JTAG may reset the chip. Committed font assets are in
`firmware/assets/`; normal builds do not require font generation.

Startup logs include `SLEEPTRACE`: the latest 16 boot, deep-sleep and controlled
restart events retained in RTC memory. Entries contain UTC time when available,
reset reason, planned timer and observed wake source. USB resets preserve this
history; a power-on reset starts a new history. No Flash writes are used.

## Code layout

| Path | Role |
| --- | --- |
| `firmware/main/app/` | Single-owner event loop; view, input, alarm, reminder, command, sync and lifecycle modules |
| `firmware/main/core/` | Hardware-free models, scheduling, power/refresh policies and protocol codecs |
| `firmware/main/ui/` | LVGL screens |
| `firmware/main/net/` | Wi-Fi, HTTPS and synchronization |
| `firmware/main/control/` | USB and BLE |
| `firmware/main/storage/` | NVS persistence and sync journal |
| `firmware/main/power/` | Sleep, wake causes and retained state |
| `firmware/main/audio/` | ES8311 and I2S tones |
| `firmware/components/` | LVGL and e-paper driver |
| `firmware/test/` | C++ host tests |
| `firmware/assets/` | Embedded fonts and font license |
| `scripts/` | Build, flash, validation and maintenance tools |

`firmware/partitions.csv` is the authorized build and Flash partition layout.
Flash scripts verify its checksum and the generated binary table.
Local device backups belong in `backups/`, serial captures in `logs/`, and default
build output in `firmware/build/`. These generated files are excluded from Git.

## License

[Apache-2.0](LICENSE). Font license information is in
[`firmware/assets/FONT_LICENSE.txt`](firmware/assets/FONT_LICENSE.txt).
