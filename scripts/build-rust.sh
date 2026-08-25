#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# Locate ESP-IDF without hardcoding an install path: honor $IDF_PATH when
# set, else probe the conventional install locations in order.
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
. "$IDF_PATH/export.sh"

# esp-idf-sys's bindgen step needs espup's esp-clang, which clang-sys does not
# find on its own. Locate it under the active `esp` rustup toolchain instead
# of hardcoding a path, so this works on any machine that ran `espup install`.
if [ -z "${LIBCLANG_PATH:-}" ]; then
    esp_toolchain_root="$(rustup toolchain list -v 2>/dev/null | awk '$1 == "esp" {print $NF}')"
    if [ -n "$esp_toolchain_root" ]; then
        for candidate in "$esp_toolchain_root"/xtensa-esp32-elf-clang/*/esp-clang/lib; do
            if [ -d "$candidate" ]; then
                export LIBCLANG_PATH="$candidate"
                break
            fi
        done
    fi
fi

if [ -z "${LIBCLANG_PATH:-}" ]; then
    echo "Could not locate esp-clang's libclang under the 'esp' rustup toolchain." >&2
    echo "Install it with 'espup install', or set LIBCLANG_PATH manually." >&2
    exit 1
fi

cd rust-firmware
exec cargo build "$@"
