# PROJECT KNOWLEDGE BASE

**Generated:** 2026-08-16 · **Updated:** 2026-08-27 · **Commit:** 84eadad · **Branch:** main

## OVERVIEW
Self-developed firmware repository for the ZECTRIX NOTE4 black-and-white display edition (ESP32-S3-WROOM-1 N16R8, 4.2" 400×300 SSD2683 EPD). A single Rust crate (`rust-firmware/`) built with ESP-IDF 5.5.5 and the `esp` Xtensa toolchain, implementing calendar/offline alarms/todos + HTTPS sync + USB/BLE configuration channels. One of a four-repository system (`../inkwash-desktop` PC tool, `../inkwash-server` backend, `../inkwash-mcp` MCP server, each an independent repository). Design principle: the device does not author content — the configuration channel only delivers Wi-Fi credentials/server address + token, content is pulled as structured JSON, and alarms ring offline.

## STRUCTURE
```
inkwash-firmware/
├── docs/            # All documentation: development guide (must read; includes board reference + hardware smoke-test checklist) and two cross-repo protocol contracts.
├── logic/           # inkwash-logic: host-testable pure logic (the only automated test suite, CI-tested)
├── rust-firmware/   # inkwash-note4 crate (28 flat src modules ~7.3k LOC + C++ EPD component)
├── scripts/         # Build/flash/provisioning scripts (.sh=Linux, .ps1=Windows twin)
├── tools/           # e-paper UI preview renderer + CJK font generator
├── vendor/          # vendored esp-idf-hal 0.46.2 + sdmmc patch (third-party, read-only, see UNIQUE STYLES)
├── CHANGELOG.md     # release history
└── LICENSE          # Apache-2.0 (font/upstream licenses — see the README footer)
```
No root Cargo.toml, no workspace; the firmware crate itself has no CI (needs the ESP-IDF toolchain) — `logic/` is CI-tested via `.github/workflows/ci.yml`. `.omo/` and `.claude/` are tool directories, not project content.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Changing any firmware behavior | `rust-firmware/src/` | Module responsibility table in `rust-firmware/AGENTS.md` |
| Environment/build/flash/troubleshooting | `docs/development-guide.md` | Must read; includes a "Safety matters (non-negotiable)" section |
| GPIO/power rails/EPD data format | `docs/development-guide.md` §3/§9 | Board-level hardware reference merged into the dev guide |
| On-device smoke-test checklist | `docs/development-guide.md` §13 | Run after touching sync/wifi/alarm/reminder/control code |
| USB/BLE command protocol | `docs/control-protocol.md` | Contract with inkwash-desktop |
| HTTP sync protocol | `docs/sync-api.md` | Contract with inkwash-server |
| Wi-Fi connection history and scan culprit investigation | `rust-firmware/src/wifi.rs` (`WifiManager` doc comments) | Complete reasoning chain and root cause of the second-connection crash (resolved); code-level summary in `rust-firmware/AGENTS.md` |

## CODE MAP
No codegraph tooling; rust-analyzer has been available since 2026-08-18 (`esp-ra` toolchain + IDF env injection, see NOTES) — the following is static analysis; reference centrality is not measured.

| Symbol | Type | Location | Role |
|--------|------|----------|------|
| `main()` | fn | `rust-firmware/src/main.rs` | Single entry point: boot + 20ms polling main loop, wires up all 28 `mod` modules |
| `Note4Board::take()` | fn | `src/board.rs` | Central hardware assembly: RTC/EPD/buttons/LED/audio/NFC/ADC, shared I2C0 |
| `WifiManager` | struct | `src/wifi.rs` | Wi-Fi singleton — carrier of the most critical constraint in the repo (see ANTI-PATTERNS #3) |
| `control::dispatch` | fn | `src/control.rs` | Shared USB/BLE command dispatch, called only from the main loop context |
| `sync::sync_now` | fn | `src/sync.rs` | HTTPS sync (bidirectional POST, dirty-set upload, urgent poll; ETag kept for legacy GET) |
| `AlarmStore` / `TodoStore` | struct | `src/alarms.rs` / `src/todos.rs` | NVS persistence; the alarm store picks the soonest one and writes it to the PCF8563's single hardware register |
| Two shared singletons | — | `main.rs` (boot sequence) | One NVS partition handle + one WifiManager, each created once per process |

## CONVENTIONS
- Docs are written in Chinese; commits use conventional format and are described in English (`feat:`/`fix:`/`docs:` etc., lowercase start).
- **The `rust-firmware` bin crate has no tests** (`harness = false`); pre-commit checks = fmt + clippy with zero warnings + release build + manual on-device verification (development-guide §12–§13). The host-testable logic lives in the sibling `logic/` crate (`inkwash-logic`, path dependency, zero ESP-IDF deps) — CI-tested on push/PR via `.github/workflows/ci.yml` (`cargo test --locked` + rustfmt + clippy), and locally with `cd logic && cargo test` (currently 49 tests; add new pure date/schedule/validation logic there, not in `rust-firmware/src`).
- Toolchain pinned to the `esp` channel (`rust-toolchain.toml`); formatting must use `cargo +esp fmt`, stable/nightly are disallowed.
- Size-first: release `opt-level="s"`, dev `"z"`; `build-std=["std","panic_abort"]`.
- `cargo run` = flash + monitor (runner=espflash); a bare `cargo build` always fails in a shell that has not sourced the ESP-IDF environment — always use `scripts/build-rust.sh`.

## ANTI-PATTERNS (THIS PROJECT)
Red lines (violation = bricked device or guaranteed crash; code anchors in `rust-firmware/AGENTS.md`):
1. **NOTE4 and NOTE4C must not be cross-flashed** — hardware/waveforms/firmware are incompatible; restore only from this unit's own backup.
2. **Flash is always DIO, QIO is forbidden** — a QIO image watchdog-reset-loops before the app starts.
3. **Wi-Fi can connect multiple times within the same boot** — it was previously believed that only one successful Wi-Fi connection was allowed per boot (a second connection would always crash); on 2026-08-17 the culprit was identified as the blocking `esp_wifi_scan_start()` run before every connection (credentials come from desktop `SetWifi`, the scan is pure redundancy), and after removing it, multiple consecutive syncs within the same boot were all verified to succeed. **Never call `esp_wifi_stop()`** (stop→start is the crash trigger point; `start()` runs once per process); to restart, only the deep-sleep + ~100ms timer-wake path is allowed, and **`esp_restart()` is strictly forbidden**. See the doc comments in `rust-firmware/src/wifi.rs`.
4. **GPIO17 power latch**: pulled high early in boot + RTC GPIO hold during deep sleep — otherwise the device physically powers off.
5. `ssd2683_waveform.h` and the official zectrix_epd component **must not be deleted or replaced**; do not use generic SSD1683 sequences.
6. Never commit: `sdkconfig`, `backups/*.bin`, `target/`, logs containing device credentials.

## UNIQUE STYLES
- `vendor/esp-idf-hal` = crates.io 0.46.2 + `sdmmc_host_t` field patch (adapted for IDF 5.5.5), wired in via `[patch.crates-io]`. **Read-only**; syncing upstream must not drop the patched field; once upstream supports 5.5.5, delete the whole directory along with the patch section (`vendor/README.md`).
- The C++ EPD driver is integrated into the crate via `extra_components` + bindgen (FFI module `zectrix_epd`); after modifying it, you must run `cargo clean -p esp-idf-sys`.
- `build.rs` injects `BUILD_EPOCH_SECS` as a fallback time source after RTC power loss.
- `partitions.csv` is referenced via `${CMAKE_CURRENT_SOURCE_DIR}/../../../../../../` at a fixed 6-level relative depth — do not change the hierarchy.

## COMMANDS
```bash
./scripts/build-rust.sh --release        # build (the script sources the ESP-IDF environment itself; no args = debug)
cargo +esp fmt --manifest-path rust-firmware/Cargo.toml -- --check   # pre-commit check
./scripts/build-rust.sh && cd rust-firmware && cargo clippy   # clippy with zero warnings (needs IDF env; can reuse build-rust.sh's exports)
espflash flash --port /dev/tty.usbmodem1101 --chip esp32s3 --flash-size 16mb \
  --flash-mode dio --flash-freq 80mhz --partition-table rust-firmware/partitions.csv \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4  # flash (macOS port name; Linux is /dev/ttyACM0)
espflash monitor --port /dev/tty.usbmodem1101     # serial logs (needs a real TTY)
inkwash-desktop --status /dev/tty.usbmodem1101    # verify the running firmware over the USB control protocol (get_status; echoes boot log lines)
espflash board-info --port /dev/tty.usbmodem1101  # confirm the chip is reachable when the node exists but tools report the device missing
```

## RELEASES
Firmware releases are built **locally** — a full ESP-IDF toolchain is impractical on CI — and published with `scripts/release.sh <tag>` (e.g. `./scripts/release.sh v0.3.0`). It builds the release ELF, tags (only if the tag doesn't already exist), pushes the tag to `origin` + `github`, then creates a **published** (non-draft) GitHub Release on `counhopig/inkwash-firmware` via `gh`.

- **Critical:** `release.sh` builds the **local working tree** but tags whatever commit is checked out, and it **reuses an existing tag** without moving it. So before releasing, make sure `git status` is clean and the intended commit is checked out — otherwise the binary and the tag can disagree (e.g. re-running `release.sh v0.3.0` after moving that tag attaches the new build to the old tag's commit). To re-release a version that already has a tag, delete the old tag + release first:
  ```bash
  gh release delete v0.1.0 --repo counhopig/inkwash-firmware --yes
  git push origin  :refs/tags/v0.1.0
  git push github  :refs/tags/v0.1.0
  git tag -f v0.1.0 <intended-commit>
  git push origin v0.1.0 && git push github v0.1.0
  ./scripts/release.sh v0.1.0
  ```
- Release check: `gh release view v0.3.0 --repo counhopig/inkwash-firmware --json isDraft,assets` (expect `isDraft: false` and the `inkwash-note4` firmware asset).

## NOTES
- `rust-firmware/.cargo/config.toml` keeps no machine-specific paths: `IDF_PATH`
  and `LIBCLANG_PATH` are resolved dynamically by `scripts/build-rust.sh` /
  `build-rust.ps1` (honoring `$IDF_PATH`/`$LIBCLANG_PATH` when set, then probing
  the conventional install locations; newest match wins). A bare `cargo build`
  still requires a sourced ESP-IDF environment — always use the scripts.
- **rust-analyzer availability depends on two per-machine pieces**: ① the
  `esp-ra` toolchain (`rustup toolchain link esp-ra ~/esp/esp-ra`): `bin/cargo`
  is a wrapper (`--version` reports 1.96.0, everything else is forwarded to the
  esp cargo), `rustc`/`rustdoc` are symlinks pointing at the esp toolchain —
  must be rebuilt after an espup reinstall; ② the ESP-IDF environment must come
  from the parent process — launch the editor from a shell where
  `source ~/esp/esp-idf/export.sh` has run (or set `IDF_PATH` globally); the
  tracked `.vscode/settings.json` injects only `RUSTUP_TOOLCHAIN=esp-ra` and no
  absolute paths. Root cause: rust-analyzer 0.3.3016 classifies the esp cargo's
  `1.95.0-nightly` as <1.95.0, falls back to the removed `--lockfile-path`
  argument, which makes `cargo metadata` degrade to `--no-deps` (spurious
  unresolved imports in the editor); the wrapper guides it to the
  `-Zlockfile-path` branch (supported by the esp cargo). Once upstream rustup
  ships rustc ≥1.96.0-nightly, the whole hack can be removed.
- The device supports periodic auto-sync (default 60 minutes, configurable to 1/5/10/30/60 in the settings menu); failures are retried on the next cycle; `esp_wifi_stop()` and `esp_restart()` remain red lines, see red line #3.
- Not yet verified on device: the full alarm-ringing flow and BLE end-to-end pairing — changes to related code cannot be vouched for by "it compiles".
