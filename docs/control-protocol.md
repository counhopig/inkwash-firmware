# Control Protocol (USB/BLE)

This document specifies the command protocol for configuring the inkwash firmware
over USB serial or BLE control channels. Transport-specific details (USB framing,
BLE characteristics) are handled by the transport layer; this document describes
the command/reply schema that both transports implement.

## Transport-Agnostic Schema

### Commands

Commands are sent as JSON objects, one per line. Each command has a `cmd` field
identifying the operation, plus command-specific fields.

#### `{"cmd":"set_wifi","ssid":"NETWORK_NAME","password":"PASSWORD"}`

Configure Wi-Fi credentials and verify the connection before saving.

**Fields:**
- **`cmd`** (string, required): `"set_wifi"`
- **`ssid`** (string, required): The Wi-Fi network name (SSID), typically 1–32 characters.
- **`password`** (string, required): The WPA2 passphrase, typically 8–63 characters.

**Behavior:**
- Attempts to connect to the specified network to verify the credentials work.
- Saves credentials to persistent storage (NVS) only if the connection succeeds.
- Disconnects after verification (does not hold a persistent connection).

**Reply:** `Ok` on success, or `Error { message }` if verification failed.

---

#### `{"cmd":"set_server","url":"HTTPS_URL","token":"AUTH_TOKEN"}`

Configure the server URL and authentication token for syncing alarms and todos.

**Fields:**
- **`cmd`** (string, required): `"set_server"`
- **`url`** (string, required): The HTTPS endpoint to fetch alarms/todos from
  (e.g., `https://example.com/api/sync`).
- **`token`** (string, required): Bearer token for authenticating to the server
  (e.g., a 32+ character random string).

**Behavior:**
- Saves the URL and token to persistent storage immediately.
- Does NOT verify the server is reachable (would require network activity;
  validation is deferred to `sync_now`).

**Reply:** `Ok` on success, or `Error { message }` if NVS save fails.

---

#### `{"cmd":"sync_now"}`

Fetch alarms and todos from the configured server and apply them to local stores.

**Fields:**
- **`cmd`** (string, required): `"sync_now"`
- (No other fields.)

**Behavior:**
- Loads server config from storage (URL + token). Checks that system time is
  available (needed to re-arm the RTC alarm after applying synced data).
- Connects Wi-Fi via the process's shared `WifiManager`, then performs the
  bidirectional HTTPS POST described in
  [`sync-api.md`](sync-api.md): uploads the device's local alarm `enabled` /
  todo `done` flags, and applies the server's merged, authoritative alarm/todo
  lists from the response body. Text, schedules, additions, and deletions are
  never uploaded by the device.
- On HTTP 200, parses the response JSON, saves alarms/todos to local stores,
  and re-arms the PCF8563 hardware alarm slot. Caches any returned ETag
  (currently informational only - the POST request does not send
  `If-None-Match`, so every sync gets a full response).
- On HTTP error or network failure, returns an error; local data is left
  unchanged.
- Disconnects Wi-Fi again once the request completes (success or failure).

**Reply:** `Ok` on success, or `Error { message }` on failure.

---

#### `{"cmd":"get_status"}`

Query the device's current configuration state.

**Fields:**
- **`cmd`** (string, required): `"get_status"`
- (No other fields.)

**Behavior:**
- Returns a snapshot of whether Wi-Fi and server credentials are configured,
  the stored SSID / server URL / timezone, and live Wi-Fi connection state.
- Secrets (Wi-Fi password, server auth token) are never sent back - only
  booleans indicating whether one is set.

**Reply:** `Status { wifi_configured, server_configured, wifi_connected,
wifi_ssid, wifi_has_password, server_url, server_has_token,
timezone_offset_minutes }` (see Replies below).

---

#### `{"cmd":"clear_alarms"}`

Delete every locally stored alarm and disarm the PCF8563 hardware alarm slot.
Wi-Fi credentials, server configuration, and todos are preserved.

**Reply:** `Ok` on success, or `Error { message }` on failure.

---

#### `{"cmd":"set_timezone","offset_minutes":480}`

