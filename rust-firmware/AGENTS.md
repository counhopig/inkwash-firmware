# rust-firmware — inkwash-note4 crate

## OVERVIEW
inkwash-note4 ESP32-S3 bin crate; entry `main.rs:114` (`[[bin]] harness=false`, no lib). Firmware main loop drives `logic::app::AppRunner` via the thin `app_runner` adapter.

> **Anchor discipline.** Every `file:line` below was re-verified against the working tree. Symbol names age better than line numbers, so prefer the named symbol when one is given. Line numbers are a convenience for the current revision, not the contract.

## MODULE MAP (src/) — 36 flat modules
| Group | Module | Responsibility |
|------|------|----------------|
| Orchestration | `main.rs` | boot + `dispatch_app_runner` `:1748`; AppRunner feeding; boot-fact gate `:498-505,1252` |
| Orchestration | `app_runner.rs` | thin `AppRunner` adapter; `Effect::StopTone` MUST NOT `set_mute(true)` `:260`; minute ticks MUST NOT full-refresh — the decision moved to the host-testable planner (`logic/src/render_plan.rs:235`) |
| Orchestration | `ctx.rs` | `DeviceContext` + `SyncScheduler` + `AlarmScheduler`; per-transport reply latches and BLE/Wi-Fi radio handoff state `:250-400` |
| Hardware | `board.rs` | `Note4Board::take()`; GPIO42 AVDD `:237` before I2C0 init `:270`; battery ADC GPIO4 steal `:416-419`; `release_power_latch_hold()` first `:227` |
| Hardware | `power.rs` | deep sleep + wake-up reasons; GPIO39/RTC-capability rationale `:21-32`; deep sleep MUST NOT inherit light-sleep GPIO wakeup arming `:74`, cleared via `disable_light_sleep_gpio_wakeup()` `:91` |
| Hardware | `rtc.rs` | PCF8563 + single hardware alarm register (doc `:108-114`; `set_alarm` `:114`, `clear_alarm` `:100`) |
| Hardware | `wake.rs` | ISR disable-pin-first `:33-40` |
| Hardware | `button.rs` / `watchdog.rs` / `audio.rs` / `nfc.rs` | debounce; WDT; ES8311; GT23SC6699 |
| Background task | `epd_task.rs` | refresh owner + recovery; notify channel must stay wired `:307-311` (wakeups `:200,225`) |
| Background task | `sync_task.rs` | HTTPS owner; `EspNvs` Send-not-Sync `:9-12`; NEVER touches I2C; main loop owns RTC reprogram + NTP align |
| Rendering | `canvas.rs` / `font8x16.rs` / `font5x7.rs` / `font_cjk.rs` / `home.rs` / `icons.rs` | layout, EPD-FFI-free, PC-previewable via `tools/preview` `#[path]`. **Caveat:** `home.rs` imports `crate::board::ChargeSnapshot`, so the render group is not dependency-free of hardware; `tools/preview` carries a hand-maintained copy of that type — keep the two in step |
| Rendering | `display.rs` | EPD FFI wrapper + `render_home` delegation (`refresh_full` `:99`, `refresh_partial` `:112`) |
| UI | `ui.rs` / `screens.rs` | shared header/row chrome (`ui.rs` 54 LOC: `header`, `draw_rows`); `screens.rs` 883 LOC, nav drawer/calendar/alarms/todos/settings. No `UiResult`/`AlarmInterrupted`/`pick_from_list` remain — those were replaced by the state-machine navigation |
| Services | `alarms.rs` / `todos.rs` / `inbox.rs` / `storage.rs` / `reminders.rs` / `nvs_blob.rs` | NVS stores; alarms owns ringing/ack + RTC scheduling; reminders owns full-screen alerts |
| Protocol | `control.rs` / `usb_console.rs` / `ble_control.rs` / `sync.rs` / `wifi.rs` | NimBLE callback thread MUST NOT touch `Note4Board`; `BLEDevice::init()` `:893` after `deinit_full()` `:913`; multi-connect OK since scan removed 2026-08-17; `esp_wifi_stop()` forbidden (rationale `:37-45`, `:255`); legacy `restart_for_fresh_wifi_session` dead `:343` |

## FFI COMPONENT: components/zectrix_epd/
Official SSD2683 C++ driver (CMakeLists.txt + `zectrix_epd.cc` + `include/zectrix_epd.h` + `private_include/ssd2683_waveform.h`). Waveforms must not be deleted. After modifying, `cargo clean -p esp-idf-sys`. EPD power/timing only through `zectrix_epd_power_on/off` FFI. **Pins GPIO6/8/9/10/11/12/13 are claimed only inside `zectrix_epd.cc`** — a Rust-only pin audit has a 7-pin blind spot here.

