# Sync API Contract

This document specifies the HTTP contract between the inkwash firmware and the
`inkwash-server` backend service. The firmware uses this endpoint for
bidirectional synchronization of server-hosted alarms and todos.

## Request

```
POST {server_url}
Authorization: Bearer {auth_token}
Content-Type: application/json

{
  "alarms": [{"id": 0, "enabled": true}],
  "todos": [{"id": 0, "done": true, "importance": "high"}]
}
```

### Headers

- **Authorization** (required): `Bearer {auth_token}` where `auth_token` is the
  authentication token configured on the device.
- The device uploads only locally *changed* state: alarm `enabled`, todo
  `done`, and todo `importance` for items the user actually toggled on the
  device since the last successful sync (dirty-set tracking). It never
  uploads text, schedules, additions, or deletions, and it does not re-upload
  flags that only the Server/Desktop side edited - so an edit made in the
  server UI survives the device's next sync instead of being clobbered by the
  device's stale copy. `importance` is optional in the upload and old
  firmware that omits it is still accepted.
- Unknown IDs are ignored, so stale device data cannot recreate content that
  Desktop or Server deleted.
- The server merges these flags and returns its complete authoritative lists.

## Response

### HTTP 200 OK

Returned when the server has new or updated content. Body is JSON shaped as:

```json
{
  "alarms": [
    {
      "id": 0,
      "hour": 7,
      "minute": 30,
      "repeat": "Daily",
      "enabled": true,
      "label": "Morning"
    },
    {
      "id": 1,
      "hour": 22,
      "minute": 0,
      "repeat": {
        "Once": {
          "year": 2026,
          "month": 12,
          "day": 25
        }
      },
      "enabled": true,
      "label": "Christmas alarm"
    },
    {
      "id": 2,
      "hour": 9,
      "minute": 0,
      "repeat": {
        "Weekly": {
          "days": [0, 2, 4]
        }
      },
      "enabled": true,
      "label": "Gym"
    }
  ],
  "todos": [
    {
      "id": 0,
      "text": "Buy groceries",
      "done": false,
      "importance": "medium",
      "due_date": null,
      "repeat": null
    },
    {
      "id": 1,
      "text": "Call home",
      "done": true,
      "importance": "high",
      "due_date": {
        "year": 2026,
        "month": 8,
        "day": 19
      },
      "repeat": {
        "Monthly": {
          "days": [1, 15]
        }
      }
    }
  ]
}
```

#### Schema Details

**`alarms`** (array): List of alarms to store on the device. May be empty.

Each alarm object:
- **`id`** (u8): Unique identifier for this alarm; must not conflict within the
  list. Used for deduplication and tracking edits across syncs.
- **`hour`** (u8): Hour component of the alarm time (0–23, 24-hour format).
- **`minute`** (u8): Minute component of the alarm time (0–59).
- **`repeat`** (enum): Recurrence pattern. Weekdays are 0=Sunday ..
  6=Saturday (matching the RTC and JS `Date.getDay()`); month days are
  1..=31. One of:
  - `"Daily"` (string) — fires every day at this time.
  - `{ "Weekly": { "days": [u8] } }` — fires at this time on each listed
    weekday.
  - `{ "Monthly": { "days": [u8] } }` — fires at this time on each listed
    day of the month.
  - `{ "Once": { "year": u16, "month": u8, "day": u8 } }` — fires once on
    the specified calendar date (after which the firmware may discard it).
- **`enabled`** (bool): Whether this alarm is currently active.
- **`label`** (string): Human-readable name or description (may be empty).

**`todos`** (array): List of todos to store on the device. May be empty.

Each todo object:
- **`id`** (u8): Unique identifier for this todo; must not conflict within the
  list.
- **`text`** (string): The todo's description or task text (may be empty, though
  that's not useful).
- **`done`** (bool): Whether this todo is marked complete.
- **`importance`** (enum, optional, default `"medium"`): One of `"low"`,
  `"medium"`, or `"high"`. The device cycles it via long-ENTER on the Todos
  page and uploads it back; the calendar page sizes the due marker by it, and
  a `high` todo due today triggers a once-per-day on-device reminder.
- **`due_date`** (object or null, optional, default `null`): `{ "year": u16,
  "month": u8, "day": u8 }` — the concrete date the todo is due (used when
  `repeat` is `null`). The device calendar draws a marker on that date;
  `high`-importance todos due today trigger the reminder screen described
  above. (The `year` field is optional on read for backward compatibility;
  a missing year never matches a real date, so such todos simply don't
  mark the calendar or remind until re-edited.)
