#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
import re
import sys
import time

try:
    import serial
    from serial import SerialException
except ImportError:
    sys.stderr.write("pyserial is required: python3 -m pip install pyserial\n")
    raise SystemExit(2)


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", default="/dev/cu.usbmodem1101")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--soak", type=float, default=60.0)
    parser.add_argument("--stress", type=int, default=20)
    parser.add_argument("--reply-timeout", type=float, default=20.0)
    parser.add_argument("--log-file", help="write the complete raw serial log to this path")
    return parser.parse_args()


class Link:
    def __init__(self, port, baud):
        self.serial = serial.Serial(
            port=None, baudrate=baud, timeout=0.05, write_timeout=2.0)
        self.serial.dtr = False
        self.serial.rts = False
        self.serial.port = port
        self.serial.open()
        time.sleep(0.3)
        self.serial.reset_input_buffer()
        self.log = []
        self.pending = bytearray()

    def close(self):
        if self.serial.is_open:
            self.serial.close()

    def _pump(self):
        chunk = self.serial.read(512)
        if not chunk:
            return []
        self.pending.extend(chunk)
        out = []
        while b"\n" in self.pending:
            raw, _, self.pending = self.pending.partition(b"\n")
            text = raw.rstrip(b"\r").decode("utf-8", "replace")
            self.log.append(text)
            out.append(text)
        return out

    def request(self, payload, wait):
        start = time.monotonic()
        self.serial.write((">>IW " + json.dumps(
            payload, separators=(",", ":")) + "\n").encode())
        self.serial.flush()
        want = payload.get("id")
        while time.monotonic() - start < wait:
            for text in self._pump():
                if text.startswith("<<IW "):
                    try:
                        reply = json.loads(text[5:])
                    except ValueError:
                        continue
                    if want is None or reply.get("id") == want:
                        return reply, time.monotonic() - start
        return None, time.monotonic() - start

    def drain(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self._pump()

    def request_until_ready(self, payload, wait):
        deadline = time.monotonic() + wait
        while time.monotonic() < deadline:
            reply, _ = self.request(payload, min(5.0, deadline - time.monotonic()))
            if not reply or reply.get("status") != "busy":
                return reply
            time.sleep(0.1)
        return None


TROUBLE = [
    "Task watchdog got triggered", "Guru Meditation", "abort()",
    "assert failed", "Stack canary", "panic", "rst:0x",
    "StoreProhibited", "LoadProhibited", "Cache disabled",
]


def main():
    args = parse_args()
    results = []
    link = Link(args.port, args.baud)
    link_error = None
    try:
        status = None
        ready_deadline = time.monotonic() + 30
        while not status and time.monotonic() < ready_deadline:
            status, _ = link.request(
                {"cmd": "get_status", "id": "smoke-status"}, 3)
        if not status:
            print("FAIL  get_status: no reply")
            return 1
        print("status:", json.dumps(status, ensure_ascii=False))
        results.append(("get_status replies", status.get("status") == "status"))

        original = status.get("timezone_offset_minutes")

        target = 60 if original != 60 else -300
        reply, _ = link.request(
            {"cmd": "set_timezone", "offset_minutes": target, "id": "smoke-tz"}, 20)
        results.append(("set_timezone accepted", bool(reply) and reply.get("status") == "ok"))

        replay, _ = link.request(
            {"cmd": "set_timezone", "offset_minutes": target, "id": "smoke-tz"}, 20)
        results.append(("same id replays the cached reply",
                        bool(replay) and replay.get("status") == "ok"))

        after, _ = link.request({"cmd": "get_status", "id": "smoke-after"}, 8)
        results.append(("timezone applied", bool(after)
                        and after.get("timezone_offset_minutes") == target))

        bad, _ = link.request(
            {"cmd": "set_timezone", "offset_minutes": 9999, "id": "smoke-badtz"}, 20)
        results.append(("out-of-range timezone rejected",
                        bool(bad) and bad.get("status") == "error"))

        bad, _ = link.request(
            {"cmd": "set_rtc", "epoch_secs": 1, "id": "smoke-badrtc"}, 20)
        results.append(("out-of-range rtc rejected",
                        bool(bad) and bad.get("status") == "error"))

        writes_ok = 0
        for index in range(args.stress):
            value = 480 if index % 2 == 0 else 60
            reply = link.request_until_ready(
                {"cmd": "set_timezone", "offset_minutes": value,
                 "id": "smoke-stress-%d" % index}, args.reply_timeout)
            if reply and reply.get("status") == "ok":
                writes_ok += 1
        print("stress writes: %d/%d" % (writes_ok, args.stress))
        results.append(("persistence writes complete", writes_ok == args.stress))

        link.drain(args.soak)
    except SerialException as err:
        link_error = str(err)
        print("FAIL  serial link:", link_error)
    finally:
        link.close()

    lines = link.log
    if link_error:
        time.sleep(1.0)
        try:
            recovery = Link(args.port, args.baud)
            try:
                recovery.drain(10.0)
                lines.extend(recovery.log)
            finally:
                recovery.close()
        except SerialException as err:
            lines.append("host recovery open failed: %s" % err)
    if args.log_file:
        Path(args.log_file).write_text("\n".join(lines) + "\n", encoding="utf-8")
        print("serial log:", args.log_file)
    trouble = [l for l in lines for pat in TROUBLE if pat.lower() in l.lower()]
    results.append(("no panic / watchdog / reset during soak", not trouble))
    results.append(("serial link stayed connected", link_error is None))
    for line in trouble[:5]:
        print("  trouble:", line)

    uptimes = [int(m.group(1)) for m in
               (re.search(r"^[IWE] \((\d+)\)", l) for l in lines) if m]
    if uptimes:
        print("uptime first/last: %d / %d ms" % (uptimes[0], uptimes[-1]))
        results.append(("uptime monotonic", uptimes[-1] >= uptimes[0]))

    refreshes = [l for l in lines if "EPD refresh completed" in l]
    full = [l for l in refreshes if "Full" in l]
    print("epd refreshes: %d (full %d, partial %d)"
          % (len(refreshes), len(full), len(refreshes) - len(full)))

    if original is not None:
        restore = Link(args.port, args.baud)
        try:
            restored, _ = restore.request(
                {"cmd": "set_timezone", "offset_minutes": original,
                 "id": "smoke-restore"}, 20)
            results.append(("timezone restored to original",
                            bool(restored) and restored.get("status") == "ok"))
        finally:
            restore.close()

    print("\n=== smoke results ===")
    failed = 0
    for name, ok in results:
        print(("PASS  " if ok else "FAIL  ") + name)
        failed += 0 if ok else 1
    print("%d/%d passed" % (len(results) - failed, len(results)))
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