Persist a fixed local UTC offset and immediately shift the hardware RTC by the
difference from the previous offset. Valid range: -720 (UTC-12:00) through 840
(UTC+14:00). NTP synchronization applies the stored offset before writing RTC.

**Reply:** `Ok` on success, or `Error { message }` on failure.

---

#### `{"cmd":"set_rtc","epoch_secs":1756000000}`

Write a trusted Unix timestamp to the PCF8563 hardware RTC. The stored local
UTC offset is applied before writing the local clock. This command is intended
for host-assisted recovery when the device can reach Wi-Fi but NTP/UDP is not
available; it does not require server configuration.

**Reply:** `Ok` on success, or `Error { message }` on failure.

---

### Request Correlation

Any command may include an optional `id` field (any JSON string), e.g.
`{"cmd":"get_status","id":"req-42"}`. If present, the device echoes it back
as a top-level `id` field on the reply: `{"status":"status","id":"req-42",...}`.
If a command omits `id`, its reply has no `id` field either - existing
clients that never send one see no wire-format change.

A client sending more than one command without waiting for each reply has no
other way to tell which reply answers which request (see Limitations below),
especially once the `busy` reply exists - a reminder screen can make several
replies arrive close together. Set `id` and match on it rather than assuming
replies arrive in the order requests were sent.

**Resending is safe and cheap when `id` is set.** If a client resends the
same command (same `id`, same body) because it hasn't seen a reply yet, the
device recognizes the exact `(id, command)` pair within the current USB or
BLE connection session and replays the terminal reply instead of re-running
the command. A deferred command keeps its correlation key until the terminal
frame is accepted by its transport. For USB, acceptance means the framed
bytes were written and flushed. For BLE, acceptance means the NimBLE
notify-tx callback reports `SuccessNotify` for the same connection
generation. A notify-tx status failure is retried while that connection
remains valid. An immediate `notify_with` error, a timeout, or a disconnect
terminates that transport delivery and clears the pending command; the client
may resend an id-tagged command in the same connection. `busy` is never cached and
never replaces an existing deferred command. Reconnecting starts a new
transport session, so an id reused by a new client is not matched to an old
reply. Without a matching `id` on both the original and the resend, the
device has no way to tell a resend apart from a genuinely new,
identically-shaped command, and will execute it again - this matters for
anything with a real side effect (`set_wifi`, `sync_now`), not just for
saving battery.

### Replies

Replies are sent as JSON objects, one per line. Each reply has a `status` field
indicating success or failure, plus status-specific fields, plus `id` if the
triggering command included one (see Request Correlation above).

#### `{"status":"ok"}`

Command succeeded. No additional data.

---

#### `{"status":"busy"}`

A command arrived over USB while a full-screen due-todo or urgent-inbox
reminder was actively ringing. The command was **not executed** — it is
dropped, not queued. Retry after the reminder is dismissed (ENTER) or times
out (120 s for urgent inbox reminders; due-todo reminders wait for ENTER
indefinitely). This distinguishes "device is temporarily unavailable" from
silence, so a client doesn't have to guess whether a request was lost.

Only the due-todo and urgent-inbox reminder screens (`reminders.rs`) return
this; commands are still silently deferred (no reply at all until the
command is later drained) while the RTC alarm-ringing screen (`alarms.rs`)
or a BLE-only menu screen is showing — see Limitations below.

**Fields:**
- **`status`** (string, required): `"busy"`

---

#### `{"status":"error","message":"ERROR_DESCRIPTION"}`

Command failed. The `message` field contains a human-readable description of
the failure (e.g., "Connection verification failed: timeout").

**Fields:**
- **`status`** (string, required): `"error"`
- **`message`** (string, required): Description of the error.

---

#### `{"status":"status","wifi_configured":BOOL,"server_configured":BOOL,"wifi_connected":BOOL,"wifi_ssid":STR|NULL,"wifi_has_password":BOOL,"server_url":STR|NULL,"server_has_token":BOOL,"timezone_offset_minutes":INT}`

Current device configuration (in reply to `get_status`).

**Fields:**
- **`status`** (string, required): `"status"`
- **`wifi_configured`** (bool, required): `true` if Wi-Fi credentials are stored in NVS.
- **`server_configured`** (bool, required): `true` if server URL + token are stored in NVS.
- **`wifi_connected`** (bool, required): `true` if currently connected to a Wi-Fi network
  (live STA link state).
