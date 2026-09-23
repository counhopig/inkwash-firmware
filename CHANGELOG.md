# Changelog

All notable changes to **inkwash-firmware** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

## [Unreleased]

### Added
- **Single event loop** — the application state machine in `inkwash-logic`
  (events in, effect batches out) drives the firmware. Batches travel through
  bounded queues, a failing batch no longer discards the rest of its event,
  queued batches count toward the sleep admission gates, and a saturated queue
  is counted instead of silently dropping work.
- **Boot-failure guard and minimum safe mode** — a ledger in RTC noinit memory
  counts consecutive attributable resets (panic, task or interrupt watchdog,
  brownout, power glitch, CPU lockup, unexpected software reset). Three in a
  row park the device in a minimum safe mode that keeps USB diagnostics and all
  stored data intact, and both the panel and the boot log state which recovery
  that mode needs. Power-on, the reset pin, a USB reset, a deep-sleep wake and
  any power cycle start a fresh count; store read faults are classified
  (corrupt / unsupported version / I/O) and never degraded to a default.
- **Runtime diagnostics** — saturating counters for conditions that are handled
  quietly (saturation of each queue and mailbox, BLE reply retries and drops,
  EPD partial-to-full fallback), logged as deltas.
- **Build profiles** — `--diagnostic` (comprehensive heap poisoning plus INFO
  logging) and `scripts/build-secure.sh` (Secure Boot V2, AES-256 flash
  encryption in release mode, NVS encryption, JTAG and basic-ROM download
  disabled), each building into its own target directory.
- **Device tooling** — `scripts/smoke-note4.py` (soak and command stress over
  the USB protocol), `scripts/capture-serial.py` (reconnecting capture with
  required-pattern assertions), `scripts/check-git-rev.sh` (fails a build whose
  embedded revision is stale), `scripts/check-boot-ledger.sh` (fails when the
  flashed image would cover the boot ledger) and `scripts/backup-flash.ps1`
  (identity-checked 16 MB dump).
- **About screen** — firmware version and embedded git revision, reached from
  Settings.
- **P0-6 first-exception recorder** — extended to both cores, with a host-side
  decoder in `tools/p06_accept.py`; it stays out of the default image behind
  the `p06_diag` / `p06_validate` / `p06_validate_core1` features.

### Changed
- **Storage and partitions** — the partition table now carries a 64 KiB NVS,
  an encrypted `nvs_keys` partition, a 512 KiB coredump partition for panic
  capture, and a single 4 MiB `factory` application partition. Grouped state
  updates are written atomically, an interrupted sync apply is recovered from a
  journal, and stored settings are validated on read against the ranges the
  device itself writes rather than clamped into range.
- **BLE** — pairing requires authentication; the worker is created on demand
  instead of at boot, connection handles are reused safely, and replies are
  retried through bounded mailboxes.
- **Control protocol** — command nesting is bounded before parsing, and the
  realtime-clock reply channel is reused under load instead of being dropped.
- **Scheduling** — task priorities and stack allocation policy are defined in
  one place, and the watchdog covers the blocking worker windows.
- **Sync** — cron boundaries re-align after the wall clock moves backwards
  (`set_rtc`, the daily NTP alignment, a PCF8563 reseed) instead of suspending
  automatic sync until the clock passes the stale cursor.
- **Settings** — the manual sleep entry is gone; sleeping is decided by the
  power state machine alone.
- **Build** — ESP-IDF discovery reuses an active environment and prefers the
  tools of the selected IDF, native component sources are tracked for rebuilds,
  and the embedded CJK font blobs are validated at build time. The release
  script verifies the target artifacts and the partition table against
  espflash's own output, and runs the boot-ledger gate, before it publishes.
- **Source comments** — the accumulated comment layer was stripped from both
  crates; comments now exist only where a non-obvious constraint needs
  recording.

### Removed
- The `docs/` set (architecture map, development guide, control protocol, sync
  API, hardware assessment, verification, review notes). The control protocol
  is specified by `logic/src/protocol.rs` plus the framing in
  `rust-firmware/src/usb_console.rs` / `ble_control.rs`, and the sync contract
  by `logic/src/sync_validate.rs` and its tests.
