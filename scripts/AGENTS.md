# Firmware tooling

## OVERVIEW

Grouped build, device, verification, font generation and release entry points.

## WHERE TO LOOK

| Task | Location | Notes |
| --- | --- | --- |
| POSIX build | `build/build-cpp.sh` | Exports IDF; checks version and LVGL availability |
| Windows build | `build/build-cpp.ps1` | Discovers IDF/tools; initializes missing LVGL |
| Firmware image gate | `checks/check-boot-ledger.sh` | Converts ELF; checks RTC noinit exclusion |
| Embedded revision | `checks/check-git-rev.sh` | Matches current Git description against ELF strings |
| USB stress and soak | `checks/smoke-note4.py` | Commands, reply deadlines and stack threshold |
| Continuous serial capture | `device/capture-serial.py` | Timestamped logs; reconnects after re-enumeration |
| Wi-Fi configuration | `device/set-wifi.py` | Waits for final response after pending replies |
| Full flash backup | `device/backup-flash.ps1` | Image plus SHA256 JSON manifest |
| Font generation | `assets/generate_cjk_font.py` | Explicit font and output directory arguments |
| Publication | `release/release.sh` | Build/test/image/config gates and GitHub archive |

## CONVENTIONS

- Scripts that locate the checkout derive its root two levels above their group directory.
- POSIX build accepts extra CMake arguments; PowerShell exposes `-BuildDir`.
- POSIX builder rejects missing LVGL; Windows builder initializes the submodule.
- USB command tools use pyserial and the `>>IW` / `<<IW` JSON console framing.
- Wi-Fi password input defaults to an interactive prompt; final replies match request ID `wifi`.
- Capture output paths are caller-supplied; backup output defaults relative to the caller's working directory.
- Release uses HEAD commit time as `SOURCE_DATE_EPOCH`, compares generated partition bytes, and packages build artifacts.

## ANTI-PATTERNS

- Do not bypass clean-tree or existing-tag/release checks (`release/release.sh:10`, `release/release.sh:17`).
- Do not accept an unparsable image segment table as a passing ledger gate (`checks/check-boot-ledger.sh:103`).
- Do not treat a `pending` Wi-Fi reply as completion (`device/set-wifi.py:65`).
- Do not report backup success before checking image size (`device/backup-flash.ps1:47`).

## NOTES

- Release publication targets `counhopig/inkwash-firmware` and publishes immediately after draft creation.
- Release config checks read `firmware/sdkconfig`; custom SDKCONFIG paths are not inferred.
- Ledger gate `--self-test` checks overlapping, non-overlapping and malformed segment tables.
- Capture `--expect` accepts repeated regexes; missing matches produce failure status.
- Capture auto-detection searches POSIX modem/ACM nodes; Windows capture needs `--port COMx`.
- Smoke port defaults to a local macOS device name; use explicit `--port` on other setups.