- **`wifi_ssid`** (string or null, required): stored SSID, or `null` if not configured.
- **`wifi_has_password`** (bool, required): whether a password is stored for the SSID.
  The password itself is never transmitted.
- **`server_url`** (string or null, required): stored server sync URL, or `null` if not
  configured.
- **`server_has_token`** (bool, required): whether an auth token is stored. The token
  itself is never transmitted.
- **`timezone_offset_minutes`** (int, required): stored UTC offset in minutes (`0` if unset).

---

## USB Serial Framing (Phase 4)

The USB serial channel shares the same physical port as firmware log output
(`CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG`). To distinguish control frames from
ordinary `log::info!`/`log::warn!` messages, each is prefixed with a sentinel:

**Command frame (PC → device):**
```
>>IW <JSON_COMMAND>\n
```

**Reply frame (device → PC):**
```
<<IW <JSON_REPLY>\n
```

The `>>IW ` and `<<IW ` prefixes (with trailing space) are literal and required.
Any line not starting with `>>IW ` is treated as log output and ignored by the
reader.

### Example USB Session

```
# Device boots and logs
[INFO] Inkwash NOTE4 Rust bring-up starting
[INFO] Power latch is high; rendering home screen

# PC sends a command (e.g., set Wi-Fi credentials)
>>IW {"cmd":"set_wifi","ssid":"<ssid>","password":"<password>"}

# Device logs internal operations
[INFO] USB control: Wi-Fi credentials saved for '<ssid>'

# Device sends reply
<<IW {"status":"ok"}

# PC sends another command
>>IW {"cmd":"sync_now"}

[INFO] USB control sync completed: 3 alarms, 5 todos
<<IW {"status":"ok"}

# PC queries device status
>>IW {"cmd":"get_status"}
<<IW {"status":"status","wifi_configured":true,"server_configured":true,"wifi_connected":false,"wifi_ssid":"MySSID","wifi_has_password":true,"server_url":"http://192.168.1.10:8080/api/sync","server_has_token":true,"timezone_offset_minutes":480}
```

---

## BLE Framing (Phase 5)

BLE control is implemented via a GATT service with two characteristics,
both carrying plain JSON with no additional framing (GATT writes and
notifications are already message-delimited at the link layer).

### Service and Characteristics

**Service UUID (UUID128):** `d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4`

**Command Characteristic (WRITE):**
- UUID (UUID128): `d2c25e51-5e22-48d8-a8b3-34f2f8e2c7d4`
- Properties: `WRITE`
- Payload: JSON command object (e.g., `{"cmd":"get_status"}`)
- Max size: 512 bytes

**Reply Characteristic (NOTIFY):**
- UUID (UUID128): `d2c25e52-5e22-48d8-a8b3-34f2f8e2c7d4`
- Properties: `READ | NOTIFY`
- Payload: JSON reply object (e.g., `{"status":"ok"}`)
- Max size: 512 bytes

### Example BLE Session

1. PC tool discovers the Inkwash service (`d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4`).
2. PC tool enables notifications on the reply characteristic.
3. PC tool writes a command to the command characteristic:
   ```
   {"cmd":"get_status"}
   ```
