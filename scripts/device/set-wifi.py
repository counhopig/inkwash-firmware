#!/usr/bin/env python3
"""Sends Wi-Fi credentials to a Note 4 over the USB serial console.

Usage:
    python scripts/device/set-wifi.py --port COM5 --ssid MyNetwork
    python scripts/device/set-wifi.py --port /dev/ttyACM0 --ssid MyNetwork --password secret

Without --password the script prompts for it, so it stays out of the shell
history. The device tries the credentials first and stores them only if it
can join the network; the script prints the device's reply. Close any serial
monitor holding the port first. Needs pyserial (the ESP-IDF Python ships it).
"""
import argparse
import getpass
import json
import sys
import time

import serial

VERIFY_TIMEOUT_SECS = 60.0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", required=True)
    parser.add_argument("--ssid", required=True)
    parser.add_argument("--password")
    args = parser.parse_args()
    password = args.password
    if password is None:
        password = getpass.getpass(f"Password for {args.ssid}: ")

    link = serial.Serial(port=None, baudrate=115200, timeout=0.05, write_timeout=2.0)
    # Keep DTR/RTS low: on the USB Serial/JTAG port they reset the chip.
    link.dtr = False
    link.rts = False
    link.port = args.port
    link.open()
    time.sleep(0.3)
    link.reset_input_buffer()

    request = {"cmd": "set_wifi", "ssid": args.ssid, "password": password, "id": "wifi"}
    link.write((">>IW " + json.dumps(request, separators=(",", ":")) + "\n").encode())
    link.flush()
    print(f"Sent set_wifi for '{args.ssid}'; waiting up to {VERIFY_TIMEOUT_SECS:.0f} s "
          "while the device joins the network...")

    pending = bytearray()
    deadline = time.monotonic() + VERIFY_TIMEOUT_SECS
    while time.monotonic() < deadline:
        pending.extend(link.read(512))
        while b"\n" in pending:
            raw, _, pending = pending.partition(b"\n")
            text = raw.rstrip(b"\r").decode("utf-8", "replace")
            if not text.startswith("<<IW "):
                continue
            try:
                reply = json.loads(text[5:])
            except ValueError:
                continue
            if reply.get("id") != "wifi":
                continue
            status = reply.get("status")
            if status == "pending":
                continue
            if status == "busy":
                print("Device is busy; try again in a few seconds.")
                return 1
            if status == "ok":
                print("OK: the device joined the network and saved the credentials.")
                return 0
            print("Device reply:", json.dumps(reply, ensure_ascii=False))
            return 1
    print("No reply from the device. Check the port and that no monitor holds it.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