- **`repeat`** (enum or null, optional, default `null`): Same shape as an
  alarm's `repeat` (`"Daily"`, `{"Weekly": ...}`, `{"Monthly": ...}`), but
  `Once` is not meaningful for a todo - use `due_date` for one-off due
  dates. When set, the todo is due on every date the schedule covers, and
  the calendar/reminder logic uses the schedule instead of `due_date`.

**`inbox`** (array, optional, default `[]`): Inbox notifications pushed to
the device from external sources (webhooks, CI, agents) via the server.
Capped at 20 items per response; `inbox_truncated` is `true` when the server
has more.

Each inbox item:
- **`id`** (u64): Device-visible stable id (the server's per-device `seq`,
  monotonic). Used for dedup and read-ack.
- **`kind`** (enum): `"alert"` | `"event"` | `"info"`.
- **`priority`** (enum, optional, default `"normal"`): `"normal"` | `"high"`.
  `high` messages are urgent: the device shows a full-screen "URGENT"
  reminder with a persistent tone as soon as they arrive.
- **`title`** (string): Short message title.
- **`body`** (string, optional): Longer detail.
- **`when`** (i64 or null, optional): Unix epoch the message relates to.
- **`read`** (bool): Whether the device has marked it read.

**`inbox_read_acked`** (array, optional): The `seq`s the device uploaded as
read that the server acknowledged; the firmware drops these from its local
pending-read set.

**`inbox_truncated`** (bool, optional): `true` when the server has more inbox
items than it could fit in this response.

#### Lightweight urgent poll

The device may include an `X-Inkwash-Poll: 1` header on `POST /api/sync`
(with an empty `{}` body). The server answers **immediately** with a tiny
`{"urgent": true|false}` response - it does not hold the connection, does not
merge device state, and does not return the full payload. The firmware calls
this on a short timer (e.g. every 30 s) to detect high-priority messages
without keeping a long connection open or blocking its main loop. When
`urgent` is `true`, the device performs a normal full `POST /api/sync` to pull
the message down and show the urgent reminder.

#### ETag Header (optional)

The server includes an **ETag** header on the response, and the firmware
caches its value in NVS. Current (POST-based) firmware does **not** send it
back as `If-None-Match` - every sync uploads state and gets a full 200
response back, so there is no conditional-request round trip on this path.
The cached value is kept mainly for diagnostics and for compatibility with
the legacy GET flow described below.

`GET /api/sync` and conditional HTTP 304 remain available for older firmware
that still sends `If-None-Match`, but current firmware always uses `POST` so
local completion/enabled changes are never discarded before upload.

### Other Status Codes

Any other response (4xx, 5xx, etc.) is treated as an error. The firmware logs
the status code and displays an error message to the user; the sync is aborted
and the device's local data remains unchanged.

## Error Handling

Errors that prevent the sync request from completing (network timeout, TLS
handshake failure, invalid server URL, malformed JSON response, etc.) are logged
and reported to the user via the on-device menu but do not disrupt the device's
normal operation. The locally-stored alarms and todos remain valid and will
continue to ring/display as scheduled.

## Timing and Scheduling

Sync cadence is driven by the device itself on wall-clock boundaries (the
main loop's `SyncScheduler`, `rust-firmware/src/main.rs`):

- A **lightweight urgent poll** (`X-Inkwash-Poll: 1`) fires at each
  `:00`/`:30` boundary — every 30 s — to check for high-priority messages
  without pulling the full payload.
- A **full sync** fires at each `interval` boundary (the top of the hour
  for a 1 h interval, the `:05` marks for 5 min, and so on): the device
  uploads its locally-changed flags and applies the server's authoritative
  lists.
- Boundaries are computed from the RTC's wall clock, not a boot-relative
  timer, so they never drift; the PCF8563 is resynced over NTP once a day
  to keep them aligned. A failed sync is retried on the next boundary.
- A manual `sync_now` from the on-device menu (or over USB/BLE) still
  triggers an immediate full sync.

## Security

- The server URL must be HTTPS (http:// URLs are not validated by the firmware,
  but the TLS certificate bundle is built into the ESP32-S3 firmware image and
  verifies the server's certificate against public CAs).
- The auth token is a bearer token; it should be a long, random string
  (e.g. 32+ characters) and must be kept secret.
- The sync request and response traverse Wi-Fi; only start syncing on a trusted
  network.
