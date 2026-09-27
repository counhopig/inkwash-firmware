# Note 4 firmware rewrite: preserve Inkwash UI

## Scope and source

Rewrite the device firmware for Zectrix Note 4. Preserve the existing Inkwash
screens, navigation, alarm, todo, inbox, settings, and device protocol behavior.
Use Slate's Note 4 firmware as a reference for lifecycle and power management,
not as a replacement for Inkwash's content model or server protocol.

Reference: https://github.com/qiujun8023/slate/tree/master/firmware

The current implementation is in `rust-firmware/src` and `logic/src`. The UI
contract is primarily `rust-firmware/src/{ui,screens,home,inbox,todos,display}.rs`
and `logic/src/app.rs`. Keep these user-visible flows working throughout the
rewrite, including BLE and USB commands.

## Power architecture to adopt

1. Classify boot before starting expensive services: button/USB wake enters the
   full UI; RTC timer wake runs only the due synchronization or maintenance work;
   RTC alarm wake handles the alarm. Return to deep sleep after background work.
2. Make wake scheduling explicit. Compute the next wake from the earliest RTC
   alarm, sync boundary, urgent inbox boundary, and maintenance deadline. Do not
   use a fixed periodic wake that silently misses a shorter configured interval.
3. Before sleep, drain pending UI renders and storage commits, stop network and
   audio services, power down the EPD rail, and release AVDD only after the RTC
   wake plan is programmed. Keep the main power latch held as required by Note 4.
4. Keep USB sessions awake so replies remain reliable. Test battery behavior
   separately because a connected host intentionally blocks deep sleep.
5. Record boot reason, planned next wake, sleep blockers, and durations in logs.
   These diagnostics must be available without a diagnostic firmware build.

## Compatibility and verification gates

| Stage | Required result |
| --- | --- |
| Boot and board | Correct ESP32-S3 Note 4 pins, power latch, RTC, EPD, buttons and battery reading. |
| UI | Pixel and navigation parity for Home, alarms, todos, inbox and settings. |
| Data | Existing NVS settings and the USB/BLE/server protocols remain readable. |
| Sleep | Button, RTC alarm, timer and USB wake paths work; no pending reply or write is lost. |
| Power | Compare battery-only runtime with the current firmware using the same usage pattern. |

No flash should occur until the complete image builds and the authorized Note 4
identity is verified before that flash. The required image settings are ESP32-S3,
16 MB, DIO, 80 MHz, and `rust-firmware/partitions.csv`.

## Status

Done (host-tested; firmware side not yet built or measured on a device):

- Timer wakes are classified (`WakeCause::Timer`) and run as background wakes:
  redraw, sync if its boundary passed since the last successful sync, then deep
  sleep once idle instead of waiting out the idle timeout. This was the main
  drain: each 10-minute timer wake used to stay up for the full 5-minute
  timeout, with urgent polls over Wi-Fi every 30 s.
- The deep-sleep timer lands on 10-minute wall-clock boundaries, so the Home
  clock stays current. A one-shot alarm in a later month can pull it earlier.
  Sync intervals shorter than 10 minutes apply only while awake.
- The idle timeout before deep sleep is 3 minutes (was 5).
- The ES8311 analog path is powered down between tones. The NFC tag is powered
  only during its read, and held off in deep sleep.
- Deep sleep with no timer planned clears the timer wake source that automatic
  light sleep leaves armed.

Open:

- AVDD (GPIO42) stays on in deep sleep. The PCF8563 alarm line and the I2C
  pull-ups may share that rail, so switching it off must be checked on hardware
  first.
- Board bring-up still initializes every peripheral on a background wake.
- While awake on Home with Wi-Fi configured, urgent polls still run every 30 s.
- No battery measurement has been taken yet.

## Risks recorded before the power changes

- `logic/src/app.rs` attempts deep sleep after roughly five minutes idle;
  light sleep wakes for a one-second poll in `rust-firmware/src/main.rs`.
- Deep sleep currently uses an at-most-ten-minute timer in `logic/src/app.rs`.
  This is independent of the selected one-, five-, ten-, thirty-, or sixty-minute
  sync interval, so the wake schedule must be modeled with the sync scheduler.
- `rust-firmware/src/board.rs` initializes audio, NFC, I2C and display during
  normal board creation. A background wake should initialize only needed devices.
- A USB serial session is not a valid measurement of battery deep sleep because
  USB connection is a deep-sleep blocker.
