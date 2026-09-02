# ZECTRIX NOTE4 Firmware Development Guide

This document records the hardware information, development environment, build and flashing workflow, e-paper display driver approach, and commonly repeated pitfalls required for taking over this repository.

> This repository has only been verified on the ZECTRIX NOTE4 **black-and-white display version**. The display hardware, firmware, and waveforms of the NOTE4C and NOTE4 are incompatible — **never cross-flash them**.

## 1. Project Scope

Target device: **ZECTRIX NOTE4 black-and-white display version** (i.e., the hardware corresponding to `itopinion/zectrix-note4-epd-demo`). The firmware is a usable offline-first calendar/alarm/todo device: Wi-Fi STA+NTP, audio (ES8311), RTC (PCF8563, including hardware alarm registers), NFC (GT23SC6699), battery management and ADC, deep sleep (GPIO17 RTC hold), USB/BLE control protocol, and HTTPS two-way sync are all implemented.

## 2. Safety Matters (Non-Negotiable)

1. **Confirm the device is the NOTE4 black-and-white display version.** The NOTE4C's display hardware and firmware differ; do not cross-flash.
2. **Before the first partition modification or flash, read the full 16 MiB Flash and save its SHA-256.** Copies of the backup should be stored in at least one physically isolated location.
3. **The factory backup contains device-unique data, credentials, and calibration information** and must not be committed publicly. `backups/*.bin` is already excluded by `.gitignore`.
4. **The DIO boot mode must be used.** Both the NOTE4 factory boot logs and this project's verification results show `mode:DIO`; a QIO image repeatedly triggers watchdog resets before the application starts.
5. **The e-paper display must use waveforms and power sequencing matching the panel.** Do not replace the official driver with a generic SSD1683 example and then repeatedly refresh.
6. **GPIO17 (PWR_ON) must be pulled high early in boot**, otherwise releasing the power key powers off the whole device; an RTC GPIO hold must also be designed for deep sleep.
7. Close the monitor, serial terminals, and other IDEs occupying the serial port before flashing.

## 3. Verified Hardware (Board Reference)

These notes are cross-referenced from the ZECTRIX NOTE4 spec page and the
Slate firmware documentation, serving as this project's board-level baseline.
The GPIO map describes the **full NOTE4 device hardware**, including every
module's pins; the rows marked with **✓** are the ones the firmware currently
uses explicitly.

### Core Specification

| Item | Value |
| --- | --- |
| MCU | ESP32-S3-WROOM-1 N16R8 |
| Flash | 16 MB; firmware boots in DIO mode |
| PSRAM | 8 MB Octal |
| Display | 4.2 inch black-white EPD, 400 × 300 |
| Audio | ES8311 codec, speaker, MEMS mic |
| Other | PCF8563 RTC, GT23SC6699 NFC |
| USB | USB-C CDC/JTAG |

### Complete GPIO Map

| GPIO | Signal | Notes | Used by firmware |
| --- | --- | --- | --- |
| 0 | KEY_ENTER / BOOT | Active low, RTC-capable wake | ✓ ENTER key |
| 1 | STDBY_H | Charge IC full status | ✓ `charge_done` |
| 2 | CHRG_L | Charge IC charging status, active low | ✓ `charging` |
| 3 | LED_G | Green LED, active low | ✓ status LED |
| 4 | ADC_BAT | VBAT 1:2 divider | ✓ battery ADC |
| 5 | RTC_INT | PCF8563 interrupt | ✓ deep-sleep ext1 wake + `alarm_flag` poll |
| 6 | EPD_PWR_EN | EPD power rail | ✓ managed by official driver |
| 7 | NFC_FD | NFC field detect | — |
| 8 | EPD_BUSY | Active low: low means busy | ✓ |
| 9 | EPD_NRES | EPD reset | ✓ managed by official driver |
| 10 | EPD_NDC | EPD data/command | ✓ managed by official driver |
| 11 | EPD_NCS | EPD chip select | ✓ SPI3 |
| 12 | EPD_SCK | SPI clock | ✓ SPI3 |
| 13 | EPD_SDA | SPI MOSI | ✓ SPI3 |
| 14 | I2S_MCLK | ES8311 MCLK | — |
| 15 | I2S_SCLK | ES8311 BCLK | — |
| 16 | I2S_ASDOUT | Mic data in | — |
| 17 | PWR_ON | Main power latch, high keeps power | ✓ pull high early in boot |
| 18 | KEY_DET / PGDN | Down key and power-on feedback | ✓ DOWN key |
| 19 | USB_DN | USB D- | — |
| 20 | USB_DP | USB D+ | — |
| 21 | NFC_PWR | NFC power | — |
| 38 | I2S_LRCK | ES8311 LRCK | — |
| 39 | KEY_PGUP | Up key, not RTC-capable wake | ✓ UP key |
| 42 | PA_PWR_EN | Audio + I2C rail (AVDD) | ✓ pulled high before I2C0 init |
| 43 | TXD0 | UART TX | — |
| 44 | RXD0 | UART RX | — |
| 45 | I2S_DSDIN | Speaker data out | — |
| 46 | PA_CTRL | Speaker PA enable | — |
| 47 | I2C_SDA | I2C data | ✓ I2C0 to PCF8563/ES8311 |
| 48 | I2C_SCL | I2C clock | ✓ I2C0 to PCF8563/ES8311 |

GPIO 26-37 are occupied by Octal PSRAM and must not be used as ordinary GPIOs.

### Power Rails