4. Once the pairing screen has been exited back to Home (see Limitations
   below - commands aren't dispatched while any menu screen is showing),
   the device processes the command and sends a reply via notification:
   ```
   {"status":"status","wifi_configured":true,"server_configured":true,"wifi_connected":false,"wifi_ssid":"MySSID","wifi_has_password":true,"server_url":"http://192.168.1.10:8080/api/sync","server_has_token":true,"timezone_offset_minutes":480}
   ```

### Lifecycle

- BLE is **not** active by default; it costs ~150KB RAM.
- User enters "BLE PAIRING" menu item from Home to start advertising.
- GATT service accepts one connected client at a time; a second connection is
  rejected so command writes, notify-tx callbacks, and retries stay bound to
  one connection handle.
- On screen exit (HOLD), advertising stops and the GATT service is torn down.
- The same `control::dispatch` logic handles both USB and BLE commands,
  so the command/reply contract is identical between transports.

The firmware tags each BLE command and reply internally with the pairing
session, connection generation, connection handle, and a private reply id.
The private reply id is used for retry bookkeeping even when the command has
no wire-level `id`; it is never added to the JSON payload. BLE notification
completion means NimBLE reported `SuccessNotify`. This is a protocol-stack
transmit result, not an application-level acknowledgement from the client.
Inbound command and reply queues are bounded: a full command queue rejects the
write, while a reply already accepted by the worker remains owned by its
bounded retry buffer until delivery, termination, or disconnect.

### Limitations (same as USB)

- Commands are dispatched from Home, content-page browsing, navigation and
  settings pickers, numeric alarm entry, inbox detail, and the BLE pairing
  screen. Deliberately time-critical alarm/reminder screens may defer control
  traffic until the user dismisses them.

---

## Error Handling

- **Malformed JSON:** Logged as a warning; command is dropped; no reply is sent.
- **Excessive nesting:** A frame nesting JSON containers deeper than 4 is
  rejected before parsing, logged as a warning, and dropped; no reply is sent.
  Every command here is a flat object, so no valid frame is affected. The limit
  exists because deserializer recursion depth follows the frame's *shape* rather
  than its length, and the parsing worker's stack is small; see
  `MAX_COMMAND_NESTING` in `logic/src/protocol.rs` for the measurements.
- **Unknown command field:** JSON parse error; handled as above.
- **Missing required field:** JSON parse error; handled as above.
- **Command execution failure:** Command completes but returns `Error { message }`.

---

## Security

- Commands traverse the USB serial port or BLE link unencrypted. Assume the
  device is physically accessible when these channels are used.
- Wi-Fi and server credentials are stored unencrypted in NVS. An attacker with
  physical access to the device can extract them.
- Bearer tokens should be long, random, and kept confidential (treat like a
  password).

---

## Timing and Scheduling

Commands execute synchronously during the main loop's 20 ms poll cycle. Most
commands complete within that cycle; `sync_now` may block for several seconds
(network latency). The main loop feeds the Task Watchdog Timer during and after
command execution, so a hung sync will eventually reboot the device.

---

## Limitations (Current Implementation)

- Six commands are implemented: `set_wifi`, `set_server`, `sync_now`,
  `get_status`, `clear_alarms`, and `set_timezone`.
- No rate limiting or command queueing; commands are dispatched as they arrive.
- The optional `id` (see Request Correlation above) only labels which reply
  answers which command; it does not make out-of-order replies impossible.
  The device still processes and replies to commands strictly in arrival
  order (one at a time, no internal queueing/reordering), so a client that
  keeps exactly one command in flight never needs `id` to disambiguate. Set
  it if you might have more than one in flight, or want to treat a reply
  that arrives after your own timeout as identifiably stale rather than
  guessing.
- The `busy` reply (see Replies above) only covers the due-todo and
  urgent-inbox reminder screens. The RTC alarm-ringing screen and BLE-only
  menu screens still silently defer USB/BLE commands until dispatch resumes
  - a client can't yet tell "device busy, will reply soon" apart from
  "device unreachable" in those cases.
- `UsbConsole` has no dedicated reader task. Home and the long-lived content
  pages and interactive pickers poll it non-blockingly. Input lines are capped
  at 512 bytes so a broken client cannot grow the firmware heap without bound.
- **Opening the serial port can itself trigger a spurious ENTER press.**
  This board's USB-Serial-JTAG auto-reset circuitry (the same one `espflash`
  uses to enter the bootloader) wires the DTR/RTS control lines to GPIO0 -
  which is also the ENTER button. A serial library that asserts DTR/RTS on
  open (many do, by default) will physically pull GPIO0 low, which the
  firmware can't distinguish from a real button press. This was observed
  directly during Phase 4 hardware testing: opening the port and asserting
  DTR+RTS caused the device to open its on-device menu, which then (per the
  point above) stopped responding to further commands until backed out of.
  A PC tool implementation should either avoid asserting DTR/RTS when
  opening the port, or account for the device possibly landing on a menu
  screen immediately after connecting.
