#!/usr/bin/env bash
# Boot-ledger image gate.
#
# The boot ledger sits in `.rtc_noinit`, which the linker marks NOLOAD: the
# section carries no contents, and the ELF -> image converters we flash with
# (espflash, esptool) therefore leave it out of the loadable segments. That
# matters, because the bootloader re-initializes RTC memory segments on every
# reset that is not a deep-sleep wake (`esp_image_format.c: should_load()`). If
# a converter ever started padding the noinit gap and covering the ledger
# address, the ledger would be zeroed on exactly the resets it counts and the
# boot-loop guard would silently stop working - with no failing test anywhere.
#
# This gate converts the application ELF with the same converters used to flash
# it and fails if any loadable segment overlaps `.rtc_noinit`.
#
# Usage: scripts/check-boot-ledger.sh [app-elf]
#        scripts/check-boot-ledger.sh --self-test
set -euo pipefail
cd "$(dirname "$0")/.."

mode=check
elf="rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4"
if [ "${1:-}" = "--self-test" ]; then
    mode=self-test
elif [ -n "${1:-}" ]; then
    elf="$1"
fi

# Locate the symbol reader without hardcoding a toolchain path: espup installs
# the generic name, older installs ship the esp32s3-prefixed one. A host `nm`
# that happens to understand Xtensa is accepted as a last resort; the RTC window
# check below is what makes a wrong tool fail loudly instead of reading a bogus
# range and passing for the wrong reason.
for candidate in "$HOME"/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin \
    "$HOME"/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/xtensa-esp-elf/bin; do
    if [ -x "$candidate/xtensa-esp-elf-nm" ]; then
        PATH="$candidate:$PATH"
        break
    fi
done
nm=""
for candidate in xtensa-esp32s3-elf-nm xtensa-esp-elf-nm nm; do
    if command -v "$candidate" >/dev/null 2>&1; then
        nm="$candidate"
        break
    fi
done
if [ -z "$nm" ]; then
    echo "no nm found; install the Xtensa toolchain (see README)" >&2
    exit 1
fi

# esptool is the reference parser for the images both converters produce. It
# lives in ESP-IDF's python environment, so honor $IDF_PATH and probe the
# conventional install locations exactly like scripts/build-rust.sh does.
if [ -z "${IDF_PATH:-}" ]; then
    for candidate in "$HOME/esp/esp-idf" "$HOME"/esp/esp-idf-* \
        "$HOME"/.espressif/frameworks/esp-idf-*; do
        if [ -f "$candidate/export.sh" ]; then
            IDF_PATH="$candidate"
            break
        fi
    done
fi
if [ -z "${IDF_PATH:-}" ] || [ ! -f "$IDF_PATH/export.sh" ]; then
    echo "Could not locate ESP-IDF. Set \$IDF_PATH or install it under ~/esp (see README)." >&2
    exit 1
fi
export IDF_PATH
# shellcheck disable=SC1091
. "$IDF_PATH/export.sh" >/dev/null 2>&1
if ! python3 -c 'import esptool' >/dev/null 2>&1; then
    echo "esptool is required to read the produced images (ESP-IDF python environment)" >&2
    exit 1
fi

# Decides whether a recorded esptool image_info table ($3) loads over the
# ledger range $1..$2. Prints the segment table it decided on, so a pass is
# auditable rather than an empty "ok".
decide() {
    python3 - "$1" "$2" "$3" <<'PY'
import re
import sys

start, end = int(sys.argv[1], 16), int(sys.argv[2], 16)
with open(sys.argv[3]) as table:
    text = table.read()
segments = [
    (int(load, 16), int(length, 16))
    for length, load in re.findall(
        r"^Segment \d+: len (0x[0-9a-fA-F]+) load (0x[0-9a-fA-F]+)", text, re.M
    )
]
if not segments:
    sys.exit("FAIL: no segments found in the image (converter output changed?)")
if all(load >= 0x50000000 for load, _ in segments):
    sys.exit("FAIL: parsed only RTC segments; this is not an application image")
for load, length in segments:
    print(f"  segment {load:#010x}..{load + length:#010x} ({length} bytes)")
covering = [
    (load, length) for load, length in segments if load < end and start < load + length
]
if covering:
    sys.exit(
        f"FAIL: a loadable segment covers the boot ledger {start:#010x}..{end:#010x}: "
        + ", ".join(f"{load:#010x}+{length:#x}" for load, length in covering)
    )
print(f"  ledger {start:#010x}..{end:#010x} is outside every segment")
PY
}

