# Firmware Remaining Work

Updated: 2026-08-25  
Flashed revision: `b38ff41` (`main`) - reflashed and reverified 2026-08-25
after a run of code-only refactors (datetime/alarm_schedule calendar-math
consolidation, a shared NVS blob-store helper, and item -2 below); boot was
clean (no watchdog reset or panic) and item -2's dedup path was confirmed
directly on the device. The 2026-08-22 pass below (items -1, 3, 6, and
"Remaining engineering work" #1-2) predates this reflash and was only
verified with `cargo check`/`cargo build --release`/`cargo test` on the
host at the time - those items' hardware-verification status is unchanged
by this reflash; do not read "fixed in code" as "verified on hardware" for
anything still marked that way below.

## Newly discovered from desktop/device logs

-3. ~~**P1 — A backward RTC jump permanently disables NTP realignment.**~~
    **Fixed and verified on physical hardware** (2026-08-25). Found live on
    the user's own device: `--status` kept reporting `2026-08-22` days
    after the actual date. Root-caused with a temporary diagnostic log in
    `maybe_align_rtc`: the stored `rtc_align_epoch` marker read
    `2026-08-24T06:00:00Z` while the RTC's current clock read
    `2026-08-22T13:22:26Z` - the "last successfully aligned" timestamp was
    *40+ hours ahead* of the clock it's supposed to be tracking.

    **Root cause:** at some point this device's PCF8563 lost backup power
    (`voltage_low`), and `main.rs`'s boot-time reseed path set the clock
    from `BUILD_EPOCH_SECS` (a stale, already-days-old firmware build's
    embedded compile time) without touching `rtc_align_epoch`, which still
    held the timestamp from a *previous, genuinely successful* NTP
    alignment that predated the power loss. `sync::maybe_align_rtc` computed
    staleness as `now.to_unix().saturating_sub(last) >= 24h` - once `last`
    ended up later than `now`, `saturating_sub` clamped the negative delta
    to 0, which reads as "just aligned". This is a stable trap: with no
    error surfaced anywhere, the device silently never becomes eligible for
    NTP realignment again, no matter how many days pass or how many
    otherwise-successful syncs run.

    **Fixed:** `maybe_align_rtc` now compares with `abs_diff` instead of
    `saturating_sub`, so a clock that jumped *backward* by 24h+ is exactly
    as strong a signal to resync as one that's 24h+ stale forward - both
    now correctly trigger realignment. Belt-and-suspenders: the boot-time
    VL reseed in `main.rs` now also calls the new
    `PersistedCounters::clear_rtc_align_epoch()` right after reseeding the
    clock, so a stale marker from before a power-loss event can never
    survive into the reseeded era in the first place, regardless of what
    the numeric delta happens to be.

    **Verified on physical hardware**, on the exact device this was found
    on: flashed the fix, ran `--sync` twice. First attempt: `align_due`
    correctly flipped to `true` and NTP was attempted (previously it never
    was), but timed out (`esp_idf_svc::sntp: ... timed out waiting for NTP
    sync`) - a transient network hiccup, not a regression. Second attempt:
    `NTP sync OK; RTC set to 2026-08-25 12:53:40` - the device's real
    current date/time. `--status` afterward confirmed `PCF8563: 2026-08-25
    12:53:53 vl=false`.

-2. ~~**P1 — A resent command is fully re-executed, not just
    re-acknowledged.**~~ **Fixed and verified on physical hardware**
    (2026-08-25). Found the same day from a real USB session log
    (`inkwash-desktop`, flashed revision `c8c83dc` - older than this doc's
    `ac995ca` reference, but the bug reproduced unchanged against current
    `main` too; grepped for any existing dedup logic and found none).
    Sequence: user sent `set_wifi`
    (`req-2`); Wi-Fi association took ~28s (repeated `Haven't to connect to
    a suitable AP now!` retries); once it finally succeeded, the log shows
    `USB control: Wi-Fi credentials saved for 'Ccloude_2.4G'` **9 more
    times** over the next ~23s, each preceded by a full disconnect/
    reconnect/DHCP cycle and each followed by `reply id 'req-2' does not
    match in-flight request 'req-3'; ignoring` (the desktop had already
    moved on to `sync_now`, `req-3`, by the time these extra replies
    arrived). `sync_now` itself then also fully ran twice (`Sync applied:
    ...` logged twice, ~33s apart) for the same reason.

    **Root cause:** `inkwash-desktop`'s `send_and_wait` (`commands/
    device.rs`) resends a command under the same `request_id` every 2s
    while waiting for a reply, on the stated assumption "All protocol
    commands are idempotent, so a resent command is safe." That's true of
    the *end state* but not of the *cost* - `usb_console::UsbConsole::
    poll_command` queues every resent copy as an independent command with
    no awareness that an identical `(id, Command)` was already dispatched,
    and `DeviceContext::poll_usb_control`/the BLE dispatch sites
    unconditionally ran every one of them through `control::dispatch`. Any
    command slower than the 2s retry interval - Wi-Fi association is the
    common case, but this applies to `sync_now` too - accumulates a backlog
    of duplicate resends that all get *fully executed* once the device
    catches up, not just re-acknowledged.

    **Fixed:** `DeviceContext` now carries `last_command: Option<(String,
    Command, Reply)>` - the last id-tagged command actually dispatched, and
    the reply it produced. `control::dispatch` takes the correlation `id`
    as a parameter and checks it (plus a full `Command` equality check, not
    just `id`) before doing any work; a match replays the cached `Reply`
    instead of re-executing. The key is `(id, Command)` together rather
    than `id` alone deliberately: the desktop's request-id counter restarts
    at 1 on every process launch, so `id` collisions *across sessions* are
    the normal case, not a rare edge case - keying on content too means a
    same-numbered command from an unrelated session can never replay a
    stale reply for the wrong command. `Busy` replies never populate the
    cache (they don't go through `dispatch` at all), so a duplicate that
    arrives while a blocking screen is up is still correctly retried later,
    not permanently told "busy".

    **Verified on physical hardware** (2026-08-25, flashed `b38ff41`):
    booted clean, no watchdog reset or panic, prior Wi-Fi/server config
    survived the reflash. `--sync` over USB completed with exactly one
    `Sync applied`/`USB control sync completed` line, no duplicates, under
    normal (non-delayed) conditions. Directly exercised the dedup path
    itself rather than waiting for a real slow AP association to trigger a
    natural resend: sent `{"cmd":"get_status","id":"dup-test"}` twice
    back-to-back over the raw USB serial line with no wait in between (the
    same shape a client's 2s-retry-on-timeout would produce). Device log:
    the first copy executed and replied normally; the second logged
    `control: Duplicate command id=dup-test; replaying cached reply without
    re-executing` and returned the byte-identical cached reply instead of
    re-running `GetStatus` - confirming the `(id, Command)` cache hit and
    replay path both work as designed. `set_wifi`/`sync_now` were not
    re-tested against a genuinely slow-to-associate AP (the original
    trigger condition), so the *end-to-end* desktop-driven repro from the
    bug report is still open as a lower-priority follow-up, but the
    mechanism the fix relies on is now hardware-confirmed directly.

-1. **P0/P1 — Task watchdog abort + reboot during background sync.**
    Found by accident 2026-08-22 during an otherwise-unrelated alarm test:
    after ~2.5 hours and dozens of successful sync cycles in one boot
    session, the device hard-crashed mid auto-sync:
    ```
    E task_wdt: Task watchdog got triggered... main (CPU 0)
    E task_wdt: Tasks currently running: CPU 0: IDLE0, CPU 1: IDLE1
    E task_wdt: Aborting.
    Rebooting...
    ```
    Last log line before the ~4s silence-then-crash was
    `esp-x509-crt-bundle: Certificate validated`, i.e. it was somewhere
    past the TLS handshake in `sync::sync_now`, most likely reading/parsing
    the HTTP response or applying it. `main (CPU 0)` not running (both
    cores idle) at the moment of the check means the main task was truly
    stuck, not just slow.

    **Root cause (high confidence) and code fix, 2026-08-22 (not yet
    hardware-verified):** `fetch_and_apply` (`sync.rs`) called
    `watchdog::feed()` once right after the HTTP status check, then had a
    single unbroken `io::try_read_full` call to read up to 16KB of response
    body, followed by JSON parse, three separate NVS blob saves, an I2C RTC
    alarm reprogram, and two more NVS writes - all with zero further
    `watchdog::feed()` calls before returning. `CONFIG_ESP_TASK_WDT_TIMEOUT_S`
    is 10s (`sdkconfig.defaults`); `HttpConfiguration` never set an explicit
    `timeout`, leaving the native `esp_http_client`'s own default socket
    timeout in effect instead of a value under our control. A slow/stalling
    read (worse the longer the boot session ran, if NVS write latency grew
    with flash fragmentation from repeated saves - consistent with this only
    showing up after "dozens of successful sync cycles") could plausibly
    exceed the 10s budget with the main task genuinely blocked on I/O, not
    spinning - matching the backtrace exactly. `poll_urgent` had the same
    single-unbroken-read shape on a smaller buffer.

    Fixed: both HTTP clients in `sync.rs` now set an explicit `timeout:
    Some(Duration::from_secs(8))`, and the response-body read is a
    watchdog-feeding loop (`read_body_fully`) instead of one blocking call;
    `watchdog::feed()` was also added after the body read, after JSON
    parse+validate, and after the NVS/RTC writes that follow. This directly
    addresses the specific gap found, but the root cause is inferred from
    logs, not reproduced under a debugger - **do not downgrade this to
    "fixed" in the Release blockers section below until a multi-hour soak
    (see `docs/hardware-smoke-test.md` §1) runs clean on real hardware.**
    Heap-usage logging and backtrace symbolization were not done (the fix
    above made them unnecessary to proceed, but revisit if the soak still
    reproduces a crash).
0. ~~**P0 — Weekday is off by one everywhere.**~~ **Fixed and verified on
   physical hardware** (2026-08-22, user-reported): device showed Friday on
   a Saturday. Both epoch-to-weekday conversions
   (`rtc::DateTime::from_unix`, `alarms::weekday_from_days`) used `(days +
   3) % 7`; 1970-01-01 (a Thursday) is `4` under the codebase's documented
   `0=Sunday..6=Saturday` convention, not `3` - confirmed against 5 known
   reference dates in a standalone check. `screens::weekday_of` (the
   calendar grid) was never affected - it independently computes weekday
   via Sakamoto's algorithm and its own comment notes it deliberately
   doesn't read `DateTime.weekday`, which in hindsight reads like the
   original author already distrusted that field. Every other consumer
   (Home's weekday label, `Repeat::Weekly` alarm/todo matching, the PCF8563
   hardware alarm's weekday match register) was wrong by one day.
   **Note for whoever's on call next:** the PCF8563 has its own free-running
   weekday register, only rewritten on `write_time()` (NTP resync, boot
   reseed after power loss, `set_timezone`) - flashing the fix alone does
   not retroactively correct an already-stored bad value. Fixed here by
   sending `set_timezone` with the device's existing offset (480) right
   after flashing, which forces a `read -> recompute via from_unix -> write`
   cycle without needing a full NTP resync.
1. ~~**P0 — Todo reminder date cannot be persisted.**~~ **Fixed and verified
   on physical hardware** (2026-08-22): synced a real high-priority due-today
   Todo down from the server; the device rang `TODOS DUE` once with no
   `ESP_ERR_NVS_KEY_TOO_LONG` in the log. NVS key renamed
   `todo_reminded_date` (18 chars) -> `todo_rem_date` (13 chars); see
   `storage.rs`.
2. ~~**P0 — A persistence failure still presents the reminder.**~~ **Fixed.**
   `remind_due_todos` now returns without ringing when
   `set_todo_reminded_date` fails; see `reminders.rs`.
3. **P1 — Long reminder screens delay USB replies.** *Mostly fixed:* the
   due-todo, urgent-inbox, and (as of this pass) RTC alarm-ringing screens all
   now poll the USB console and reply `{"status":"busy"}` (dropping, not
   queueing, the command) instead of leaving the client waiting in silence —
   see the `Busy` reply in `control-protocol.md`. The shared helper moved to
   `usb_console::reject_pending_command` so `alarms.rs` doesn't need to depend
   on `reminders.rs` for it. Still open: BLE is untouched everywhere -
   `BleControl` isn't reachable from any of these call paths (it's owned by
   `main.rs`, not `DeviceContext`), so BLE commands during any blocking screen
   still get no reply at all, not even `busy`.
   **Verified on physical hardware** (2026-08-22): sent `get_status` over USB
   while the due-todo reminder from item 1 was actively ringing and got
   `{"status":"busy"}` back in a few seconds; repeated with a live RTC alarm
   ring - same result, `{"status":"busy"}` within ~3s of the alarm firing.
   Well inside the 45s desktop timeout either way, no more silent hang.
   **BLE gap closed in code, 2026-08-22 (not yet hardware-verified):**
   `DeviceContext` now owns a `ble_control: &mut Option<BleControl>` field
   (a reference to the slot `main.rs` already owned, not the `BleControl`
   itself - its lifetime still differs, populated only while the pairing
   screen is open) instead of BLE being threaded as a separate parameter
   that never reached the blocking screens. `ble_control::reject_pending_command`
   mirrors the USB version; every call site that already called
   `usb_console::reject_pending_command` (due-todo, urgent-inbox, RTC
   alarm-ringing) now also calls the BLE version when BLE is active. Needs a
   hardware pass with the pairing screen open during a ring/reminder to
   confirm a queued BLE command actually gets `busy` back - see
   `docs/hardware-smoke-test.md` §2.
3a. ~~**P0 — Alarm-ring screen's ENTER dismiss was unusable.**~~ **Fixed and
    verified on physical hardware across 5 real alarm firings** (2026-08-22,
    user-reported: "闹钟收到了，但是 enter=dismiss 没有效果"). Two compounding bugs
    in `alarms.rs::ring_until_dismissed`, found live:
    - The loop polled the button only once per audio tone
      (`audio::play_sine_stereo` blocks ~210ms per call - see
      `drain_and_disable`'s unconditional 150ms drain sleep), so a press
      entirely inside that gap was invisible to the debounce state machine,
      which only advances when `poll()` runs. First live test: pressed ENTER
      repeatedly for the entire 300s `MAX_RING_SECS` safety window, zero
      effect. Fixed with a tight 20ms-granularity poll window between tones,
      matching `reminders.rs::show_urgent`'s already-working pattern.
    - Even with that fix, the shared debounced `is_pressed()` state needed a
      hold of over a second before registering at all - user description:
      "感觉像在触发菜单的长按手势" (felt like the menu's long-press gesture) - on a
      button (`key_enter`, GPIO0) this codebase otherwise treats as instant
      everywhere else. Root cause not fully pinned down (electrical bounce
      characteristics on this specific button are the leading theory - GPIO0
      is also shared with the USB auto-reset circuit, though a same-bug
      repro with zero USB connection open ruled that specific interaction
      out). For a safety-critical dismiss path a false positive from noise
      is far cheaper than a false negative (a stuck alarm), so this now
      reads the raw pin level directly (`Button::is_raw_pressed`, bypassing
      debounce) instead of going through the shared state machine. Verified
      fixed: sound stopped and the alarm dismissed within ~0.8s of a normal
      short press (`Alarm dismissed` -> next `Partial display refresh
      completed` log line).
    - A third symptom (screen visibly stuck on "ALARM" for a few seconds
      after the sound stopped) turned out to be a red herring once measured
      precisely - see the 0.8s figure above - not a real bug, just the
      earlier tests' impression before timing it.
4. ~~**P1 — Command correlation is implicit.**~~ **Fixed and verified on
   physical hardware, both sides** (2026-08-22): commands may now carry an
   optional `id` (any string), echoed back on the reply; omitted `id` means
   no wire-format change for old clients. See the `Request Correlation`
   section in `control-protocol.md`, `control::parse_command`/`render_reply`.
   Firmware verified on hardware: a `{"cmd":"get_status","id":"req-42"}`
   request got `"id":"req-42"` back; a plain request with no `id` got a
   reply with no `id` field, byte-identical to before this change.
   `inkwash-desktop` (separate repo) now generates one per request, reuses
   it across resends of that request, and ignores a reply whose echoed id
   doesn't match instead of mistaking it for the current answer; verified
   via that repo's CLI (`--status`/`--sync`) against the same physical
   device. Also picked up `Reply::Busy` there, which `send_and_wait`
   auto-retries transparently instead of surfacing to callers.
5. **P2 — Startup status is requested more than once.** During the USB reset
   and boot sequence the desktop sends `get_status` twice and receives two
   valid status replies. This is not a firmware failure, but the UI/logging
   should coalesce identical startup probes so users do not mistake them for
   duplicate actions.
6. ~~**P2 — `esp-idf-svc` 0.52.1 always associates with PMF advertised as
   unsupported, ignoring `ClientConfiguration.pmf_cfg`.**~~ **Fixed in code,
   2026-08-22 (not yet re-verified on the failing hardware).** Root-caused
   2026-08-22 on hardware: a router with WPA2/WPA3-mixed + PMF-required
   rejected the device shortly after association (`assoc -> init` right
   after the `Wi-Fi connected` log line, before DHCP could run), while the
   same router in WPA2-only mode connected cleanly. Traced to
   `esp-idf-svc-0.52.1/src/wifi.rs`'s
   `TryFrom<&ClientConfiguration> for Newtype<wifi_sta_config_t>`, which
   hardcodes `wifi_pmf_config_t { capable: false, required: false }` and
   never reads `conf.pmf_cfg` — so nothing `wifi.rs::connect()` sets on the
   Rust side can change it. Fixed by doing exactly the bypass this item
   originally proposed: `WifiManager::connect()` now reads back the STA
   config the wrapper just wrote (`esp_wifi_get_config`), patches only
   `pmf_cfg` to `{capable: true, required: false}` (PMF optional - a
   superset of the old hardcoded `false/false`, so PMF-disabled APs are
   unaffected), and writes it back directly via `esp_wifi_set_config()` -
   the same raw FFI call the wrapper itself uses, matching how
   `connect()`/`disconnect()` already bypass the wrapper elsewhere in this
   file. Needs a hardware pass against the specific PMF-required router that
   originally failed - see `docs/hardware-smoke-test.md` §1.

The same log also confirms that consecutive scan-free Wi-Fi connections work
within one boot, HTTPS certificate validation succeeds, and a full sync applies
alarm/Todo/Inbox state. Those paths should remain unchanged while fixing the
issues above.

## Verification completed in this pass

- ESP32-S3 release build completed successfully.
- Firmware flashed through `/dev/tty.usbmodem1101` using DIO, 80 MHz and a
  16 MB flash layout.
- Boot ROM confirmed ESP32-S3 revision 0.2, DIO mode, 16 MB flash and 8 MB
  PSRAM; the PSRAM memory test passed.
- The application initialized the power latch, RTC, display and Wi-Fi stack
  without a watchdog reset or panic.
- The RTC retained a valid clock and the display completed full and partial
  refreshes.
- An aligned background network cycle connected, obtained DHCP and validated
  the HTTPS certificate after boot.

### 2026-08-22 hardware pass (items 1 and 3 above)

- `set_wifi` reconfigured live (previous stored network was unreachable from
  this location); verification correctly rejected a bad DHCP-timeout attempt
  before saving, then saved once a working network connected - see item 6 for
  the PMF root cause of the first attempt's failure.
- `sync_now` over USB pulled 2 Todos from the live server (0 alarms, 0 inbox);
  `Wi-Fi already used this boot session; attempting a second connect
  (scan-free)` fired twice more in the same boot (once for `sync_now`, once
  for the next urgent-poll boundary) and both reconnected and completed
  cleanly - the documented "multiple connects per boot are safe" fix in
  `wifi.rs` still holds.
- The due-Todo reminder fired exactly once (item 1) and, while it was
  actively ringing, a `get_status` sent over USB got `{"status":"busy"}`
  back (item 3) instead of hanging silently.
- Opening the USB serial port with a naive client (pyserial's default
  `Serial(port, ...)` constructor, which asserts DTR/RTS before the caller
  can change them) reliably resets the chip - a full reboot from the
  bootloader, not just the documented spurious-ENTER GPIO0 pull. Not a
  firmware bug (same auto-reset circuit `espflash` itself relies on to
  attach), but `control-protocol.md`'s existing warning undersells the
  effect; worth a doc pass for PC-tool authors on setting `dtr`/`rts` to
  `False` *before* opening the port.

### 2026-08-22 second hardware pass (alarm ring/dismiss, item 3a above)

- Live-fired 5 real one-shot alarms (server-side `Once` schedule, synced down
  each time) to iterate on the ENTER-dismiss bug - see item 3a for the two
  root causes found and fixed.
- AF acknowledgement (`ack_alarm`) and one-shot removal from the alarm store
  both confirmed working: `Hardware alarm armed: id=... (Once {...})` at
  sync, `Alarm dismissed` at ring time, then `No enabled alarms; hardware
  alarm cleared` after - the fired one-shot was correctly dropped and no
  stale hardware alarm was left armed.
- This alarm path fired via the *device-already-awake* route
  (`ctx.rs::poll_alarm`'s `RTC alarm fired while device was awake; ringing`),
  not the deep-sleep-wake boot path (`main.rs`'s `alarm_fired_at_boot`
  branch) - the two share `ring_until_dismissed`/`handle_fired_alarm` so the
  same fixes apply to both, but deep-sleep wake itself (and its ~2 hours
  later, mid-pass) surfaced item -1's watchdog crash - unrelated to this
  work but found during it.
- Ruled out the USB-connection/GPIO0-sharing theory for the slow-dismiss
  symptom: reran with *zero* USB connection open during the ring and got the
  identical "single press mutes, doesn't exit; long hold does" symptom,
  which is what motivated the `is_raw_pressed` fix over trying to tune the
  shared debounce constants.

### 2026-08-25 hardware pass (item -2 above, request-id dedup)

- Built `--release` and flashed `b38ff41` via `espflash flash` (no
  `--monitor`, to avoid blocking an automated session) plus a follow-up
  `--status` over USB to confirm boot. `GIT_REV` read back as
  `v0.3.0-31-gb38ff41`; boot log showed no watchdog reset or panic, and the
  device's previously-saved Wi-Fi/server config survived the reflash
  unchanged (NVS partition untouched by a plain app-partition flash).
- `--sync` completed with exactly one `Sync applied: 0 alarms, 0 todos, 3
  inbox` / `USB control sync completed` line - no duplicate execution under
  normal (non-delayed) conditions.
- Directly exercised the dedup path with a hand-crafted duplicate: opened
  the raw USB serial line (pyserial) and wrote
  `{"cmd":"get_status","id":"dup-test"}` twice back-to-back with no wait in
  between - the same shape a client's 2s-retry-on-timeout would produce
  while the device is still working on the first copy. Device log: the
  first copy executed and replied normally; the second logged `control:
  Duplicate command id=dup-test; replaying cached reply without
  re-executing` and returned the byte-identical cached reply. Confirms the
  `(id, Command)` cache hit and replay path both work on real hardware, not
  just in the `logic`/`protocol` host test suites.
- Not covered: a genuinely slow Wi-Fi association naturally triggering the
  desktop's real 2s retry loop (the original bug report's exact trigger)
  wasn't reproduced this pass - see item -2's note on this being a
  lower-priority follow-up.

## Required physical-device verification

1. **Alarm end-to-end:** ~~test RTC alarm wake from deep sleep, audible ring,
   ENTER dismissal, AF acknowledgement, one-shot removal and re-arming of the
   next recurring alarm.~~ *Partially done* (2026-08-22): ring, ENTER
   dismissal, AF acknowledgement, and one-shot removal all verified on
   hardware - see item 3a and the second hardware pass above. **Still not
   verified: wake from actual deep sleep** (every firing this pass happened
   while the device was already awake, via `ctx.rs::poll_alarm`, not the
   `main.rs` boot-time `alarm_fired_at_boot` path) **and re-arming the next
   recurring alarm** (only tested a one-shot `Once` schedule, which gets
   removed rather than re-armed).
2. **Alerts outside Home:** verify alarm, urgent Inbox and high-importance Todo
   screens interrupt Calendar, Settings, list/detail and BLE pairing screens,
   then return to a correctly redrawn page.
3. **Connection soak:** repeatedly disconnect/reconnect BLE and unplug/replug
   USB over a long-running session; confirm BLE teardown does not affect later
   Wi-Fi synchronization.
4. **Repeated Wi-Fi cycles:** leave the device running across many urgent and
   full-sync boundaries and confirm there is no second-connect crash or heap
   degradation. **This is no longer purely hypothetical** — see item -1: a
   watchdog abort+reboot was observed mid-sync after ~2.5 hours / dozens of
   cycles in one boot session. A high-confidence root cause was found and a
   fix applied in code 2026-08-22 (missing `watchdog::feed()` calls across a
   network-read-then-NVS-write span, plus no explicit HTTP timeout); treat
   this item as still actively failing until a multi-hour soak on real
   hardware runs clean under the fixed code - see
   `docs/hardware-smoke-test.md` §1.
5. **Large data sets:** exercise maximum practical alarm, Todo and Inbox lists;
   check pagination, truncation, NVS capacity and e-paper ghosting.

## Remaining engineering work

1. ~~Add host-runnable tests for alarm ordering/recurrence, scheduler
   boundary transitions, reminder de-duplication, ID allocation and sync
   merge rules.~~ **Done, 2026-08-22.** The bin crate cross-compiles only
   for `xtensa-esp32s3-espidf` and links `esp-idf-sys` (needs the ESP-IDF
   SDK toolchain to build at all), so no part of it could ever run under
   `cargo test`. Pulled every listed category's *pure* logic out into a new
   sibling crate, `logic/` (package `inkwash-logic`, path-dependency of
   `rust-firmware`, zero ESP-IDF deps) - deliberately placed outside
   `rust-firmware/` so it doesn't inherit `rust-firmware/.cargo/config.toml`'s
   `target = "xtensa-esp32s3-espidf"` override and builds for the host by
   default. `rust-firmware`'s modules (`rtc.rs`, `alarms.rs`, `todos.rs`,
   `inbox.rs`, `sync.rs`, `ctx.rs`, `reminders.rs`) now re-export or call
   into it instead of keeping their own copies, so it's the single source
   of truth, not a parallel implementation that can drift. Covers:
   `datetime` (epoch/weekday math - includes a regression test for the item
   0 weekday bug, checked against an independent Zeller's-congruence
   reference rather than the same formula under test), `alarm_schedule`
   (`Repeat`/`StoredAlarm`, recurrence, `next_id` allocation),
   `sync_validate` (`validate_repeat`/`validate_date`/
   `validate_sync_response`), `reminder_dedup` (due-todo "already reminded
   today" and inbox pending-read merge/ack), and `scheduler` (wall-clock
   boundary-index math). 40 tests, all passing: `cd logic && cargo test`.
   `rust-firmware` itself still builds clean for the real target
   (`cargo check --release` / `cargo build --release`, verified).
2. ~~Add a repeatable hardware smoke-test checklist or serial-log harness for
   boot, synchronization, alarm and BLE/USB recovery.~~ **Done, 2026-08-22:**
   see `docs/hardware-smoke-test.md`.
3. ~~Fix stale ESP-IDF application metadata.~~ *Worked around* (2026-08-22):
   the ESP-IDF app descriptor's `App version`/`Compile time` fields are still
   stale (confirmed on hardware: still printed `v0.3.0-14-g71062c9-dirty`
   after flashing `9e7e9b3`) and forcing them to refresh would mean touching
   `esp-idf-sys`'s CMake caching, out of scope here. Instead `build.rs` now
   embeds a fresh `git describe` as `GIT_REV`, logged once at boot
   (`main.rs`) - verified on hardware printing the correct
   `v0.3.0-18-g672e229-dirty` on the same boot where the ESP-IDF field was
   stale. Use the firmware's own log line, not `App version`, to identify
   what's actually running.
4. Replace machine-specific ESP-IDF, Python and rust-analyzer paths with a
   documented local bootstrap/configuration mechanism. *Partially done*
   (2026-08-22): `LIBCLANG_PATH` no longer needs a hardcoded per-developer
   path in `rust-firmware/.cargo/config.toml` - `scripts/build-rust.sh`/
   `.ps1` now locate it dynamically under the active `esp` rustup toolchain
   (verified end-to-end on Linux with a clean `target/` rebuild; the
   PowerShell side is unverified - no Windows machine available). Still
   open: `.vscode/settings.json` (rust-analyzer paths) and the `IDF_PATH`
   fallback in `config.toml` remain machine-specific; the former was
   untracked from git so per-developer edits stop showing up as diffs.
5. Design and implement OTA with signed images, health confirmation and a
   rollback partition before enabling remote firmware upgrades.

## Release blockers

- Do not claim the offline-alarm feature verified until physical test item 1
  passes *in full*, including deep-sleep wake and recurring-alarm re-arm -
  ring/dismiss/AF-ack/one-shot-removal are done, those two are not.
- Do not claim long-running stability until item -1's watchdog crash is
  confirmed fixed on hardware - a high-confidence root cause was found and a
  fix applied in code 2026-08-22, but it's inferred from logs, not
  reproduced under a debugger; treat it as an active, reproduced failure
  until a multi-hour soak (`docs/hardware-smoke-test.md` §1) runs clean.
- Do not ship OTA until rollback and power-loss behavior are demonstrated.
- Continue flashing only ESP32-S3 NOTE4 images in DIO mode; NOTE4C firmware and
  QIO mode remain forbidden.