| Rail | Control | Notes |
| --- | --- | --- |
| Main power | GPIO17 | Must be held high before the user releases the power key |
| EPD 3V3 | GPIO6 | Can be off while EPD keeps its visible image |
| AVDD 3V3 | GPIO42 | Audio power and I2C pull-ups |

### Deep-Sleep Tier (Automatic)

The firmware has two sleep tiers: light sleep engages automatically whenever
the main loop is idle (both cores idle past
`CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP`), and after **5 minutes** without user
activity the device drops to deep sleep for µA-level idle power. While a real
USB host is connected, `CONFIG_USJ_NO_AUTO_LS_ON_CONNECTION` blocks automatic
light sleep and the main loop pauses its explicit deep-sleep timer so logs and
the control protocol remain connected. A charge-only source sends no USB SOF
packets and does not block either sleep tier. Deep-sleep wake sources:

| Source | Pin | Notes |
| --- | --- | --- |
| ENTER button | GPIO0 | RTC-capable |
| DOWN button | GPIO18 | RTC-capable; included in the ext1 wake mask (`wake_cause()` reports `WakeCause::Down`) |
| RTC alarm line | GPIO5 | `RTC_INT`, open-drain active low - alarms ring from deep sleep |
| Maintenance timer | - | Wakes for the next month-boundary alarm re-arm, or after the 10-minute fallback (whichever is sooner) so the on-screen clock and sync scheduler stay current |

**Deep-sleep limitations (by hardware):**
- **UP (GPIO39) cannot wake deep sleep.** GPIO39 is not an RTC-capable pin, so it is the one nav key excluded from the ext1 wake mask (`power.rs:89` masks GPIO0|GPIO5|GPIO18). ENTER and DOWN both wake deep sleep; UP is light-sleep-only. Users must press ENTER or DOWN to resume from deep sleep.
- **USB cannot wake deep sleep.** USB-Serial-JTAG has no light- or deep-sleep
  wake path on ESP32-S3. The firmware therefore does not enter either sleep
  tier while a real USB host is connected. If USB is attached only after the
  device is already in deep sleep, press ENTER/DOWN or reset/re-plug it first;
  opening the serial port may reset the chip (see §13). The desktop tool's
  35/45 s timeout covers a fresh boot.
- The maintenance timer wake boots into the normal loop; the aligned sync scheduler then fires at the next :00/:30 boundary, so scheduled syncs survive deep sleep.

See `rust-firmware/src/power.rs` (`enter_deep_sleep_with_wakeups`) and `rust-firmware/src/main.rs` (the deep-sleep tier in the main loop).

## 4. Software Architecture

The application core is written in Rust, built on top of ESP-IDF (5.5.5):

```text
Rust application (main.rs, 28 flat mod modules — see rust-firmware/AGENTS.md)
  |
  +-- Board ownership, buttons, RTC, audio, NFC, ADC (board.rs / esp-idf-hal)
  |
  +-- 1bpp framebuffer and proportional glyph renderer (canvas.rs / font8x16.rs / display.rs)
         |
         +-- generated C bindings (esp-idf-sys bindgen)
                 |
                 +-- official zectrix_epd C++ component (vendor)
                         |
                         +-- ESP-IDF GPIO/SPI drivers + SSD2683 waveform
```

See the MODULE MAP in [`rust-firmware/AGENTS.md`](../rust-firmware/AGENTS.md) for the complete module responsibility table (28 src files, including application-layer modules such as UI/alarm/todo/sync/USB/BLE) — the rest of this section only keeps the build details of the EPD FFI component itself and does not duplicate the full module list.

| File | Role |
| --- | --- |
| `rust-firmware/components/zectrix_epd/` | Official NOTE4 EPD C++ driver + SSD2683 waveform table |
| `rust-firmware/Cargo.toml` | Rust dependencies + esp-idf-sys extra_components configuration (generates the `zectrix_epd` FFI) |
| `rust-firmware/sdkconfig.defaults` | ESP32-S3, DIO, PSRAM, serial port, etc. configuration |
| `rust-firmware/partitions.csv` | 16 MB Flash partition table |
| `scripts/build-rust.sh` | Sources the local ESP-IDF environment and builds the Rust firmware (Linux) |

### Official EPD Driver Source

- Official repository: <https://github.com/itopinion/zectrix-note4-epd-demo>
- This project uses its `components/zectrix_epd`.

Do not delete `rust-firmware/components/zectrix_epd/private_include/ssd2683_waveform.h`. It contains the complete waveform data and is the key fix for the "the program reports a successful refresh but the screen still shows the old image" issue. When updating the upstream component, preserve its directory structure and rebuild completely.

In `Cargo.toml`:

```toml
[[package.metadata.esp-idf-sys.extra_components]]
component_dirs = ["components/zectrix_epd"]
bindings_header = "components/zectrix_epd/include/zectrix_epd.h"
bindings_module = "zectrix_epd"
```

