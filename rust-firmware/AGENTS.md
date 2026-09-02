# rust-firmware — inkwash-note4 crate

## OVERVIEW
inkwash-note4 ESP32-S3 bin crate; entry `main.rs:114` (`[[bin]] harness=false`, no lib). Firmware main loop drives `logic::app::AppRunner` via the thin `app_runner` adapter.

## MODULE MAP (src/) — 33 flat modules
| Group | Module | Responsibility |
|------|------|------|
| Orchestration | `main.rs` | boot + dispatch_app_runner `:1135`; AppRunner feeding |
| Orchestration | `app_runner.rs` | thin `AppRunner` adapter; `Effect::StopTone` MUST NOT `set_mute(true)` `:121`; minute ticks MUST NOT full-refresh `:209-213,247-248` |
| Orchestration | `ctx.rs` | `DeviceContext` + `SyncScheduler` + `AlarmScheduler`; page-owned alarm entry MUST NOT ACK/rearm RTC `:209-213,221-224`; `urgent_synced` flag `:450-454,530-533` |
| Hardware | `board.rs` | `Note4Board::take()`; GPIO42 AVDD before I2C0 init `:69-70,87`; battery ADC GPIO4 steals `:399-400`; GPIO17 `release_power_latch_hold()` first `:61` |
| Hardware | `power.rs` | deep sleep + wake-up reasons; GPIO39 not RTC-capable `:32-33,80-81`; deep sleep MUST NOT inherit light-sleep GPIO wakeup arming `:74-78` |
| Hardware | `rtc.rs` | PCF8563 + single hardware alarm register `:165-170` |
| Hardware | `wake.rs` | ISR disable-pin-first `:34` |
| Hardware | `button.rs` / `watchdog.rs` / `audio.rs` / `nfc.rs` | debounce; WDT; ES8311; GT23SC6699 |
| Background task | `epd_task.rs` | refresh owner + recovery; notify channel must stay wired `:215-219` |
| Background task | `sync_task.rs` | HTTPS owner; `EspNvs` Send-not-Sync; NEVER touches I2C `:9-12`; main loop owns RTC reprogram + NTP align |
| Rendering | `canvas.rs` / `font8x16.rs` / `font5x7.rs` / `font_cjk.rs` / `home.rs` / `icons.rs` | pure layout, EPD-FFI-free, PC-previewable via `tools/preview` `#[path]` |
| Rendering | `display.rs` | EPD FFI wrapper + `render_home` delegation; UI screens best-effort `:129-134` |
| UI | `ui.rs` / `screens.rs` | generic 3-button; `UiResult::AlarmInterrupted` MUST NOT be user-cancel `:145-146`; `screens.rs` 1612 LOC, nav drawer/calendar/alarms/todos/settings; `pick_from_list` returns `PickResult::{Selected,Cancelled,OpenNav}` |
| Services | `alarms.rs` / `todos.rs` / `inbox.rs` / `storage.rs` / `reminders.rs` / `nvs_blob.rs` | NVS stores; alarms owns ringing/ack + RTC scheduling; reminders owns full-screen alerts |
| Protocol | `control.rs` / `usb_console.rs` / `ble_control.rs` / `sync.rs` / `wifi.rs` | NVS-changing commands must `dirty.push(FULL_SCREEN_RECT)` in main poll `:294-328`; NimBLE callback thread MUST NOT touch `Note4Board` `:80-90`; `BLEDevice::init()` after `deinit_full()` `:54-58`; multi-connect OK since scan removed 2026-08-17; `esp_wifi_stop()` forbidden `:34-55,223-227`; legacy `restart_for_fresh_wifi_session` dead `:253-254` |

## FFI COMPONENT: components/zectrix_epd/
Official SSD2683 C++ driver (CMakeLists.txt + `zectrix_epd.cc` + `include/zectrix_epd.h` + `private_include/ssd2683_waveform.h`). Waveforms must not be deleted. After modifying, `cargo clean -p esp-idf-sys`. EPD power/timing only through `zectrix_epd_power_on/off` FFI.