- The obsolete ETag sync state.

### Known limitations
- Input stays poll-based: there is no dedicated button task or bounded event
  queue, and `wake.rs` only wakes the main loop rather than emitting semantic
  key events.
- Not yet verified on device: idle power and response latency measurements, the
  full alarm-ringing flow, and BLE end-to-end pairing.

## [0.6.0] - 2026-09-08

### Added
- **Power/response overhaul** (EPD refresh, Wi-Fi/HTTPS sync, and sleep
  tiering):
  - **Light sleep**: `CONFIG_PM_ENABLE` + tickless idle with a 200-tick
    admission threshold; a two-level poll cadence (20 ms active / 1 s idle)
    with a 10 s idle RTC re-read; digital-GPIO wake for the three nav keys;
    and a `LightSleepBlock` no-light-sleep lock while BLE pairing is open.
  - **EPD refresh task**: the `zectrix_epd` FFI driver now lives in
    `epd_task.rs`, which owns it exclusively. Refresh requests carry an
    immutable full-frame snapshot through a bounded single-slot latest-wins
    path; completion events (`ok` / `recovered`) report success, failure, and
    partial->full recovery back to the app; and `display.rs` centralizes the
    "8 partials then a full refresh" promotion policy.
  - **Sync task**: `sync_task.rs` owns the process's single
    `WifiManager` and runs every Wi-Fi operation (SyncNow/SetWifi/UrgentPoll)
    off the main loop over command/reply channels, with independent NVS
    handles per thread. Deferred replies use a new `pending` status, `busy`
    keeps "not executed, safe to retry", and a home-content fingerprint plus
    an urgent dedup flag skip unchanged redraws / repeated full syncs.
  - **Deep sleep tier**: 5-minute idle drops to deep sleep with ext1
    wake on GPIO0/5/18 (ENTER/RTC_INT/DOWN); a maintenance-timer wake takes
    the minimum of the next month-boundary alarm and a 10-minute fallback;
    a deep-sleep wake skips the boot full refresh.
  - **One-shot wake interrupts** (`wake.rs`): keys and the RTC alarm resume
    the main loop's 1 s idle wait immediately via a task notification.

### Changed
- Tightened the task-watchdog timeout from 20 s to 10 s now that the sync
  and EPD blocking windows live on their own watched tasks.

### Known limitations
- Input stays poll-based: there is no dedicated button
  task or bounded event queue, and `wake.rs` only wakes the main loop rather
  than emitting semantic key events.
- Not yet verified on device: idle power / response latency measurements, the
  full alarm-ringing flow, and BLE end-to-end pairing still lack reproducible
  hardware evidence.

## [0.5.0] - 2026-08-27

### Changed
- **Version aligned to 0.5.0** across the four Inkwash repositories.

### Added
- **CI workflow for `inkwash-logic`** (host tests, rustfmt, clippy) on
  push/PR - the firmware crate itself still needs the ESP-IDF toolchain,
  so only the host-testable logic layer runs in CI.

## [0.4.0] - 2026-08-21

### Changed
- **Rebranded to Inkwash** - display wordmark, USB/BLE protocol framing
  (`>>IP ` / `<<IP ` -> `>>IW ` / `<<IW `), the urgent-poll header
  (`x-inkpaper-poll` -> `x-inkwash-poll`), crate/binary name
  (`inkwash-note4`), and NVS namespaces (device data resets on first
  boot - re-provision Wi-Fi/server via the desktop tool).

## [0.3.0] - 2026-08-21

### Added
- **Device inbox** — notifications pushed from external sources via the
  server's webhook channels: browse / open / mark-read, an unread badge
  on the home screen, and full-screen **URGENT** reminders with an
  insistent two-tone siren for high-priority alert messages.
- **Lightweight urgent poll** — replaces the long-poll: every 30 s (on
  the wall-clock :00/:30 boundaries) the device asks the server with
  `X-Inkwash-Poll: 1` and gets an instant `{"urgent": bool}` answer; a
  full sync follows only when a high-priority message is pending.
