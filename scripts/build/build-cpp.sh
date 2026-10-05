#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -z "${IDF_PATH:-}" ]; then
    for candidate in "$HOME/esp/esp-idf" "$HOME"/esp/esp-idf-*; do
        if [ -f "$candidate/export.sh" ]; then
            export IDF_PATH="$candidate"
            break
        fi
    done
fi
if [ -z "${IDF_PATH:-}" ] || [ ! -f "$IDF_PATH/export.sh" ]; then
    echo 'Set IDF_PATH to an ESP-IDF 5.5.5 installation.' >&2
    exit 1
fi
source "$IDF_PATH/export.sh"
if [ "$(idf.py --version)" != "ESP-IDF v5.5.5" ]; then
    echo 'ESP-IDF 5.5.5 is required.' >&2
    exit 1
fi
if [ ! -f "$repo/firmware/components/lvgl/lvgl.h" ]; then
    echo 'Initialize LVGL with: git submodule update --init firmware/components/lvgl' >&2
    exit 1
fi
build_dir="${INKWASH_CPP_BUILD_DIR:-$repo/firmware/build}"
idf.py -C "$repo/firmware" -B "$build_dir" -D CMAKE_EXPORT_COMPILE_COMMANDS=ON "$@" reconfigure build
