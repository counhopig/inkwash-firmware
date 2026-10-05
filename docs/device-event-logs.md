# Offline Device Event Logs

Note 4 stores key runtime events in the Flash `eventlog` partition (`0x4B0000`, 1 MiB). Records survive deep sleep, resets, and complete power loss. The partition holds 8192 records of 128 bytes each. It reclaims 4 KiB sectors in a circular buffer, removing up to 32 of the oldest records per sector. Retention time depends on event frequency.

## Recorded Events

- Boot: firmware Git description, ESP-IDF reset reason, wake cause, external wake pin mask, and consecutive failed boot count.
- Sleep: sleep type, planned timer wake interval, wake pin mask, and work that keeps the device awake.
- Power: battery percentage, USB power, charging state, and available heap. Samples are taken every 15 minutes during normal operation and when the power state changes. The low battery threshold is 10%.
- Network: job type, start and completion, duration, retry deadline, connection timeout, HTTP status or system error, and sync decoding or storage failures.
- Alarms and reminders: RTC alarm configuration, due alarm ID, ringing and dismissal, reminder count, and automatic timeout.
- Faults: RTC, display, audio, and NVS write failures; rejected application events; safe mode; and crash summaries with up to 16 backtrace addresses.
- Input and control: key and screen IDs, control command type and channel, and BLE link events.

Logs exclude Wi-Fi credentials, server URLs and tokens, notification content, and raw control commands. `net_done` records the success flag; separate diagnostic events identify the failure stage.

Each record includes an increasing sequence number, boot ID, uptime in milliseconds for that boot, UTC seconds, and time quality:

| time_quality | Meaning |
| --- | --- |
| `rtc` | Time comes from the RTC or has been set through NTP or USB |
| `estimated` | Time is estimated from the build timestamp after RTC power loss |
| `unknown` | No usable clock is available; `utc_secs` is 0 |

Early boot records may lack an absolute timestamp. Use the `clock` event from the same boot and `uptime_ms` to reconstruct their timing. Sequence numbers, boot IDs, and timestamps are transmitted as decimal strings.

Multiple bits in `sleep_held mask` may be set:

| Mask | Work blocking sleep |
| --- | --- |
| `0x001` | USB host connection |
| `0x002` | Network job |
| `0x004` | Ringing alarm |
| `0x008` | Reminder screen |
| `0x010` | BLE pairing or activity |
| `0x020` | Pending command reply |
| `0x040` | Audio work |
| `0x080` | Display recovery |
| `0x100` | Pending events |
| `0x200` | Held key |

Up to 16 ordinary events are buffered in RAM. They are written to Flash when the buffer fills, at the 30-second flush interval, or before deep sleep. Critical events are flushed immediately. The commit marker is written last, and CRC validation rejects incomplete records. Recovery skips damaged records and unfinished tail writes. Sudden power loss may discard events that have not been flushed. Log storage failures do not prevent normal sleep or device operation.

## USB Export

Use Python with pyserial installed. Close any serial monitor using the port, then run:

```sh
python scripts/device/export-logs.py --port /dev/cu.usbmodemXXXX --output logs/battery-session.jsonl
```

On Windows, use the device's actual port, such as `COM5`. The output file must not already exist, and its parent directory must exist. The exporter sets DTR and RTS low before opening the port. USB reconnection may still reset the hardware; historical records remain in Flash.

The output is JSONL: an `export_start` metadata row, `event` rows, and an `export_end` row. The final completion marker is required for a complete export. Records already written remain readable if the export is interrupted. Reading does not clear device logs.

USB command frame:

```text
>>IW {"cmd":"get_logs","id":"logs-0"}
```

Replies use the `<<IW ` prefix and include `entries` (up to 8 records per page), `anchor`, `snapshot`, `cursor`, `capacity`, and `done`. Subsequent requests return the anchor, snapshot, and cursor exactly as received:

```text
>>IW {"cmd":"get_logs","id":"logs-8","anchor":0,"snapshot":"123","cursor":8}
```

The same request can be retried. One reader is supported per export snapshot; starting another export replaces the snapshot. A device reset or sector reclamation during export invalidates continuation requests and requires a new export. Safe mode also supports reading logs. BLE does not support log export.

Metadata field `invalid_records` counts damaged or incomplete records found during this boot's scan. `dropped_this_boot` and `io_errors_this_boot` count buffer drops and I/O failures during the current boot.

## Storage Boundaries

The event log partition occupies space from the reserved region. NVS, application, and coredump partition addresses and sizes remain fixed. Initial installation requires both the partition table and application to be flashed using the repository's `flash-note4` script. A full Flash erase deletes historical logs. Normal flashing of the bootloader, partition table, and application does not overwrite the event log partition.

The boot process reclaims a crash dump after its summary and backtrace have been persisted in the event log. The dump is retained if summary extraction or log persistence fails.
