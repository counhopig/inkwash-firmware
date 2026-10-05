#!/usr/bin/env bash
# Flashes a Zectrix Note 4 only after proving the port holds that exact board.
#
# Usage: scripts/device/flash-note4.sh --port PORT [--build-dir DIR] [--monitor]
#
# A serial-port name is not an identity: another ESP32, or a Note 4C with a
# different panel, may enumerate on the same name. Before writing anything this
# script requires an ESP32-S3 with 16 MB flash whose MAC is the authorized
# Note 4 (20:6E:F1:B4:7D:E4), then flashes it as 16 MB DIO
# 80 MHz with this repository's partition table.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
authorized_mac="20:6E:F1:B4:7D:E4"
port="${INKWASH_NOTE4_PORT:-}"
monitor=()
build_dir="${INKWASH_CPP_BUILD_DIR:-$repo/firmware/build}"

while [ $# -gt 0 ]; do
    case "$1" in
        --build-dir)
            build_dir="${2:?--build-dir needs a value}"
            shift 2
            ;;
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
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

partitions="$repo/firmware/partitions.csv"
required=("$build_dir/inkwash.bin" "$build_dir/bootloader/bootloader.bin"
          "$build_dir/partition_table/partition-table.bin" "$partitions")

if [ -z "$port" ]; then
    echo "Refusing to flash: name the port with --port or INKWASH_NOTE4_PORT." >&2
    exit 1
fi
for file in "${required[@]}"; do
    if [ ! -f "$file" ]; then
        echo "Refusing to flash: $file not found." >&2
        exit 1
    fi
done
if [[ ! "$authorized_mac" =~ ^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$ ]]; then
    echo "Refusing to flash: authorized MAC '$authorized_mac' is not a MAC address." >&2
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
if [ ! -f "${IDF_PATH:-}/components/partition_table/gen_esp32part.py" ]; then
    echo "Refusing to flash: export the ESP-IDF environment first." >&2
    exit 1
fi
python3 -c 'import hashlib,sys; sys.exit(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest() != sys.argv[2])' \
    "$partitions" "b3a31833dfa23b985b89821768fdf4cf5d3b4e23c52a815c551c79e30fc4de74" || {
    echo "Refusing to flash: partition layout differs from the authorized Note 4 table." >&2
    exit 1
}
verify_dir="$(mktemp -d)"
trap 'rm -rf "$verify_dir"' EXIT
python3 "$IDF_PATH/components/partition_table/gen_esp32part.py" \
    "$partitions" "$verify_dir/partition-table.bin" >/dev/null
cmp "$verify_dir/partition-table.bin" "$build_dir/partition_table/partition-table.bin" >/dev/null || {
    echo "Refusing to flash: generated partition table does not match firmware/partitions.csv." >&2
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
if [ "$(printf '%s' "$mac" | tr '[:upper:]' '[:lower:]')" != "$(printf '%s' "$authorized_mac" | tr '[:upper:]' '[:lower:]')" ]; then
    echo "Refusing to flash: $port has MAC '${mac:-unknown}', not the authorized Note 4 $authorized_mac." >&2
    exit 1
fi
if ! grep -Eiq '^Detected flash size:[[:space:]]*16MB[[:space:]]*$' <<<"$(probe flash_id)"; then
    echo "Refusing to flash: $port is not identified as a 16 MB flash device." >&2
    exit 1
fi

echo "==> $port is the authorized Note 4 ($mac); flashing C++ firmware"
"${esptool[@]}" --chip esp32s3 --port "$port" --baud 921600 \
    write_flash --flash_size 16MB --flash_mode dio --flash_freq 80m \
    0x0 "$build_dir/bootloader/bootloader.bin" \
    0x10000 "$build_dir/partition_table/partition-table.bin" \
    0x30000 "$build_dir/inkwash.bin"
if [ "${#monitor[@]}" -gt 0 ]; then
    python3 -m serial.tools.miniterm --raw --dtr 0 --rts 0 "$port" 115200
fi