if [ "$mode" = self-test ]; then
    start=0x50000000
    end=0x50000008
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    printf 'Segment 1: len 0x00800 load 0x3fc90000 file_offs 0x0 [DRAM]\nSegment 6: len 0x00020 load 0x50000000 file_offs 0x0 [RTC_DATA]\n' \
        > "$tmp/covering.txt"
    printf 'Segment 1: len 0x00800 load 0x3fc90000 file_offs 0x0 [DRAM]\nSegment 6: len 0x00020 load 0x50000008 file_offs 0x0 [RTC_DATA]\n' \
        > "$tmp/clear.txt"
    printf 'no segments here\n' > "$tmp/unparsable.txt"

    echo "==> self-test: a segment covering the ledger must be rejected"
    if decide "$start" "$end" "$tmp/covering.txt" >/dev/null 2>&1; then
        echo "self-test FAILED: a covering segment was accepted" >&2
        exit 1
    fi
    echo "==> self-test: a segment that stops short of the ledger must be accepted"
    if ! decide "$start" "$end" "$tmp/clear.txt" >/dev/null; then
        echo "self-test FAILED: a non-covering segment was rejected" >&2
        exit 1
    fi
    echo "==> self-test: an image without segments must be rejected"
    if decide "$start" "$end" "$tmp/unparsable.txt" >/dev/null 2>&1; then
        echo "self-test FAILED: an unparsable image was accepted" >&2
        exit 1
    fi
    echo "self-test ok"
    exit 0
fi

if [ ! -f "$elf" ]; then
    echo "application ELF not found: $elf" >&2
    exit 1
fi

# nm prints symbol values in hexadecimal without a prefix; normalize before any
# arithmetic, or `$((...))` silently reads them as decimal.
start="0x$("$nm" "$elf" | awk '$3 == "_rtc_noinit_start" { print $1 }')"
end="0x$("$nm" "$elf" | awk '$3 == "_rtc_noinit_end" { print $1 }')"
if [ "$start" = "0x" ] || [ "$end" = "0x" ]; then
    echo "no _rtc_noinit_start/_rtc_noinit_end in $elf" >&2
    exit 1
fi
if [ "$((start))" -ge "$((end))" ]; then
    echo "$elf has an empty .rtc_noinit: the boot ledger is not linked in" >&2
    exit 1
fi
# ESP32-S3 RTC slow memory. Anything else means the symbol reader could not be
# trusted, and a range outside the RTC window would make this gate vacuous.
if [ "$((start))" -lt "$((0x50000000))" ] || [ "$((end))" -gt "$((0x50002000))" ]; then
    echo "ledger range $start..$end is not inside ESP32-S3 RTC slow memory" >&2
    exit 1
fi
printf '==> boot ledger occupies %s..%s in %s\n' "$start" "$end" "$elf"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

checked=0
failures=0

# espflash is the converter used by README.md and scripts/release.sh, so check
# it first; esptool is what the Espressif build uses and stays the fallback.
if command -v espflash >/dev/null 2>&1; then
    espflash save-image --skip-update-check --chip esp32s3 "$elf" "$tmp/espflash.bin" >/dev/null 2>&1
    checked=$((checked + 1))
    echo "==> espflash image"
    python3 -m esptool --chip esp32s3 image_info "$tmp/espflash.bin" > "$tmp/espflash.txt" 2>/dev/null
    if ! decide "$start" "$end" "$tmp/espflash.txt"; then
        failures=$((failures + 1))
    fi
else
    echo "note: espflash not found; the flashing path was not checked" >&2
fi

python3 -m esptool --chip esp32s3 elf2image -o "$tmp/esptool.bin" "$elf" >/dev/null
checked=$((checked + 1))
echo "==> esptool image"
python3 -m esptool --chip esp32s3 image_info "$tmp/esptool.bin" > "$tmp/esptool.txt" 2>/dev/null
if ! decide "$start" "$end" "$tmp/esptool.txt"; then
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    echo "boot ledger is not safe from the application image" >&2
    exit 1
fi
echo "==> ok: $checked image(s) checked, no segment covers the boot ledger"
