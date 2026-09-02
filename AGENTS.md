# PROJECT KNOWLEDGE BASE

**Generated:** 2026-09-02 · **Updated:** 2026-09-02 · **Commit:** e9224a1 (working tree dirty) · **Branch:** feature/power-and-response

## OVERVIEW
Firmware for ZECTRIX NOTE4 b/w (ESP32-S3-WROOM-1 N16R8, 4.2" 400×300 SSD2683 EPD). One of four independent repos; siblings out-of-scope. Two Rust crates (`rust-firmware/` + `logic/`), no root Cargo.toml, no workspace.

## STRUCTURE
```
inkwash-firmware/
├── docs/            # dev guide (must read) + protocol contracts
├── logic/           # inkwash-logic: host-testable pure logic (the only automated test suite, CI-tested)
├── rust-firmware/   # inkwash-note4: 33 flat src modules + C++ EPD FFI
├── scripts/         # Build/flash/provisioning (.sh Linux, .ps1 Windows twin)
├── tools/           # inkwash-preview renderer + CJK font generator
├── vendor/          # vendored esp-idf-hal 0.46.2 + sdmmc patch (third-party, read-only)
├── CHANGELOG.md
├── README.md
├── LICENSE          # Apache-2.0
├── AGENTS.md
└── .github/workflows/ci.yml   # ONE job `logic` (rust-firmware not built in CI)
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Changing firmware behavior | `rust-firmware/src/` | Module map in `rust-firmware/AGENTS.md` |
| Pure-logic host-testable lib | `logic/` | `logic/AGENTS.md` owns the module map |
| Env/build/flash/troubleshooting | `docs/development-guide.md` | Must read; includes "Safety matters" |
| GPIO/power/EPD board ref | `docs/development-guide.md` §3/§9 | Board-level hardware reference merged into dev guide |
| On-device smoke-test gate | `docs/development-guide.md` §13 | After sync/wifi/alarm/reminder/control edits |
| USB/BLE control contract | `docs/control-protocol.md` | Contract with inkwash-desktop |
| HTTPS sync contract | `docs/sync-api.md` | Contract with inkwash-server |
| Wi-Fi crash history | `rust-firmware/src/wifi.rs` doc comments | Root-cause analysis anchored there |

## CODE MAP
Highest-signal symbols only; per-module role map lives in `rust-firmware/AGENTS.md`.

| Symbol | Location | Role |
|--------|----------|------|
| `main()` | `rust-firmware/src/main.rs:114` | Sole device entry; boot + `dispatch_app_runner` at ~:1135 |
| `AppRunner` + `EffectRunner` | `logic/src/app.rs` | Firmware-driven host-testable state machine; `update()` MUST NOT do IO (`app.rs:17-19`) |
| `Note4Board::take()` | `rust-firmware/src/board.rs` | Central hardware assembly (RTC/EPD/buttons/LED/audio/NFC/ADC) |
| `WifiManager` | `rust-firmware/src/wifi.rs` | One-per-process Wi-Fi singleton; crash-history anchor |
| `DeviceContext` | `rust-firmware/src/ctx.rs:679` LOC | Shared wall-clock schedulers (`SyncScheduler` + `AlarmScheduler`) for Home/blocking UI loops |
| `control::dispatch` | `rust-firmware/src/control.rs` | Shared USB/BLE command dispatch, main-loop only |
| EPD task | `rust-firmware/src/epd_task.rs` | Owns refresh + recovery; `submit_*` channel must stay wired (`epd_task.rs:215-219`) |
| Sync task | `rust-firmware/src/sync_task.rs` | Never touches I2C (`sync_task.rs:9-12`) |
| Logic test harness | `logic/src/harness.rs` | `FakeExecutor`/`FakeHostAdapter`/`ScriptedFailures` drive the same `AppRunner` the firmware drives |

## CONVENTIONS
- Docs mixed-language: dev guide + contracts English; `device-verification-*.md` Chinese. Commits conventional English lowercase (`feat:`/`fix:`/`docs:`/`chore:`/`build:`).
- `rust-firmware` has no tests; `logic/` owns the entire test suite (121 `#[test]` markers across 10 modules, host). Pre-commit = `cargo +esp fmt --check` + clippy (zero warnings) + release build + on-device verification.
- Toolchain pinned `esp` channel; formatting MUST use `cargo +esp fmt`. SDK = ESP-IDF v5.5.5.
- Size-first: release `opt-level="s"`, dev `"z"`; `build-std=["std","panic_abort"]`.
- `sdkconfig.defaults` is the only IDF config committed; real `sdkconfig*` are gitignored `target/` artifacts.

## ANTI-PATTERNS (THIS PROJECT)
Red lines (violation = bricked device or guaranteed crash). Anchors in `rust-firmware/AGENTS.md`.
1. **NOTE4 ≠ NOTE4C** — no cross-flash; restore only from this unit's own backup.
2. **DIO flash only** — QIO watchdog-loops pre-app.
3. **`esp_wifi_stop()` forbidden; `esp_restart()` strictly forbidden.** Multi-connect in one boot IS allowed since the scan culprit was removed on 2026-08-17. To restart: only `power::enter_deep_sleep_with_wakeups` + ~100ms timer wake. See `rust-firmware/src/wifi.rs` doc comments.
4. **GPIO17 power latch** — high early in boot + RTC GPIO hold during deep sleep.
5. `ssd2683_waveform.h` + `zectrix_epd` component **must not be deleted or replaced**; no generic SSD1683 sequences.
6. Never commit: `sdkconfig` (real one), `backups/*.bin`, `target/`, device-credential logs.

## UNIQUE STYLES
- `vendor/esp-idf-hal` = 0.46.2 + `sdmmc_host_t` field patch (IDF 5.5.5). Read-only; drop once upstream supports 5.5.5.
- C++ EPD FFI via `extra_components` + bindgen (`zectrix_epd`). After modifying: `cargo clean -p esp-idf-sys`.
- `build.rs` injects `BUILD_EPOCH_SECS` (RTC fallback) + `GIT_REV`.
- `partitions.csv` referenced at fixed 6-level relative depth — do not move the file.
- `rust-firmware` = single bin (`harness=false`) bin crate; **NOT a workspace member**.
- `logic/` = path-dep host lib; zero ESP-IDF deps.
- `tools/preview` `#[path]`-includes real render modules (canvas/font8x16/font_cjk/font5x7/icons/home) so PC preview never drifts from device render.

## COMMANDS
```bash
./scripts/build-rust.sh --release        # builds (sources IDF env); no args = debug
cargo +esp fmt --manifest-path rust-firmware/Cargo.toml -- --check   # pre-commit
./scripts/build-rust.sh && (cd rust-firmware && cargo clippy)        # clippy (needs IDF env)
espflash flash --port /dev/tty.usbmodem1101 --chip esp32s3 --flash-size 16mb \
  --flash-mode dio --flash-freq 80mhz --partition-table rust-firmware/partitions.csv \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4  # macOS; Linux = /dev/ttyACM0
espflash monitor --port /dev/tty.usbmodem1101
inkwash-desktop --status /dev/tty.usbmodem1101    # verify firmware over USB control protocol
espflash board-info --port /dev/tty.usbmodem1101  # confirm chip reachable when tools miss device
```

## RELEASES
Built **locally** (CI impractical with full ESP-IDF) via `scripts/release.sh <tag>` (e.g. `./scripts/release.sh v0.3.0`). It builds release ELF, tags (only if tag absent), pushes to `origin` + `github`, then creates a **non-draft** GitHub Release on `counhopig/inkwash-firmware` via `gh`.

- **Critical:** `release.sh` builds the local working tree but tags whatever commit is checked out, and **reuses an existing tag without moving it**. Ensure `git status` is clean and the intended commit is checked out — otherwise the binary and tag can disagree. To re-release:
  ```bash
  gh release delete v0.1.0 --repo counhopig/inkwash-firmware --yes
  git push origin  :refs/tags/v0.1.0
  git push github  :refs/tags/v0.1.0
  git tag -f v0.1.0 <intended-commit>
  git push origin v0.1.0 && git push github v0.1.0
  ./scripts/release.sh v0.1.0
  ```
- Release check: `gh release view v0.3.0 --repo counhopig/inkwash-firmware --json isDraft,assets` (expect `isDraft:false` + `inkwash-note4` asset).

## NOTES
- `rust-firmware/.cargo/config.toml` keeps no machine-specific paths: `IDF_PATH` / `LIBCLANG_PATH` are resolved dynamically by `scripts/build-rust.sh` / `build-rust.ps1` (honoring `$IDF_PATH`/`$LIBCLANG_PATH` when set, then probing conventional install locations; newest match wins). A bare `cargo build` still requires a sourced ESP-IDF environment — always use the scripts.
- **rust-analyzer availability depends on two per-machine pieces**: ① `esp-ra` toolchain (`rustup toolchain link esp-ra ~/esp/esp-ra`): `bin/cargo` is a wrapper (`--version` reports 1.96.0, forwarding to esp cargo), `rustc`/`rustdoc` are symlinks to the esp toolchain — must be rebuilt after an espup reinstall; ② the ESP-IDF environment must come from the parent process — launch the editor from a shell where `source ~/esp/esp-idf/export.sh` has run (or set `IDF_PATH` globally); tracked `.vscode/settings.json` injects only `RUSTUP_TOOLCHAIN=esp-ra` and no absolute paths. Root cause: rust-analyzer 0.3.3016 classifies esp cargo's `1.95.0-nightly` as <1.95.0, falls back to the removed `--lockfile-path` arg, which makes `cargo metadata` degrade to `--no-deps` (spurious unresolved imports); the wrapper guides it to the `-Zlockfile-path` branch (supported by esp cargo). Once upstream rustup ships rustc ≥1.96.0-nightly, the hack can be removed.
- Working tree dirty on `feature/power-and-response`: `rust-firmware/src/main.rs` (uncommitted edits in flight).
- Not yet verified on device: full alarm-ringing flow end-to-end; BLE end-to-end pairing.