- **Full GB2312 CJK fonts** — 16×16 and 12×12 bitmap fonts (Noto Sans
  SC, 7445 characters, SIL OFL 1.1) embedded and rendered mixed with the
  ASCII fonts in every screen; regenerable via
  `tools/generate_cjk_font.py`. List rows, titles and reminders truncate
  by measured width, and word-wrap respects UTF-8 char boundaries.
- **Two-way sync** — the device uploads only *locally changed* flags
  (persisted dirty-set, cleared after a successful round-trip), so edits
  made on the Server/Desktop side survive the next sync instead of being
  clobbered by the device's stale copy.
- **Cron-aligned automatic sync** — urgent polls fire at each :00/:30
  wall-clock boundary and full syncs at every `interval` boundary (top
  of the hour for 1 h, :05 marks for 5 min, ...), driven by the RTC
  instead of a boot-relative timer; a never-synced device syncs on its
  first boundary.
- **Daily NTP RTC alignment** — the PCF8563 is resynced over NTP once a
  day inside the sync connection window, so the cron boundaries never
  drift off the real wall clock.
- **DeviceContext refactor** — board / stores / wifi / counters bundled
  into one context threaded through screens, control and main instead of
  eight-argument parameter bundles.
- **NVS-safe inbox storage** — bodies are truncated to 300 chars and the
  oldest items dropped by a byte budget, so a large Chinese backlog can
  no longer fail the whole sync with `items blob too large`.

### Changed
- Full-screen URGENT / TODOS DUE / ALARM screens now share the standard
  page header (brand + title + rule); the number-picker value is
  vertically centered; the inbox detail page fits long titles by scaling
  down or wrapping.

### Fixed
- Inbox detail body text overflowed the right edge (wrapped with the
  5×7 metrics but drawn with the 8×16 font).
- Long rows and titles with CJK text overflowed (char-count truncation
  replaced with measured-width truncation).
- Word-wrap panicked on multi-byte characters (byte-splitting now steps
  back to char boundaries).
- The URGENT siren could not be dismissed with ENTER (the debouncer was
  never fed inside the ring loop).
- Stale reminder pixels remained on Home after dismissing a full-screen
  alert (no forced redraw).

## [0.2.0] - 2026-08-20

### Added
- **PC preview tool** (`tools/preview`) — renders the home screen and
  sub-screen mockups to PNG on the host using the real `home.rs` /
  `canvas` / font / icon tables, so the on-device visual language can be
  iterated without flashing.
- **5×7 tiny font and rule-separated week view** — the calendar week
  view uses a compact 5×7 font with rule separators, and long todos wrap
  inside the week-view columns.
- **GO TO left-hand overlay bar** — the navigation drawer now slides out
  as a left-hand overlay bar covering the current page, with a border
  and selection box for the highlighted entry.
- **Unified home visual language** — all on-device screens (home,
  calendar, alarms, todos, settings) share one consistent paper + ink
  visual language.
- **Local release script** (`scripts/release.sh`) — builds the release
  ELF locally, tags, and publishes a GitHub Release with the firmware
  asset.

## [0.1.0] - 2026-08-19

### Added
- Initial release: complete offline-first calendar / alarm / todo
  firmware for the Zectrix Note 4 (ESP32-S3, 4.2″ SSD2683 e-paper).
- **Offline-capable alarms** — daily / weekly / monthly / one-shot
  schedules rung by the PCF8563 hardware alarm, with deep-sleep wake,
  no network dependency.
- **Interactive calendar** — month grid with due-today dots; pick a day
  to open a week view listing what's due.
- **Todos** — importance (low/med/high), due dates, repeat schedules,
  one-shot reminders for high-priority items.
- **Bidirectional HTTPS sync** — `POST /api/sync` uploads local
  `enabled`/`done` flags and applies the server's authoritative lists;
  periodic auto-sync (1/5/10/30/60 min).
- **USB + BLE control protocol** — `set_wifi`, `set_server`,
  `set_timezone`, `sync_now`, `get_status`, `clear_alarms` over USB
  Serial/JTAG or BLE GATT.
- **Deep sleep and wake** with GPIO17 power latch and ticking clock.
- Apache-2.0 license and open-source README with screenshots.
