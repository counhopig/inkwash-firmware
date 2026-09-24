#!/usr/bin/env bash
# Flashes a Zectrix Note 4 only after proving the port holds that exact board.
#
# Usage:
#   scripts/flash-note4.sh --port /dev/ttyACM0 [--monitor] [path/to/inkwash-note4]
#
# Also the cargo runner (rust-firmware/.cargo/config.toml), which passes the
# ELF path; set INKWASH_NOTE4_PORT (or ESPFLASH_PORT) to choose the port.
#
# A serial-port name is not an identity: another ESP32, or a Note 4C with a
# different panel, may enumerate on the same name. Before writing anything this
# script requires an ESP32-S3 with 16 MB flash whose MAC is the authorized
# Note 4 (INKWASH_NOTE4_MAC, default below), then flashes it as 16 MB DIO
# 80 MHz with this repository's partition table.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
authorized_mac="${INKWASH_NOTE4_MAC:-20:6E:F1:B4:7D:E4}"
port="${INKWASH_NOTE4_PORT:-${ESPFLASH_PORT:-}}"
monitor=()
elf=""

while [ $# -gt 0 ]; do
    case "$1" in
        --port)
            port="${2:?--port needs a value}"
            shift 2
            ;;
        --monitor)
            monitor=(--monitor)
            shift
            ;;
        -h | --help)
            sed -n '2,14p' "$0"
            exit 0
            ;;
        *)
            elf="$1"
            shift
            ;;
    esac
done

elf="${elf:-$repo/rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4}"
bootloader="$(dirname "$elf")/bootloader.bin"
partitions="$repo/rust-firmware/partitions.csv"

if [ -z "$port" ]; then
    echo "Refusing to flash: name the port with --port or INKWASH_NOTE4_PORT." >&2
    exit 1
fi
for file in "$elf" "$bootloader" "$partitions"; do
    if [ ! -f "$file" ]; then
        echo "Refusing to flash: $file not found (build with scripts/build-rust.sh --release)." >&2
        exit 1
    fi
done
if [[ ! "$authorized_mac" =~ ^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$ ]]; then
    echo "Refusing to flash: INKWASH_NOTE4_MAC '$authorized_mac' is not a MAC address." >&2
    exit 1
fi

if command -v esptool.py >/dev/null 2>&1; then
    esptool=(esptool.py)
elif command -v esptool >/dev/null 2>&1; then
    esptool=(esptool)
elif python3 -c 'import esptool' >/dev/null 2>&1; then
    esptool=(python3 -m esptool)
else
    echo "Refusing to flash: esptool is required to identify the board." >&2
    exit 1
fi
command -v espflash >/dev/null 2>&1 || {
    echo "Refusing to flash: espflash is required." >&2
    exit 1
}

probe() {
    local output
    if ! output="$("${esptool[@]}" --chip esp32s3 --port "$port" "$@" 2>&1)"; then
        printf '%s\n' "$output" >&2
        echo "Refusing to flash: esptool could not probe $port." >&2
        exit 1
    fi
    printf '%s\n' "$output"
}

identity="$(probe read_mac)"
# esptool 4 prints "Chip is ESP32-S3 ...", esptool 5 "Chip type: ESP32-S3 ...".
if ! grep -Eiq '^(Chip is|Chip type:)[[:space:]]*ESP32-S3\b' <<<"$identity"; then
    echo "Refusing to flash: $port is not identified as an ESP32-S3." >&2
    exit 1
fi
mac="$(grep -Eio '^MAC:[[:space:]]*([0-9a-f]{2}:){5}[0-9a-f]{2}' <<<"$identity" | head -n 1 | awk '{print $NF}')"
if [ "${mac,,}" != "${authorized_mac,,}" ]; then
    echo "Refusing to flash: $port has MAC '${mac:-unknown}', not the authorized Note 4 $authorized_mac." >&2
    exit 1
fi
if ! grep -Eiq '^Detected flash size:[[:space:]]*16MB[[:space:]]*$' <<<"$(probe flash_id)"; then
    echo "Refusing to flash: $port is not identified as a 16 MB flash device." >&2
    exit 1
fi

echo "==> $port is the authorized Note 4 ($mac); flashing $elf"
espflash flash --port "$port" --chip esp32s3 \
    --flash-size 16mb --flash-mode dio --flash-freq 80mhz \
    --bootloader "$bootloader" \
    --partition-table "$partitions" --partition-table-offset 0x10000 \
    --non-interactive "${monitor[@]}" \
    "$elf"