## CONVENTIONS (crate-level)
- `anyhow::Result`; all peripheral init in `Note4Board::take()`; `Peripherals::steal()` only allowed at `board.rs:416` (battery ADC channel rebuild) + `wifi.rs:94` (lazy one-per-process `EspWifi` modem).
- `main.rs` mod decls alphabetical.
- Glyph tables `#[rustfmt::skip]`.
- `build.rs` injects `BUILD_EPOCH_SECS` (RTC fallback) + `GIT_REV`.
- rustflags include `--cfg espidf_time64`; `harness=false`.

## ANTI-PATTERNS (code-enforced)
- **Wi-Fi** (`wifi.rs`): multi-connect OK since scan removed 2026-08-17; `esp_wifi_stop()` forbidden (rationale `:37-45`; the retaining comment `:255`); `esp_restart()` strictly forbidden — restart = `power::enter_deep_sleep_with_wakeups` + ~100ms timer wake only; legacy `restart_for_fresh_wifi_session` dead `:343`.
- **Sync scheduler** (`ctx.rs`): BLE pairing USB-only polling; BLE+Wi-Fi share radio, never simultaneous.
- **BLE** (`ble_control.rs`): callback thread MUST NOT touch `Note4Board`; `BLEDevice::init()` `:893` after `deinit_full()` `:913`.
- **RTC** (`rtc.rs:108-114` + `alarms::next_due`): single slot → soonest always written; boot `clear_alarm()` residue.
- **Power sequencing**: GPIO42 AVDD high before I2C0 init (`board.rs:237` then `:270`); `release_power_latch_hold()` first on wake (`board.rs:227`).
- **EPD**: 1bpp full refresh required once after 4bpp full before partial resumes; power/timing only via `zectrix_epd_*` FFI.
- **Audio trap**: `Effect::StopTone` MUST NOT `set_mute(true)` (`app_runner.rs:260`; the only `set_mute(false)` is the audio task's own bring-up, `audio_task.rs:73`).
- **Minute ticks**: MUST NOT full-refresh — a Home minute change plans a clock-region partial (`logic/src/render_plan.rs:235`; the decision moved out of `app_runner.rs` into the host-testable planner).
- **Id non-collision** within list (sync-api contract); USB commands polled across Home/content/nav/number entry/inbox detail; only time-critical screens defer.
- **Remote-triggered state changes** (`main.rs:921-1000`): capture the command variant with `matches!` before dispatch (`Command` is `Clone`, not `Copy`) and act only on `Reply::Ok`. The former `dirty.push(FULL_SCREEN_RECT)` poll-side push is retired — the state machine owns its renders and the render registry matches completions (`main.rs:638-640`).
- **EPD supersede contract** (`main.rs:1054-1062`): a refresh superseded before the panel ran MUST NOT be reported `RenderDone`; its replacement completes on its own request id.
- **BootSnapshot AF/AIE residue**: AF/Tick events are interpreted only when the snapshot's core facts were sound — on a default `AppState` a real alarm would read as residue and be ACKed off (`main.rs:498-505`; gate consulted at `main.rs:1252`).
- **Light-sleep config failure** MUST NOT abort: `EnterLightSleep` returns a typed `EffectCategory::Sleep` error for the state machine to absorb (`app_runner.rs:464-468`).
- **EPD notify channel** must stay wired (`epd_task.rs:307-311`; wakeups `try_send` at `:200,225`).
- **GetStatus polls are user activity** — no deep sleep mid-session (`main.rs:1178-1183`).
- **Worker stacks**: every worker declares `.stack_size(...)` explicitly — `usb_console.rs` reader 12 KiB / writer 8 KiB (constants at the top of the file) after both originally inherited the 4096-byte pthread default. The reader runs `control::parse_command`, whose recursion follows frame nesting, so an explicit size is part of the fix rather than decoration.
- **Command nesting is capped before parsing**: `control::parse_command` rejects any frame nesting deeper than `inkwash_logic::protocol::MAX_COMMAND_NESTING` (4) *before* deserializing. Deserialization recurses once per container level, so peak stack follows the frame's shape rather than its length: measured against the pinned `serde_json`, 16 levels inside a 69-byte frame costs ~6.9 KB, while the USB reader has 4096 bytes and the NimBLE `on_write` callback runs on a 5120-byte host task. Every command in this protocol is a flat object, so the cap rejects no valid frame. The scan lives in `logic/src/protocol.rs` and is host-tested (7 tests); deleting the cap re-opens the overflow.
- **`CONFIG_SPIRAM_ALLOW_STACK_EXTERNAL_MEMORY` does NOT exist in the pinned IDF v5.5.5.** It is gone; IDF 5.2.3 defined it with default `n` and scoped it to `xTaskCreateStatic`, never to pthread stacks, and 5.5.5's pthread default caps are already `MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT`. `tasks::spawn_internal_stack`'s explicit caps are therefore belt-and-braces — kept to keep the internal-RAM requirement visible at each spawn, not because they mitigate a live hazard. See the module doc in `tasks.rs`.

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
- CI builds only the `logic/` crate; nothing formats, lints, or size-checks this crate. Assume no automated gate here.
