#!/usr/bin/env python3
"""Export persistent Note 4 event logs over USB to JSONL (requires pyserial)."""
import argparse
import json
from pathlib import Path
import sys
import time

import serial


def request(link, command, pending):
    wire = (">>IW " + json.dumps(command, separators=(",", ":")) + "\n").encode()
    # Retry the identical cursor: exporting never deletes or advances device state.
    for _ in range(3):
        link.write(wire)
        link.flush()
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            pending.extend(link.read(1024))
            while b"\n" in pending:
                raw, _, remainder = pending.partition(b"\n")
                pending[:] = remainder
                start = raw.find(b"<<IW ")
                if start < 0:
                    continue
                try:
                    reply = json.loads(raw[start + 5:])
                except (ValueError, UnicodeDecodeError):
                    continue
                if reply.get("id") != command["id"]:
                    continue
                if reply.get("status") == "busy":
                    time.sleep(0.5)
                    break
                if reply.get("status") != "ok":
                    raise RuntimeError(reply.get("message", "Device rejected log export"))
                return reply
    raise TimeoutError("No valid event-log reply after three attempts")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    # Exclusive creation protects earlier exports. A partial export remains
    # readable on interruption; only an export_end row marks completion.
    with args.output.open("x", encoding="utf-8") as output:
        link = serial.Serial(port=None, baudrate=115200, timeout=0.1, write_timeout=2)
        link.dtr = False
        link.rts = False
        link.port = args.port
        link.open()
        try:
            time.sleep(0.3)
            pending = bytearray()
            command = {"cmd": "get_logs", "id": "logs-0"}
            page = request(link, command, pending)
            anchor, snapshot, capacity = page["anchor"], page["snapshot"], page["capacity"]
            metadata = {k: v for k, v in page.items() if k not in ("entries", "id", "cursor", "done", "status")}
            output.write(json.dumps({"type": "export_start", **metadata}, ensure_ascii=False) + "\n")
            total = 0
            last_sequence = 0
            cursor = 0
            while True:
                if page["anchor"] != anchor or page["snapshot"] != snapshot or page["capacity"] != capacity:
                    raise RuntimeError("Device log snapshot changed during export")
                next_cursor = page["cursor"]
                if not cursor <= next_cursor <= capacity or (not page["done"] and next_cursor <= cursor):
                    raise RuntimeError("Device returned an invalid log cursor")
                for entry in page["entries"]:
                    sequence = int(entry["sequence"])
                    if sequence <= last_sequence or sequence > int(snapshot):
                        raise RuntimeError("Logs changed during export; reconnect and export again")
                    output.write(json.dumps({"type": "event", **entry}, ensure_ascii=False) + "\n")
                    last_sequence = sequence
                    total += 1
                output.flush()
                if page["done"]:
                    if next_cursor != capacity:
                        raise RuntimeError("Device ended log export prematurely")
                    output.write(json.dumps({"type": "export_end", "records": total, "snapshot": snapshot,
                                             "invalid_records": page["invalid_records"],
                                             "dropped_this_boot": page["dropped_this_boot"],
                                             "io_errors_this_boot": page["io_errors_this_boot"]}) + "\n")
                    break
                cursor = next_cursor
                command = {"cmd": "get_logs", "id": f"logs-{cursor}", "cursor": cursor,
                           "anchor": anchor, "snapshot": snapshot}
                page = request(link, command, pending)
            print(f"Exported {total} records to {args.output}")
        finally:
            link.close()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, TimeoutError, serial.SerialException) as error:
        print(f"Export failed: {error}", file=sys.stderr)
        sys.exit(1)
