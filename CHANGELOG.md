# Changelog

All notable changes to **inkwash-firmware** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
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
- `docs/sync-api.md` documents the lightweight poll and the dirty-set
  upload semantics.

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
