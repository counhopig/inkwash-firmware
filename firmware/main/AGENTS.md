# Firmware main component

## OVERVIEW

Hardware bootstrap, LVGL adapter and transport services within one ESP-IDF component.

## STRUCTURE

- `app/`: application orchestration; separate local instructions.
- `core/`: hardware-free rules; separate local instructions.
- `control/`: USB Serial/JTAG and NimBLE.
- `net/`: Wi-Fi, HTTPS and NTP.
- `storage/`: NVS schemas and sync journal.
- `power/`: retained state and sleep/wake primitives.
- `audio/`: ES8311 codec and I2S worker.
- `ui/`: screen composition and pixel-positioned widgets.
- `assets/`: compiled ASCII glyphs and icons.

## WHERE TO LOOK

| Task | File | Notes |
| --- | --- | --- |
| Initialization order | `main.cc` | Boot ledger, board, storage, RTC, display, audio |
| GPIO assignments | `board_pins.h` | GPIO42 shared audio/I2C pull-up rail |
| RTC register handling | `pcf8563.cc` | Alarm flags, interrupt enables and calendar |
| Frame conversion | `display.cc` | RGB565 strips to packed monochrome panel frame |
| Fonts | `fonts.cc` | LVGL adapter for ASCII and embedded CJK |
| Journaled sync apply | `storage/store.cc` | Recover interrupted apply and preserve pending reads |
| Retained state | `power/power.cc` | NOINIT ledger, trace and Home seed |

## CONVENTIONS

- Render LVGL synchronously through `display::Update`; the display adapter pauses the LVGL refresh timer.
- A successful panel update establishes the shadow baseline; a failed update leaves recovery pending.
- `display::Healthy()` participates in sleep admission; recover through a full baseline.
- Codec/I2S setup is lazy; queued and active audio both block sleep.
- Wi-Fi sessions end after completed work; the icon follows an active IP connection.

## ANTI-PATTERNS

- No constructor-driven initialization of RTC NOINIT objects (`power/power.cc:28`).
- No unconditional periodic full refresh for Home clock updates (`core/refresh_policy.cc:3`).
- Do not rebuild the shared GPIO42 rail independently in an audio path (`board_pins.h:7`).

## NOTES

- Enter and Down are RTC-capable deep-sleep keys; Up is available during active/light-sleep operation.
- Release RTC GPIO holds and restore normal GPIO ownership on wake.
- `ui/widgets.h` supplies pixel primitives; screen content lives in `ui/screens.cc`.