## CONVENTIONS (crate-level)
- `anyhow::Result`; all peripheral init in `Note4Board::take()`; `Peripherals::steal()` only allowed in `board.rs:399-400` + `wifi.rs:66-73`.
- `main.rs` mod decls alphabetical.
- Glyph tables `#[rustfmt::skip]`.
- `build.rs` injects `BUILD_EPOCH_SECS` (RTC fallback) + `GIT_REV`.
- rustflags include `--cfg espidf_time64`; `harness=false`.

## ANTI-PATTERNS (code-enforced)
- **Wi-Fi** (`wifi.rs`): multi-connect OK since scan removed 2026-08-17; `esp_wifi_stop()` forbidden `:34-55,223-227`; `esp_restart()` strictly forbidden — restart = `power::enter_deep_sleep_with_wakeups` + ~100ms timer wake only; legacy `restart_for_fresh_wifi_session` dead `:253-254`.
- **Sync scheduler** (`ctx.rs`): BLE pairing USB-only polling; BLE+Wi-Fi share radio, never simultaneous.
- **BLE** (`ble_control.rs`): callback thread MUST NOT touch `Note4Board` `:80-90`; `BLEDevice::init()` after `deinit_full()` `:54-58`.
- **RTC** (`rtc.rs:165-170` + `alarms::next_due`): single slot → soonest always written; boot `clear_alarm()` residue.
- **Power sequencing**: GPIO42 AVDD high before I2C0 init (`board.rs:69-70,87`); `release_power_latch_hold()` first on wake (`board.rs:61`).
- **EPD**: 1bpp full refresh required once after 4bpp full before partial resumes; power/timing only via `zectrix_epd_*` FFI.
- **Audio trap**: `Effect::StopTone` MUST NOT `set_mute(true)` (`app_runner.rs:121`).
- **Minute ticks**: MUST NOT full-refresh (`app_runner.rs:209-213,247-248`).
- **Id non-collision** within list (sync-api contract); USB commands polled across Home/content/nav/number entry/inbox detail; only time-critical screens defer.
- **Remote-triggered state changes** (`main.rs:294-328`): `SyncNow`/`clear_alarms` etc. must `dirty.push(FULL_SCREEN_RECT)` in main poll; capture command variant with `matches!` before dispatch (`Command` not `Copy`); push only on `Reply::Ok`.
- **EPD supersede contract** (`main.rs:674-678`): superseded refresh MUST NOT be reported `RenderDone`.
- **BootSnapshot AF/AIE residue**: interpreted only on successful snapshot (`main.rs:386-390`).
- **Light-sleep config failure** MUST NOT abort (`main.rs:322-324`).
- **EPD notify channel** must stay wired (`epd_task.rs:215-219`).
- **GetStatus polls are user activity** — no deep sleep mid-session (`main.rs:867-871`).

## COMMANDS
```bash
./scripts/build-rust.sh --release   # run from the repo root; bare `cargo build` requires a sourced ESP-IDF env
cargo +esp fmt -- --check           # inside the crate directory
cargo clippy                        # requires sourcing the ESP-IDF environment first; zero warnings
cd tools/preview && cargo run --release   # PC preview: home + sub-screen PNGs (real home/canvas/font/icons via `#[path]`)
```

## NOTES
- `IDF_PATH`/`LIBCLANG_PATH` in `.cargo/config.toml` are machine-specific; resolved by `scripts/build-rust.sh` (see root AGENTS.md NOTES).
- rust-analyzer depends on repo-root `.vscode/settings.json` + the `esp-ra` toolchain with the IDF env loaded in the parent shell; diagnostics show in the editor after edits.
- Changing `sdkconfig.defaults` cross-check root red line #2 (DIO) and `partitions.csv` 6-level relative-path rule.
- No tests in-crate; run the `logic/` host suite. Shared `ui::header`/row chrome iterates in `tools/preview` first, then flash.
