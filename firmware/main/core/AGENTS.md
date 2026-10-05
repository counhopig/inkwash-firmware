# Core domain and policies

## OVERVIEW

Hardware-free calendar, models, JSON contracts, scheduling and retained-record arithmetic.

## WHERE TO LOOK

| Task | File | Notes |
| --- | --- | --- |
| Calendar and weekdays | `datetime.cc` | Leap years, epoch conversions and minute comparisons |
| Alarm/todo models | `model.h` | Repeat variants, optional dates and urgency predicate |
| JSON model conversion | `codec.cc` | Typed fields and compact serialization |
| Control command parsing | `protocol.cc` | Command validation and reply construction |
| Server response validation | `sync_payload.cc` | IDs, repeat rules and serialized storage budgets |
| Alarm programming rules | `schedule.cc` | Next occurrence and PCF8563 register selection |
| Sleep admission/deadlines | `power_policy.cc` | Work flags, minute deadline and UTC retry validity |
| Refresh selection | `refresh_policy.cc` | Pure decision inputs; no panel calls |
| Boot failure accounting | `boot_guard.cc` | Reset classification and bounded ledger |
| Retained trace operations | `sleep_trace.cc` | Validation, overwrite and oldest-first access |

## CONVENTIONS

- Keep this directory compilable on the host; hardware adapters belong outside `core/`.
- JSON uses ESP-IDF's bundled cJSON; `codec::Print` consumes and frees its argument.
- Missing or null optional fields keep defaults; present fields with incorrect types fail decoding.
- Integer decoding checks both range and integrality before narrowing.
- Weekly repeat days use Sunday = 0; monthly repeat days use 1–31.
- Stored data sanitization and server response validation are separate operations.
- Scheduling exposes PCF8563-compatible alarm registers; month-bound one-shots require maintenance wakes.

## ANTI-PATTERNS

- Do not bypass fractional-number/range rejection (`codec.cc:21`).
- Do not accept duplicate server IDs or payloads exceeding alarm/todo storage budgets (`sync_payload.cc:58`).
- Do not remove retry deadline invalidation after a backward clock shift (`power_policy.cc:14`).
- Do not enlarge the retained trace beyond its compile-time memory budget (`sleep_trace.h:32`).

## NOTES

- `../../test/core_test.cc` holds host cases; `../../test/run.sh` compiles every `core/*.cc` with cJSON.
- The host runner uses a temporary output directory and removes it on exit.
- Extend existing host cases when changing calendar boundaries, malformed JSON, wake deadlines or ring corruption handling.
- `boot_guard::Ledger` is a value model; actual RTC placement belongs to the power adapter.
