# inkwash-logic AGENTS

**Generated:** 2026-09-02 · **Updated:** 2026-09-11 · **Branch:** main

## OVERVIEW

`inkwash-logic` — pure, host-testable lib crate (edition 2021, ZERO ESP-IDF deps). `rust-firmware` path-deps on it and re-exports types from the firmware's usual module names (`rtc::DateTime`, `alarms::{Repeat, StoredAlarm}`, `sync::{validate_repeat, validate_date}`). Hosts the entire test suite — **383 `#[test]` markers across the 29 source files** (`app.rs` alone holds 155).

> **Anchor discipline.** Every `file:line` below was re-verified against the working tree. Prefer the named symbol; the line number is a convenience for this revision, not the contract.

## WHERE TO LOOK

| Concern | File |
| --- | --- |
| Wire-type source of truth | `alarm_schedule.rs` (`Repeat` serde externally tagged) / `todo.rs` / `datetime.rs` (`DateTime` + Zeller-oracle weekday) / `inbox_item.rs` / `device_config.rs` |
| Control-channel wire protocol | `protocol.rs` (`Channel`/`Command`/`Reply`; `MAX_COMMAND_NESTING` + `nesting_exceeds` — the pre-parse depth cap bounding deserializer recursion, because peak stack follows frame shape rather than length) |
| Calendar math | `alarm_schedule.rs` (Daily/Weekly/Once/Monthly; next-occurrence; boundary wrap; Feb 29 → real 29th) / `datetime.rs` (unix↔`DateTime` roundtrip; minute shifting) |
| Render planning | `render_plan.rs` (`plan_render` `:177`; Home minute change → `PartialRegion::Clock` `:235` — moved here out of the firmware adapter so it is host-testable) |
| Sync validation | `sync_validate.rs` (dup-id reject, time/date range, Feb-29 rule, unsupported `Once`; `serde_json` only for size caps) |
| State machine | `app.rs` (`update()` pure; boot/dismiss/rearm; op-id staleness; busy transport; arbitration latches `:1061-1118`) |
| Host fake harness | `harness.rs` (`FakeExecutor` + `ScriptedFailures` + `FakeHostAdapter`; `Harness::new` `:361` drives the shared `AlarmPoll` orchestration firmware also uses) |
| Reminder + scheduler | `reminder_dedup.rs` / `scheduler.rs` (wall-clock hourly alignment; zero-index guard) |
| EPD registry | `epd_registry.rs` (one terminal per request id; supersede; failure terminal; unknown ignored) |
| Event queue + command sessions | `event_queue.rs` / `command_sessions.rs` / `runtime.rs` (the driving loop firmware mirrors) |
| Sleep / power policy | `power_state.rs` (`SleepInputs` `:34-48`; `SleepKind`; admission gates) |
| BLE radio arbitration | `ble_radio.rs` (`BleRadioCoordinator`; BLE/Wi-Fi mutual exclusion) |
| UI control primitives | `button_event.rs` / `wake_cause.rs` / `list_window.rs` / `protocol.rs` (`Command` serde; keyed reply match) |
| RTC register layout | `alarm_regs.rs` |
| RTC write serialization | `rtc_latch.rs` |

## CODE MAP

- `app::update` contract — the doc comment at `logic/src/app.rs:17-19` (**MUST NOT** call hardware, NVS or IO; a transition that cannot decide on pure data leaves state intact and lets the executor report back an `Event`).
- `Repeat::kind` — `logic/src/alarm_schedule.rs:47` (server SQLite `repeat_kind` column value — must NOT change without server migration).
- `Importance::as_str` — `logic/src/todo.rs:25` (server SQLite column value — must not change without migration).
- `DateTime` — `logic/src/datetime.rs` (weekday validated against a hand-rolled Zeller reference impl as ground-truth oracle).
- `EffectBatchId` / `RenderGeneration` / `OperationId` — `logic/src/app.rs` (unique IDs for outcome supersede/terminal tracking).
- `AlarmHost` trait — `logic/src/alarm_flow.rs:23`; `AlarmPoll::poll` — `:84`.

## CONVENTIONS

- Inline `#[cfg(test)] mod tests` per source file; no dev-dependencies; no `proptest`/`rstest`/`serde_test`.
- Clock/time injected as explicit `&DateTime` arg — no `now()` mock, no mocking crate.
- Test helpers: tiny hand-built struct literal helpers (`dt()`, `dt_full()`, `alarm()`, `boot_snapshot()`), plain `assert!`/`assert_eq!`/`matches!`; `serde_json::to_vec` used only to enforce size caps.
- No property tests, no JSON wire-format fixtures (size caps only).
- `lib.rs` docs explain the host-test split rationale.

## ANTI-PATTERNS

- `app::update` MUST NOT call hardware/NVS/IO (`app.rs:17-19`) — emit `Event` back to the executor instead.
- `Repeat` discriminator string is the server's column (`alarm_schedule.rs:47`) — no rename without server migration.
- `Importance::as_str` is the server's column (`todo.rs:25`) — no rename without server migration.
- Daily alarms never 'expired'; `Once` past-date must never be re-armed; a Feb-29 skip must land on the next real 29th, never a clamped 28th (`alarm_schedule.rs:546` test `next_occurrence_date_monthly_skips_feb_29_on_non_leap_years`).
- Residue ACK: a repeated GPIO5 snapshot while an ACK is pending must NOT be re-interpreted — it could overwrite `pending_residue_ack` with a fresh op id (stranding the original completion) or re-read the same uncleared AF as a real trigger and ring (`app.rs:2072-2082`; field `:1092`).
- `BootSnapshot` init must NOT clear AF/AIE/alarm register — the state machine decides (`app.rs:797-798`).

## COMMANDS

```bash
cd logic && cargo test                  # full host suite
cd logic && cargo test --locked         # CI parity
cd logic && cargo fmt -- --check
cd logic && cargo clippy --all-targets -- -D warnings
```

## NOTES

- CI covers ONLY this crate (`.github/workflows/ci.yml` job `logic`, ubuntu + stable toolchain); `rust-firmware` has no CI (built locally via `scripts/build-rust.sh`). Nothing in CI formats, lints, or builds the firmware crate.
- New pure logic → land it here; `rust-firmware` adds the hardware-facing wrapper. Re-export from the firmware's natural module name to keep callers stable.
