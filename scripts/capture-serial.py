#!/usr/bin/env python3
"""Capture NOTE4 serial logs without requiring an interactive terminal."""

from __future__ import annotations

import argparse
import glob
import re
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

try:
    import serial
except ImportError:
    sys.stderr.write("pyserial is required: python3 -m pip install pyserial\n")
    raise SystemExit(2)


DEFAULT_PATTERNS = ("/dev/cu.usbmodem*", "/dev/ttyACM*")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Capture timestamped NOTE4 USB serial output, reconnecting when "
            "the USB node is re-enumerated. DTR and RTS are released before open."
        )
    )
    parser.add_argument("--port", help="exact serial device; otherwise auto-detect")
    parser.add_argument("--duration", type=float, default=300, help="seconds to capture")
    parser.add_argument("--output", type=Path, help="also write the complete capture here")
    parser.add_argument(
        "--expect",
        action="append",
        default=[],
        metavar="REGEX",
        help="required regex; may be repeated, missing matches make exit status 1",
    )
    return parser.parse_args()


def available_port(exact: str | None) -> str | None:
    if exact:
        return exact if Path(exact).exists() else None
    matches: list[str] = []
    for pattern in DEFAULT_PATTERNS:
        matches.extend(glob.glob(pattern))
    return sorted(set(matches))[0] if matches else None


def timestamped(text: str) -> str:
    stamp = datetime.now(timezone.utc).isoformat(timespec="milliseconds")
    return f"{stamp} {text}"


def main() -> int:
    args = parse_args()
    if args.duration <= 0:
        raise SystemExit("--duration must be greater than zero")

    expected = [(value, re.compile(value)) for value in args.expect]
    matched = {value: False for value, _ in expected}
    deadline = time.monotonic() + args.duration
    byte_count = 0
    connection_count = 0
    pending = bytearray()
    output = args.output.open("w", encoding="utf-8") if args.output else None

    def emit(message: str) -> None:
        print(message, flush=True)
        if output:
            output.write(message + "\n")
            output.flush()
        for value, pattern in expected:
            if pattern.search(message):
                matched[value] = True

    try:
        while time.monotonic() < deadline:
            port_name = available_port(args.port)
            if port_name is None:
                time.sleep(0.25)
                continue

            port = serial.Serial(port=None, baudrate=115200, timeout=0.25)
            # Set inactive modem-control state before open to reduce host-side
            # DTR/RTS pulses. Opening USB Serial/JTAG may still reset NOTE4.
            port.dtr = False
            port.rts = False
            try:
                port.port = port_name
                port.open()
                connection_count += 1
                emit(timestamped(f"[capture] connected {port_name}"))

                while time.monotonic() < deadline:
                    chunk = port.read(512)
                    if not chunk:
                        continue
                    byte_count += len(chunk)
                    pending.extend(chunk)
                    while b"\n" in pending:
                        raw, _, pending = pending.partition(b"\n")
                        line = raw.rstrip(b"\r").decode("utf-8", errors="replace")
                        emit(timestamped(line))
            except (OSError, serial.SerialException) as exc:
                emit(timestamped(f"[capture] disconnected {port_name}: {exc}"))
                time.sleep(0.25)
            finally:
                if port.is_open:
                    port.close()
    finally:
        if pending:
            emit(timestamped(pending.decode("utf-8", errors="replace")))
        if output:
            output.close()

    sys.stderr.write(
        f"capture complete: connections={connection_count} bytes={byte_count}\n"
    )
    if connection_count == 0:
        sys.stderr.write("no matching serial device appeared during capture\n")
        return 2
    missing = [value for value, found in matched.items() if not found]
    if missing:
        sys.stderr.write("missing expected patterns: " + ", ".join(missing) + "\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
