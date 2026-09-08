#!/usr/bin/env bash
set -euo pipefail

# rust-analyzer may be launched from the GUI, where ESP-IDF's environment is
# not inherited. Discover an ESP-IDF installation using the conventional
# locations supported by the firmware build workflow.
if [[ -z "${IDF_PATH:-}" ]]; then
    for candidate in "$HOME/esp/esp-idf" "$HOME"/esp/esp-idf-* \
        "$HOME"/.espressif/frameworks/esp-idf-*; do
        if [[ -f "$candidate/export.sh" ]]; then
            IDF_PATH="$candidate"
            break
        fi
    done
fi

if [[ -z "${IDF_PATH:-}" || ! -f "$IDF_PATH/export.sh" ]]; then
    echo "Could not locate ESP-IDF; set IDF_PATH before starting rust-analyzer." >&2
    exit 1
fi
export IDF_PATH

# export.sh supplies the IDF tools and Python environment required by
# esp-idf-sys's build script. Its normal status output is useful interactively
# but only obscures rust-analyzer's JSON cargo output.
# shellcheck disable=SC1091
. "$IDF_PATH/export.sh" >/dev/null 2>&1

if [[ -z "${LIBCLANG_PATH:-}" ]]; then
    esp_toolchain_root="$(rustup toolchain list -v 2>/dev/null | awk '$1 == "esp" {print $NF}')"
    if [[ -n "$esp_toolchain_root" ]]; then
        for candidate in "$esp_toolchain_root"/xtensa-esp32-elf-clang/*/esp-clang/lib; do
            if [[ -d "$candidate" ]]; then
                LIBCLANG_PATH="$candidate"
                break
            fi
        done
    fi
fi
export LIBCLANG_PATH

# Keep rustup's compiler selection active after export.sh. The esp cargo
# wrapper is invoked directly, so without this environment it would resolve
# build scripts through the user's default stable toolchain.
export RUSTUP_TOOLCHAIN=esp-ra

# rust-analyzer override commands receive no implicit cargo arguments.
if [[ "$#" -eq 0 ]]; then
    set -- check --quiet --workspace --message-format=json --all-targets --keep-going
fi

exec "$(rustup which cargo --toolchain esp-ra)" "$@"
