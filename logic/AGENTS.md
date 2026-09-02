# inkwash-logic AGENTS

**Generated:** 2026-09-02 · **Commit:** e9224a1 · **Branch:** feature/power-and-response

## OVERVIEW

`inkwash-logic` — pure, host-testable lib crate (v0.1.0, edition 2021, only dep `serde`/`serde_json`, ZERO ESP-IDF deps). `rust-firmware` path-deps on it and re-exports types from the firmware's usual module names (`rtc::DateTime`, `alarms::{Repeat, StoredAlarm}`, `sync::{validate_repeat, validate_date}`). Hosts the entire test suite — 121 `#[test]` markers across 10 modules.

## WHERE TO LOOK

| Concern | File |
| --- | --- |
| Wire-type source of truth | `alarm_schedule.rs` (`Repeat` serde externally tagged) / `todo.rs` / `datetime.rs` (`DateTime` + Zeller-oracle weekday) / `inbox_item.rs` / `device_config.rs` |
| Calendar math | `alarm_schedule.rs` (Daily/Weekly/Once; next-occurrence; boundary wrap; Feb 29 → real 29th) / `datetime.rs` (unix↔`DateTime` roundtrip; minute shifting) |
| Sync validation | `sync_validate.rs` (dup-id reject, time/date range, Feb-29 rule, unsupported `Once`; `serde_json` only for size caps) |
| State machine | `app.rs` (`AppRunner` + `EffectRunner` + `Effect::Batch`; `update()` pure; boot/dismiss/rearm tests; op-id staleness; busy transport) |
| Host fake harness | `harness.rs` (`FakeExecutor` + `ScriptedFailures` + `FakeHostAdapter`; `Harness::new` drives shared `AlarmPoll` orchestration firmware also uses) |
| Reminder + scheduler | `reminder_dedup.rs` / `reminder_flow.rs` / `scheduler.rs` (wall-clock hourly alignment; zero-index guard) |
| Background outcome merge + EPD registry | `background_outcome.rs` / `epd_registry.rs` (one terminal per request id; supersede; failure terminal; unknown ignored) |
| UI control primitives | `button_event.rs` / `wake_cause.rs` / `protocol.rs` (`Command` serde; keyed reply match) |
| RTC register layout | `alarm_regs.rs` |
| App state-machine runner | `runner.rs` (`AppRunner` driving) |

## CODE MAP

- `AppRunner::update` — `logic/src/app.rs:17-19` (MUST NOT do IO; pure state-only contract).
- `Repeat::kind` — `logic/src/alarm_schedule.rs:43-45` (server SQLite `repeat_kind` column value — must NOT change without server migration).
- `Importance::as_str` — `logic/src/todo.rs:22-24` (server SQLite column value — must not change without migration).
- `DateTime` — `logic/src/datetime.rs` (weekday validated against hand-rolled Zeller reference impl as ground-truth oracle).
- `EffectBatchId` / `RenderGeneration` / `OperationId` — `logic/src/app.rs` (unique IDs for outcome supersede/terminal tracking).
- `Harness::new` — `logic/src/harness.rs:464` (drives exact shared orchestration firmware uses).
- `AlarmPoll` — `logic/src/alarm_flow.rs:48` (alarm host trait + `poll` loop at `:69`).

## CONVENTIONS

- Inline `#[cfg(test)] mod tests` per source file; no dev-dependencies; no `proptest`/`rstest`/`serde_test`.
- Clock/time injected as explicit `&DateTime` arg — no `now()` mock, no mocking crate.
- Test helpers: tiny hand-built struct literal helpers (`dt()`, `dt_full()`, `alarm()`, `boot_snapshot()`), plain `assert!`/`assert_eq!`/`matches!`; `serde_json::to_vec` used only to enforce size caps.
- No property tests, no JSON wire-format fixtures (size caps only).
- `lib.rs` docs explain the host-test split rationale.

## ANTI-PATTERNS

- `app::update` MUST NOT call hardware/NVS/IO (`app.rs:17-19`) — emit `Event` back to executor instead.
- `Repeat` discriminator string is the server's column (`alarm_schedule.rs:43-45`) — no rename without server migration.
- `Importance::as_str` is the server's column (`todo.rs:22-24`) — no rename without server migration.
- Daily alarms never 'expired'; `Once` past-date must never be re-armed; Feb 29 skip must land on next real 29th, never a clamped 28th (`alarm_schedule.rs:215, 372-374`).
- Residue ACK failure must NOT re-open AIE — RTC reprogrammed only after successful ACK (`app.rs:1078-1081`).
- `BootSnapshot` init must NOT clear AF/AIE/alarm register — state machine decides (`app.rs:319-320`).
- Never overwrite an already-pending residue ACK with a different op id — strands the original ACK's completion (`app.rs:781-784`).

## COMMANDS

```bash
cd logic && cargo test                  # all 121 host tests
cd logic && cargo test --locked         # CI parity
cd logic && cargo fmt -- --check
cd logic && cargo clippy --all-targets -- -D warnings
```

## NOTES

- CI covers ONLY this crate (`.github/workflows/ci.yml` job `logic`, ubuntu + stable toolchain); `rust-firmware` has no CI (built locally via `scripts/build-rust.sh`).
- New pure logic → land it here; `rust-firmware` adds the hardware-facing wrapper. Re-export from the firmware's natural module name to keep callers stable.