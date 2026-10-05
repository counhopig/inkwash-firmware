# Inkwash — Note 4 firmware

**Generated:** 2026-10-05
**Commit:** b42a686
**Branch:** fix/sync-persistence-and-ble-reply-fence

## OVERVIEW

Offline calendar, alarms, todos and notifications; C++17, ESP-IDF 5.5.5,
LVGL 9, ESP32-S3 and a 400 × 300 monochrome e-paper panel.

## STRUCTURE

```text
firmware/
├── main/             # One ESP-IDF application component
│   ├── app/          # State owner and event orchestration
│   └── core/         # Hardware-free domain and policies
├── components/       # First-party EPD driver; LVGL Git submodule
├── assets/           # Embedded CJK blobs and license
└── test/             # Host C++ tests
scripts/              # build, device, checks, assets, release
```

## WHERE TO LOOK

| Task | Location | Notes |
| --- | --- | --- |
| Startup | `firmware/main/main.cc` | `app_main`, initialization and recovery |
| Runtime behavior | `firmware/main/app/` | Commands, sync, input, alarms and lifecycle |
| Domain rules | `firmware/main/core/` | Models, calendar, JSON and policies |
| UI | `firmware/main/ui/` | LVGL screen construction |
| Hardware | `firmware/main/board_pins.h` | Shared pin definitions |
| Panel transport | `firmware/components/zectrix_epd/` | SPI, BUSY and waveform handling |
| Build inputs | `firmware/main/CMakeLists.txt` | Explicit source list and embedded fonts |
| Configuration | `firmware/sdkconfig.defaults` | Target, memory, radio and LVGL |
| CI | `.github/workflows/ci.yml` | Host tests, build, ledger and revision gates |

## CODE MAP

Refs below count first-party files containing the symbol, including declarations;
namespace collisions are omitted.

| Symbol | Type | Location | Refs | Role |
| --- | --- | --- | --- | --- |
| `app_main` | Function | `firmware/main/main.cc` | — | Hardware bootstrap |
| `app::Run` | Function | `firmware/main/app/app.cc` | — | Event loop |
| `DateTime` | Struct | `firmware/main/core/datetime.h` | 15 | Local calendar arithmetic |
| `power_policy::CanSleep` | Function | `firmware/main/core/power_policy.cc` | 3 | Pending-work gate |
| `power_policy::NextWakeSecs` | Function | `firmware/main/core/power_policy.cc` | 3 | Minute and maintenance wake deadline |
| `refresh_policy::Choose` | Function | `firmware/main/core/refresh_policy.cc` | 3 | None, partial or full refresh |

## CONVENTIONS

- Add application sources to the explicit `firmware/main/CMakeLists.txt` list.
- Main component compiles as C++17 with `-Wall -Wextra -Werror`.
- Keep LVGL as the pinned Git submodule; first-party driver changes belong in `zectrix_epd`.
- Existing device NVS and server/control JSON contracts remain compatibility boundaries.
- RTC calendar epochs are local time; network timestamps require explicit UTC conversion.

## ANTI-PATTERNS

- Do not silently erase NVS on initialization errors (`firmware/main/storage/store.cc:224`).
- Do not treat host core tests as evidence for task, radio, panel or alarm hardware behavior.
- Do not add generated build output, serial logs or device backups to tracked sources.

## COMMANDS

```sh
git submodule update --init firmware/components/lvgl
./scripts/build/build-cpp.sh
IDF_PATH=/path/to/esp-idf bash firmware/test/run.sh
./scripts/checks/check-boot-ledger.sh firmware/build/inkwash.elf
./scripts/checks/check-boot-ledger.sh --self-test
./scripts/checks/check-git-rev.sh firmware/build/inkwash.elf
```

## NOTES

- `SOURCE_DATE_EPOCH` overrides the build-time RTC seed; Git description is embedded separately.
- Build defaults to `firmware/build/`; Windows defaults to `C:\ikc`.
- `INKWASH_CPP_BUILD_DIR` selects another build directory.
- Binary font assets are committed; normal builds do not regenerate them.
- Local `logs/` and `backups/*.bin` are ignored; opening USB Serial/JTAG may reset the chip.
