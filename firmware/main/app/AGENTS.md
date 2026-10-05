# Application orchestration

## OVERVIEW

One application task owns screens, runtime state, replies and deadlines; producers deliver events.

## WHERE TO LOOK

| Task | File | Notes |
| --- | --- | --- |
| Event loop and workers | `app.cc` | Queue ownership, draining and coalesced BLE link state |
| Shared runtime types | `internal.h` | `State`, `Event`, `NetRequest` and internal API |
| Startup and sleep | `lifecycle.cc` | Retained Home reconstruction, housekeeping and wake planning |
| Control commands | `commands.cc` | Per-channel reply slots and network admission |
| Sync completion | `sync.cc` | Reconcile local edits, resolve replies and retain retry deadline |
| Pairing lifecycle | `pairing.cc` | NimBLE callbacks, pairing timeout and radio transition |
| Screen construction | `view.cc` | Active screen replacement and scene refresh selection |
| Key handling | `input.cc` | Navigation and editable alarm/todo actions |
| Alarm/reminder state | `alarms.cc`, `reminders.cc` | Ringing, dismissal and RTC programming |
| Boot recovery UI | `safe_mode.cc` | Independent recovery loop |

## CONVENTIONS

- Mutate `g` only in the application task; callbacks enqueue events or update the atomic link mailbox.
- `Post` transfers event ownership and deletes rejected events; the consumer uses `unique_ptr`.
- Event queue capacity is 16; network request capacity is 2; drain at most 16 events per iteration.
- Network requests capture inputs; the worker posts completion without holding application locks.
- One network job runs at a time; completion clears admission and resolves deferred channel replies.
- Preserve alarm/todo toggles made during sync when loading the server's persisted lists.
- Pairing after Wi-Fi use restarts through the retained pairing flag before starting NimBLE.

## ANTI-PATTERNS

- Never block a NimBLE callback on the application queue: commands use zero timeout and link events coalesce (`pairing.cc:7`, `pairing.cc:14`).
- Do not discard network completions when the event queue is full (`app.cc:33`).
- Do not drain producers indefinitely before housekeeping or rendering (`app.cc:84`).
- Do not enter deep sleep with USB, network, ringing, reminders, pairing, replies, audio, display recovery, queued events or held keys (`lifecycle.cc:7`).
- Do not use a stale pre-refresh RTC reading to align the sleep deadline (`lifecycle.cc:47`).
- Do not overwrite pending local toggles with sync results (`sync.cc:60`).

## NOTES

- Timer wakes begin in background mode; a key event switches to interactive mode.
- Wake startup reconstructs the retained Home frame before drawing current content (`lifecycle.cc:182`).
- RTC alarm handling precedes pairing and reminder presentation at boot (`lifecycle.cc:193`).
- Sleep admission is checked again after drawing the final Home frame.
- Retry deadlines survive minute wakes in retained state; clock-setting commands realign scheduler cursors.
- Reply slots are independent for USB and BLE; pairing teardown clears the BLE slot.