This section accomplishes two things at once: it makes CMake compile the official component and generates a standalone Rust FFI module from `zectrix_epd.h` (9 `zectrix_epd_*` symbols have been observed in `out/bindings.rs` in this repository's release artifacts).

After building, the ELF exports the following C ABI symbols (verified with `xtensa-esp32s3-elf-nm`):

```
T zectrix_epd_del
T zectrix_epd_get_default_config
T zectrix_epd_new
T zectrix_epd_power_off
T zectrix_epd_power_on
T zectrix_epd_refresh_full_1bpp
T zectrix_epd_refresh_partial_1bpp
```

## 5. Arch Linux Development Environment

Verified combination:

| Item | Version |
| --- | --- |
| Operating system | Arch Linux x86-64 |
| ESP-IDF | 5.5.5 (git clone `v5.5.5` to `~/esp/esp-idf`, `alias get_idf='. $HOME/esp/esp-idf/export.sh'`) |
| Python | Managed by ESP-IDF (`~/.espressif/python_env/idf5.5_py3.14_env`) |
| Rust stable | Installed via pacman (`rustup`) |
| Rust Xtensa | Toolchain `esp` (installed via `espup install` to `~/.rustup/toolchains/esp`, includes rust-src) |
| `espflash` / `cargo-espflash` | 4.5.0 (the `espflash` package in the extra repository) |
| USB serial port | USB Serial/JTAG → `/dev/ttyACM0`; the user must be a member of the `uucp` group |

Verify the environment:

```bash
get_idf && idf.py --version
rustup toolchain list
rustc +esp --version
espflash --version
```

You should see ESP-IDF 5.5.x, a toolchain named `esp`, and a runnable `espflash`. If anything is missing:

```bash
sudo pacman -S espflash espup rustup
espup install     # installs the Xtensa Rust toolchain + xtensa-esp-elf GCC + clang
```

If the local ESP-IDF path or toolchain version differs, modify `scripts/build-rust.sh` and `rust-firmware/.cargo/config.toml` (`IDF_PATH`, `LIBCLANG_PATH`).

> **espup troubleshooting**: espup downloads the Xtensa toolchain from GitHub, while RISC-V targets go through `rustup`. If installation fails, first manually add the RISC-V target: `rustup target add riscv32imc-unknown-none-elf`, then manually download the Xtensa toolchain (`rust-<ver>-x86_64-unknown-linux-gnu.tar.xz` from the `esp-rs/rust-build` releases), extract it, and install it with the included `install.sh --prefix=~/.rustup/toolchains/esp`; also add `rust-src-<ver>.tar.xz` (extract with `--strip-components=2` into the same toolchain directory for build-std).

## 6. First Connection and Full Backup

Connect with a USB-C cable that supports data transfer, then:

```bash
espflash board-info --port /dev/ttyACM0
```

Full backup:

```bash
mkdir -p backups
esptool.py --chip esp32s3 --port /dev/ttyACM0 --baud 921600 \
  read_flash 0x0 0x1000000 \
  backups/note4-factory-$(date +%Y%m%d-%H%M%S).bin
sha256sum backups/note4-factory-*.bin > backups/SHA256SUMS
```

On macOS, replace `/dev/ttyACM0` with `/dev/cu.usbmodem*` and use
`shasum -a 256` if `sha256sum` is unavailable. The expected size of
`backups/note4-factory-YYYYMMDD-HHMMSS.bin` is exactly `16777216` bytes.
Read it a second time to a different file and require identical SHA-256
values before treating it as a recovery image. Copy the verified backup and
hash to at least one physically isolated location.

`espflash save-image` does **not** read a connected device: it converts a
local ELF into a flashable image. Do not use it for factory backup.

## 7. Building the Rust Firmware

From a plain shell, run in the repository root:

```bash
./scripts/build-rust.sh --release
```

The script sources the ESP-IDF environment and then runs `cargo build --release` in `rust-firmware`. The artifact is at `rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4` (Linux has no Windows path length limit, so a fixed output directory is no longer needed).

After modifying the extra component configuration in `Cargo.toml`, if the new FFI module does not appear, clean `esp-idf-sys` and rebuild:

```bash
cd rust-firmware
cargo clean -p esp-idf-sys
cd ..
./scripts/build-rust.sh --release
```

## 8. Flashing and Serial Monitoring

Run from the repository root:

```bash
espflash flash \
  --port /dev/ttyACM0 \
  --before usb-reset \
  --chip esp32s3 \
  --flash-size 16mb \
  --flash-mode dio \
  --flash-freq 80mhz \
  --partition-table rust-firmware/partitions.csv \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4

espflash monitor --port /dev/ttyACM0
```

Exit the monitor with `Ctrl+C`. If flashing fails, exit the monitor first, then rerun the flash command.
NOTE4 uses the ESP32-S3 USB-Serial-JTAG peripheral; `--before usb-reset` is
the verified reset sequence. If automatic connection still fails, keep USB
connected, hold the voice/ENTER button (BOOT), press the left-side RESET
pinhole once, release BOOT, and retry. This is the same manual download-mode
sequence documented by the official ZECTRIX updater.

### Browser flashing with the official ZECTRIX tool

The official updater accepts a `.bin` at address `0x0000`, while this
repository's normal build artifact is an ELF. Generate a complete merged
image containing the bootloader, partition table, and application:

```bash
espflash save-image \
  --chip esp32s3 \
  --flash-size 16mb \
  --flash-mode dio \
  --flash-freq 80mhz \
  --partition-table rust-firmware/partitions.csv \
  --merge \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4 \
  inkwash-note4-merged.bin

wc -c inkwash-note4-merged.bin
sha256sum inkwash-note4-merged.bin
```

The byte count must be `16777216`. Open the
[official ZECTRIX firmware updater](https://zectrix.com/firmware-updater.html)
in a current desktop Chrome or Edge, connect the NOTE4 directly with a data
cable, choose `inkwash-note4-merged.bin`, keep the default write address
`0x0000`, and keep **Preserve device settings** enabled unless the test
explicitly requires a clean NVS. Keep the page open, USB attached, and the
computer awake until the tool reports both installation and restart complete.

The browser operates locally and uses Web Serial; Safari and Firefox are not
supported. Never select a NOTE4C image or change the write address for the
merged image. If settings backup fails, cancel and investigate before the
first test flash; continuing without preservation is only appropriate after
the device-specific full backup in §6 has been verified.

### Success Criteria

These are the minimal post-flash checks for the current Inkwash firmware;
the full on-device checklist (Wi-Fi sync, alarm ringing, reminders, BLE/USB
recovery, capacity) is §13.

1. After boot, the device stays powered when the power key is released.
2. The serial log shows `Inkwash NOTE4 Rust bring-up starting (git <rev>)`
   (plus the PCF8563 `read_time` line), then the main loop's periodic
   power/clock reports.
3. The screen renders the Home screen after an obvious full-refresh
   process: the clock, the NEXT ALARM / OPEN TODOS cards, and the battery
   indicator — no `Hello world`, no counters.
4. The firmware answers the USB control protocol:
   `inkwash-desktop --status <port>` returns a `Status { ... }` reply (it
   also echoes the recent boot log lines).
5. A long press of UP/DOWN opens the navigation drawer (HOME /
   CALENDAR / INBOX / ALARMS / TODOS / SETTINGS) and the pages render;
   short presses on Home are a deliberate no-op.

## 9. Display Data Conventions

| Item | Value |
| --- | --- |
| Resolution | 400 × 300 |
| 1bpp frame size | `400 * 300 / 8 = 15000` bytes |
| Row-major | MSB-first |
| Colors | `1` = white, `0` = black |
| BUSY | Active-low busy |
| Controller | SSD2683 |
| Official APIs | Full refresh, partial refresh, 16-level grayscale full refresh |
| After 4bpp full refresh | Partial refresh cannot be used again until another 1bpp full refresh completes |

Typical full-refresh lifecycle:

```text
zectrix_epd_power_on
zectrix_epd_refresh_full_1bpp
zectrix_epd_power_off
```

An e-paper display retains its last image after power-off; this is normal. Seeing the "old image" does not prove that the new firmware is not running — combine the serial log and the actual refresh flicker to judge.

This repository uses the official ZECTRIX EPD component and its calibrated waveform data (SSD2683); keep that implementation as the known-good baseline when changing display code.

## 10. Current Implementation Highlights (Source Index)

> This section only collects **low-level implementation details** that do not fit into the module table of `rust-firmware/AGENTS.md`
> (ADC read strategy, I2C power-up sequencing, RTC register layout, etc.); application-layer behavior (menu structure, command
> protocol, sync flow) is governed by the root README and `rust-firmware/AGENTS.md` and is not duplicated here.

- The `main.rs` main loop polls the three keys at `POLL_INTERVAL_MS = 20` (ms); key events are carried via `Option<ButtonEvent>`. A short press does nothing on Home; a long press of UP/DOWN opens the navigation drawer (`screens::open_navigation`); when a redraw is needed, the corresponding `Rect` is collected into the `dirty` list, and one partial/full refresh is issued after the round ends.
- `board.rs::take()` centralizes initialization: the power latch is pulled high, AVDD is pulled low (off; pulled high again before I2C0 init), keys use `Pull::Up`, `charging` uses `Pull::Up`, and `charge_done` is a floating input. The status LED (`GPIO3`, active-low, officially named `ZECTRIX_POWER_LED`) is driven by `update_charging_led` as an external-power indicator: on when the charger is plugged in, off when unplugged.

### Storage and Power State

- `storage.rs` wraps the default NVS partition (a `nvs` 24 KiB partition is declared in `partitions.csv`) under the `inkwash` namespace. `PersistedCounters` (historically evolved from button-counter persistence; the name stuck) now stores six keys: `wifi_ssid` / `wifi_pass` / `server_url` / `auth_token` / `sync_etag` / `timezone_min`; `open()` calls `EspDefaultNvsPartition::take()` to initialize the NVS flash automatically. The `AlarmStore`/`TodoStore` in `alarms.rs`/`todos.rs` each hold additional namespaces of the same NVS partition and store whole JSON blobs rather than per-field keys.
- `board.rs::battery_millivolts()` uses the ESP-IDF 5.x ADC oneshot API: `AdcDriver::new(peripherals.adc1)` holds the ADC unit persistently; each voltage read takes `GPIO4` (ADC1 CH3 on the ESP32-S3) via `Peripherals::steal()` and temporarily constructs `AdcChannelDriver::new(&self.adc, gpio4, &BATTERY_ADC_CHANNEL_CONFIG)`. Because every field of `Note4Board` is `'static`, putting the channel directly into the board would trigger a borrow conflict, so the strategy is to "rebuild the channel on each read"; the return value is ESP-IDF mV × 2 (onboard 1:2 divider) and is clamped to `u16::MAX`. `BATTERY_ADC_CHANNEL_CONFIG` enables `Calibration::Curve` (eFuse three-point fitting, matching the official demo's `adc_cali_curve_fitting`) and averages `BATTERY_ADC_SAMPLES = 10` readings to suppress jitter.
- The `main.rs` main loop calls `report_power_state()` every `STATUS_REPORT_INTERVAL_POLLS` (50 polls ≈ 1 s), printing `Power state: power_present=… charging=… full=… vbat_mV=… (..%)`; it re-reads the PCF8563 every `CLOCK_POLL_INTERVAL_POLLS` (60 polls ≈ 1.2 s) and marks `CLOCK_RECT` as dirty to trigger a partial refresh only when the second/minute/hour change, avoiding a redraw on every poll.
- Charging state is determined by the `ChargeStatus` state machine in `board.rs` (ported from the official demo's `charge_status.cc`, a simplified debounced version); each tick ≈ 1 s, and stabilization requires 2 consecutive ticks: `power_present` (any status line active), `charging` (`CHRG_L = GPIO2` low and not full), `full` (`STDBY_H = GPIO1` high). `report_power_state` calls `charging_state()` to advance the state machine; the rendering path only reads `charge_snapshot()` so different call frequencies do not break debouncing. Confirmed on real hardware: when fully charged and plugged in, the state is `power_present=true charging=false full=true` (the charging IC has stopped charging; `charging=false` is the real state, not a bug).
- `sdkconfig.defaults` explicitly enables `CONFIG_NVS_ENABLED=y` and `CONFIG_ADC_ONESHOT_ENABLED=y`; ADC eFuse curve calibration is enabled via `Calibration::Curve` and needs no extra config.
- The battery percentage `battery_percent_from_mv` uses the official demo's quadratic polynomial `(-mv² + 9016·mv - 19189000)/10000` (0% ≈ 3444 mV, 100% ≈ 4200 mV), which fits the LiPo discharge curve better than the old 3300–4200 linear mapping (the 4.0 V plateau no longer reads artificially high).

### PCF8563 RTC and I2C0

- Bus: `I2C0`, `SDA = GPIO47`, `SCL = GPIO48`, 400 kHz master mode. `board.rs` calls `I2cDriver::new` only after `_avdd_power` (`GPIO42`, used by the factory as the power source for audio + I2C pull-ups) is initialized high; otherwise the floating SDA/SCL lines NACK.
- Device address: `0x51` (7-bit); `rtc::Pcf8563::probe()` performs a one-byte read of 0x00 as a connectivity test during board initialization; failure makes `Note4Board::take()` return an error.
- Register layout: BCD; second/minute/hour/day/weekday/month/year are at `0x02..=0x08`; bit7 of `0x00` is `voltage_low` — VL=1 means the RTC backup battery lost power or this is the first power-on, so the time can no longer be trusted.
- `main.rs` boot sequence: `read_time()` → print; when `voltage_low`, rewrite via `from_unix(BUILD_EPOCH_SECS)` (`BUILD_EPOCH_SECS` is injected into `cargo:rustc-env` by `build.rs` using `SystemTime::now()`, refreshed at build time).
- Alarms and square wave: after startup, `Pcf8563::clear_alarm()` in `board.rs` clears the alarm registers `0x09..=0x0C` and the AIE bit in `0x01` so a leftover alarm cannot interrupt subsequent deep-sleep wake-ups; `alarms::program_hardware_alarm()` then immediately rewrites the nearest stored alarm into the same register group (the chip has only one hardware alarm slot; in multi-alarm scenarios the firmware itself picks the nearest one).

## 11. Common Failures

### Firmware flashes successfully but keeps resetting

`sdkconfig.defaults` must contain:

```text
CONFIG_ESPTOOLPY_FLASHMODE_DIO=y
```

Do not change it to QIO.

### The log says refresh completed, but the screen does not move at all

- Confirm that the official `zectrix_epd` component from this repository is used, not a simplified SSD1683 command sequence.
- Confirm that the waveform header exists and is compiled into `libzectrix_epd.a` (the build log should show `__idf_zectrix_epd.dir/zectrix_epd.cc.obj`).
- Confirm that the `GPIOGPIO6` power is managed by the driver (`zectrix_epd_power_on/off`).

### `cargo` cannot find the `zectrix_epd` module

Usually the `esp-idf-sys` cache predates the extra component configuration:

```bash
cd rust-firmware
cargo clean -p esp-idf-sys
cd ..
./scripts/build-rust.sh --release
```

### `.cargo-lock` or the target directory is locked

Close any running Cargo, rustc, IDE check tasks, or stale build processes, then retry.

### Wi-Fi connection fails (`reason=201` / `NO_AP_FOUND`), but a manual scan sees the target AP

`EspWifi::connect()` internally triggers a directed scan with SSID filtering that is case-sensitive byte by byte; a manual `scan()`
has no filter and returns all APs. When the two results differ, first confirm that the SSID stored in NVS matches the router's
actual broadcast exactly in case (`gen-nvs-wifi.py` provisioning SSIDs typed by hand often differ in case from what the AP
broadcasts, e.g., `XiaoMi_ED4E` vs the router's actual `Xiaomi_ED4E`) — the SSID printed by scan is the authoritative source.

### Cannot open the serial port

On Linux, confirm the user is a member of the `uucp` group (run `sudo
usermod -aG uucp $USER` and log in again), close other monitors / serial
tools, and check the device node with `ls /dev/ttyACM*` (macOS:
`/dev/tty.usbmodem*`). If necessary, hold ENTER/BOOT and trigger a reset
to enter download mode.

espflash failing with `Device error, e.g. paper out` or `No such file or
directory (os error 2)` usually means the device is not actually on the
bus. **Check the physical USB connection first** (cable, hub, port) and
re-seat it — in practice a disappearing node has been a loose/unplugged
cable, not a tooling problem. The node briefly vanishing and
re-enumerating during a re-plug is normal; wait a few seconds and retry
before suspecting the software. (Opening the USB-Serial/JTAG port does
reset the board, but that alone does not make the node disappear.)

To confirm the chip is reachable and flashable at all:

```bash
espflash board-info --port /dev/tty.usbmodem1101
```

A successful `board-info` (chip id, flash size, security info) means the
device is alive. If the node exists but every tool reports the device
missing, the USB link is down — re-seat the cable.

Verifying the running firmware: `espflash monitor` needs a real TTY
(`Failed to initialize input reader` outside an interactive shell) and a
plain `cat` on the node may read nothing, so a silent serial log is not
proof of a dead device. The authoritative check is the USB control
protocol:

```bash
inkwash-desktop --status /dev/tty.usbmodem1101
```

A `Status { ... }` reply proves the freshly-flashed firmware booted and
is polling commands; the CLI also echoes the recent boot log lines
alongside the reply.

### Watching logs on a running device without disturbing it

The board's USB-Serial/JTAG peripheral shares its modem-control lines
(DTR/RTS) with GPIO0 - the ENTER button and the boot-mode strap - so
*how* a host tool opens the port decides whether the device keeps
running or gets a spurious keypress, a reset, or a stuck download-mode
entry. These rules are load-bearing for any interactive debugging
session; they were learned the hard way during migration smoke tests
(see `control-protocol.md` for the underlying wiring and
`inkwash-desktop/src/transport/usb.rs` for the reference
implementation):

For agent-driven tests, use the repository's non-interactive collector instead
of `espflash monitor`:

```bash
mkdir -p test-results
python3 scripts/capture-serial.py \
  --duration 420 \
  --output test-results/note4-serial.log \
  --expect 'Inkwash NOTE4 Rust bring-up starting' \
  --expect 'EPD refresh completed'
```

It requires `pyserial`, runs without a TTY, timestamps every line in UTC,
releases DTR/RTS **before** opening the device, reconnects after USB
re-enumeration, and exits by itself. A missing `--expect` pattern makes the
command fail, so an agent cannot mistake an empty log for a passing test. Pass
`--port /dev/cu.usbmodem1101` when more than one serial device is connected;
otherwise the script auto-detects `/dev/cu.usbmodem*` and `/dev/ttyACM*`.

Opening this board's USB Serial/JTAG port can still produce
`rst:0x15 (USB_UART_CHIP_RESET)` even when DTR/RTS were set inactive before
`open()`; this was reproduced with the collector on 2026-09-02. Treat a
collector session as the start of a fresh boot unless the captured reset reason
proves otherwise. It is suitable for boot and post-boot smoke evidence, but it
cannot prove that a previously running uptime/soak session survived, and an
automatic reconnect can contaminate deep-sleep or maintenance-wake evidence.
Use photos/video, the displayed clock/action, and the final reset reason for
those tests; do not claim a non-disruptive sleep/soak pass from USB logs alone.

The collector is read-only and must not run at the same time as flashing or an
`inkwash-desktop` USB command because a serial port has one owner. For a smoke
test, use this evidence sequence:

1. Flash the image and wait for the flasher to release the port.
2. Run `inkwash-desktop --status <port> > test-results/status.txt 2>&1`. The
   status command supplies the revision and recent boot lines even if the
   collector missed the first two seconds.
3. Start `capture-serial.py` for the bounded test window, then perform physical
   button, sleep, alarm, or display actions. Do not run another USB command
   until capture exits.
4. Evaluate the log with explicit `--expect` expressions or `rg`; preserve the
   log and command exit status in the test record. Log silence is a failed or
   blocked evidence step, never a pass.

The lower-level rules remain:

1. **Release DTR and RTS before opening the port, but do not assume this makes
   opening non-disruptive.**
   Host defaults vary (pyserial asserts DTR by default, and on some
   platforms opening alone asserts both lines). Asserting either pulls
   GPIO0 low, which the firmware cannot distinguish from a real ENTER
   press - the device may open a menu, dismiss an alarm, or appear to
   "not respond" while it is actually busy handling your fake press.
   Desktop releases them immediately after `open()`:
   `write_data_terminal_ready(false)` + `write_request_to_send(false)`.
   The collector creates a closed `Serial(port=None)`, sets both values false,
   and only then calls `open()`. This reduces one known reset source, but the
   macOS/USB stack can still cause `USB_UART_CHIP_RESET`; always inspect the
   captured reset reason.

2. **Do not run `espflash monitor` / `board-info` / `reset` against a
   device that is running application firmware you want to keep alive.**
   These tools connect through the ROM bootloader and upload a flash
   stub, which halts or wedges the running app (observed: a live device
   went unresponsive until physically re-plugged). They are for
   flashing and bootloader-level checks, not for observing a running
   system.

3. **Do not require the raw collector to prove early boot.** The app prints
   its banner during the small gap between flashing and reopening the port.
   Use `inkwash-desktop --status` for the buffered recent boot lines and use
   the collector for subsequent runtime events. If an unbuffered ROM/early
   boot line is essential, run `espflash flash --monitor` from a real PTY and
   treat that as a separate, interactive diagnostic run.

4. **Prefer `inkwash-desktop` for talking to a live device.** Its USB
   transport already implements rules 1 and 2, and `--status` /
   `--sync` exercise the real control protocol. Write a throwaway
   pyserial reader only when you need raw log bytes and cannot use the
   desktop CLI.

5. **When a reset is genuinely required while the app is running,
   use the physical ENTER/BOOT button** (hold it and trigger a reset to
   enter download mode) rather than a tool's DTR/RTS reset - the strap
   is what the ROM bootloader expects, and it avoids the spurious-press
   side effects above. After any tool-induced reset the USB node may
   vanish for several seconds or need a physical re-plug; that is the
   USB-JTAG re-enumerating, not necessarily a dead board.

### Powers off shortly after power-on

`GPIO17` is the main power soft latch. It must be configured as a high output early in boot. An RTC GPIO hold must also be designed before entering deep sleep, otherwise the device may truly power off.

### The screen does not clear immediately

This is normal. An e-paper display holds its image when powered off; only a valid refresh waveform changes the content.

## 12. Pre-Commit Checks

```bash
cargo +esp fmt --manifest-path rust-firmware/Cargo.toml -- --check
./scripts/build-rust.sh --release
```

The host-testable `logic/` crate is checked by CI on every push/PR
(`cargo test --locked` + rustfmt + clippy) and locally with
`cd logic && cargo test`; the firmware crate itself needs the ESP-IDF
toolchain and is verified on device.

Check on real hardware at least once: cold boot, power hold, initial full refresh, one press of each of the three keys, USB reconnection, and the serial log.

Do not commit `sdkconfig`, build directories, factory backups, or logs containing device credentials. The current `.gitignore` already excludes `build/`, `managed_components/`, `dependencies.lock`, `sdkconfig`, `sdkconfig.old`, `backups/*.bin`, `rust-firmware/target/`, and `rust-firmware/.embuild/`.

For each new peripheral, run standalone tests first, then integrate it into the main application. Display, power, and sleep changes carry the highest risk; always keep a recoverable serial path and the factory backup.

## 13. Physical-Hardware Test Procedure

This procedure is the release gate for behavior that host tests cannot
exercise: power retention, USB reset behavior, I2C peripherals, the real EPD
waveform, RTC wake, audio, Wi-Fi/BLE radios, and NVS persistence. Run the
quick gate after every firmware flash. Run the affected feature stages after
changes to their modules, and run the full gate before a release.

### 13.1 Test record and stop rules

Create one result record per device and build. Record:

| Field | Required value |
| --- | --- |
| Device | NOTE4 black-and-white; device label or MAC; never a NOTE4C |
| Build | `git rev-parse HEAD`, `git describe --always --dirty`, merged-bin SHA-256 |
| Flash | CLI or official browser updater; whether settings were preserved |
| Network | AP model/security mode; server version or commit |
| Times | start/end time and timezone |
| Evidence | boot log, photos of display/alarm, command results, failure timestamps |

Stop the run immediately and restore or diagnose before further refreshes if
the device power-cycles repeatedly, boot reports QIO, the EPD BUSY line never
finishes, the panel shows abnormal sustained noise rather than a normal flash,
the enclosure becomes hot, or the flashed revision differs from the test
record. Do not repeatedly power-cycle or refresh the EPD to “see if it clears.”

Use one continuous log session when raw logs are required. Opening or reopening
serial tools can reset this board and invalidates pre-existing sleep, wake,
uptime, and alarm evidence.
For agent-driven evidence, use the bounded `scripts/capture-serial.py` command
from §11 and list required `--expect` patterns before starting the stage. Do
not run `inkwash-desktop`, `espflash`, or a second reader while it owns the
port. Use `inkwash-desktop --status <port>` and `--sync <port>` only between
capture windows; reserve `espflash monitor` and `board-info` for the
flash/debug boundary described in §11.

### 13.2 Stage A — preparation and image gate

- [ ] Confirm the case/display is the black-and-white NOTE4 and match it to
      the device-specific 16 MiB backup and SHA-256 from §6.
- [ ] Run `cargo +esp fmt --manifest-path rust-firmware/Cargo.toml -- --check`,
      `cd logic && cargo test --locked`, then `./scripts/build-rust.sh --release`.
- [ ] For browser flashing, generate the merged image with §8 and require a
      `16777216` byte file. Record its SHA-256 before selecting it in Chrome or
      Edge. For CLI flashing, require DIO, 80 MHz, 16 MB, the repository
      partition table, and `--before usb-reset`.
- [ ] Close all other serial clients. Keep the computer awake and USB directly
      connected until flash verification and restart complete.

**Pass:** checks/build succeed, artifact identity is recorded, and the flasher
reports a verified write and restart. **Fail:** wrong model/image, size/hash
mismatch, a settings-backup failure without an approved clean-NVS test, or any
write/verify error.

### 13.3 Stage B — five-minute quick gate

- [ ] Release the power key after boot; the device remains powered.
- [ ] Boot shows `Inkwash NOTE4 Rust bring-up starting (git <rev>)`; `<rev>`
      matches the recorded build. Confirm `mode:DIO`, a valid PCF8563 read (or
      an explicit VL reseed), initial EPD refresh, and no panic/watchdog loop.
- [ ] Home finishes one normal full refresh and shows a plausible clock,
      battery, next-alarm, and Todo state. Full-refresh flashing/brief inversion
      is normal; a permanently busy or noisy panel is not.
- [ ] Short-press ENTER, UP, and DOWN once. Long-press UP or DOWN, navigate all
      top-level pages, then return Home; no key sticks or phantom ENTER occurs.
- [ ] Run `inkwash-desktop --status <port>` once. It returns `Status { ... }`
      within the default 35 s timeout and echoes the expected boot revision.
- [ ] Unplug USB, confirm battery operation, then reconnect once and repeat
      `--status`. Verify retained configuration and a clean boot.

**Pass:** all items pass without an unexplained reset. This is the minimum gate
for any flashed build. A compile or successful write alone is not a hardware
pass.

### 13.4 Stage C — Wi-Fi, sync, and persistence

- [ ] Save a deliberately invalid SSID/password through the desktop UI; require
      an explicit failure. Reboot and confirm the previously valid credentials
      still work.
- [ ] Save the valid 2.4 GHz credentials, reach DHCP, and run
      `inkwash-desktop --sync <port>`. Verify alarms, Todos, and Inbox both in
      `--status` and on their device pages.
- [ ] Change one server-side item, sync again in the **same boot**, and verify
      the second result. This guards the historical second-connect failure.
- [ ] Mark a supported local state change, sync, reboot, and sync again. Verify
      the server merge and the device NVS state agree; no older payload replaces
      the change.
- [ ] If PMF behavior changed, repeat against the target PMF-required or
      PMF-optional AP and require association plus DHCP.

**Pass:** failure does not corrupt known-good credentials; two same-boot syncs
and one rebooted persistence cycle converge without panic, watchdog, or lost
state.

### 13.5 Stage D — display, sleep, and wake

- [ ] Leave the Home screen through at least two minute changes. Each displayed
      minute advances and later EPD refreshes continue; capture
      `EPD refresh completed` or documented full-recovery logs.
- [ ] Disconnect the USB data host, then leave the device untouched for more
      than five minutes. Confirm deep sleep from display behavior and the next
      wake cause; an attached USB host intentionally blocks both sleep tiers.
- [ ] Press UP only: it must not wake deep sleep (GPIO39 limitation). Press
      ENTER, then repeat with DOWN in a separate sleep cycle; each wakes and
      redraws a current Home screen.
- [ ] Allow one maintenance-timer wake cycle. The displayed time must not remain
      permanently frozen, and there must be no reset loop or EPD task stall.
- [ ] Inspect the screen after several partial and full refreshes. Text remains
      readable and ghosting clears on a full refresh.

**Pass:** time continues advancing, ENTER/DOWN wake reliably, UP behavior
matches the documented hardware limitation, and refresh processing does not
stall after the first command.

### 13.6 Stage E — offline alarm and reminder gate

- [ ] Sync a one-shot (`Once`) alarm several minutes ahead, then disable Wi-Fi
      or make the server unreachable and let the device enter deep sleep.
- [ ] The RTC wakes the device and audio/display alarm begins at the scheduled
      local time. Record scheduled and observed times; ENTER stops sound within
      about one second.
- [ ] Confirm the PCF8563 AF flag is acknowledged and the fired one-shot is
      removed; logs show either the next alarm armed or the hardware alarm
      cleared.
- [ ] Repeat with one recurring `Daily`, `Weekly`, or `Monthly` alarm and verify
      its next occurrence is re-armed after dismissal.
- [ ] During a separate ringing run, send a USB command and require
      `{"status":"busy"}` within a few seconds. If BLE control changed, repeat
      the busy-response check over BLE.
- [ ] Trigger a due high-importance Todo and an urgent `Alert`/`High` Inbox
      reminder. Each fires once per intended rule, interrupts the current page,
      dismisses with ENTER, and redraws that page correctly. Reboot on the same
      date and confirm the Todo does not duplicate.

**Pass:** the one-shot alarm rings entirely offline from deep sleep, dismissal
clears/re-arms hardware state correctly, and reminder dedup survives reboot.
Until this stage passes, alarm behavior must be reported as unverified on
hardware.

### 13.7 Stage F — BLE/USB recovery and capacity

- [ ] Open the BLE pairing page, connect, send `get_status`, and receive a
      reply. Leave the page and verify advertising stops; reconnect in a fresh
      pairing session.
- [ ] Perform three controlled USB unplug/replug cycles, running one `--status`
      after each. Then perform BLE connect/disconnect and a Wi-Fi sync; all
      transports remain usable.
- [ ] Sync payloads near `alarms::BLOB_BUF_LEN`, `todos::BLOB_BUF_LEN`, and
      `inbox::BLOB_BUF_LEN`. Verify the documented limit/truncation behavior,
      pagination, successful NVS writes, and a clean reboot/readback.

**Pass:** transport teardown does not poison the next transport or Wi-Fi, and
near-limit state survives NVS persistence without silent loss.

### 13.8 Stage G — soak and release decision

- [ ] Leave the normal release configuration running for at least six hours,
      spanning multiple urgent polls, full-sync boundaries, deep-sleep entries,
      and maintenance wakes. Do not reopen/reset the serial port during the run.
- [ ] At the end, record `--status`, displayed time, battery state, last
      successful sync, and any reset reason. Perform one final manual sync and
      one navigation pass.

**Pass:** no panic, watchdog abort, unexplained reset, frozen clock, transport
loss, or missed scheduled action. Mark each stage `PASS`, `FAIL`, or `BLOCKED`;
`BLOCKED` is not a pass. A release is hardware-verified only when Stages A-G
pass on the target NOTE4. For a narrow change, record exactly which stages were
rerun and keep untouched stages tied to their most recent build/device record.

## 14. Restoring Factory Firmware

Only use the full backup **of that specific device**:

```bash
esptool.py --chip esp32s3 --port /dev/ttyACM0 --baud 921600 \
  write_flash --flash_mode dio --flash_freq 80m --flash_size 16MB \
  0x0 backups/note4-factory-20260815-213553.bin
```

After restoring, re-read the Flash or compute the backup file hash to confirm the correct image was used. Never write another device's or a NOTE4C's backup into this device.

## 15. References

- ZECTRIX support and USB tool: <https://zectrix.com/support.html#firmware-updater>
- ZECTRIX firmware updater: <https://zectrix.com/firmware-updater.html>
- ZECTRIX open-source resources: <https://www.zectrix.com/open-source.html>
- NOTE4 hardware specifications: <https://wiki.zectrix.com/zh/hardware/note/spec>
- Firmware resources: <https://wiki.zectrix.com/zh/software/firmware>
- Community open-source firmware: <https://wiki.zectrix.com/zh/software/Community-OpenSource-Firmware>
- Official NOTE4 EPD Demo: <https://github.com/itopinion/zectrix-note4-epd-demo>
- Slate reference firmware: <https://github.com/qiujun8023/slate>
- Rust on ESP Book: <https://docs.esp-rs.org/book/>
- espflash: <https://github.com/esp-rs/espflash>
