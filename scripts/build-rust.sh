#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

build_profile="${INKWASH_BUILD_PROFILE:-production}"
cargo_args=()
for arg in "$@"; do
    if [ "$arg" = "--diagnostic" ]; then
        build_profile="diagnostic"
    else
        cargo_args+=("$arg")
    fi
done

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

if [ -z "${IDF_PYTHON_ENV_PATH:-}" ] \
    || [ ! -x "$IDF_PYTHON_ENV_PATH/bin/python" ] \
    || ! command -v idf.py >/dev/null 2>&1 \
    || ! command -v xtensa-esp32s3-elf-gcc >/dev/null 2>&1; then
    # shellcheck disable=SC1091
    . "$IDF_PATH/export.sh"
else
    export PATH="$IDF_PATH/tools:$PATH"
fi

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
target_root="target"
if [ "$build_profile" = "diagnostic" ]; then
    export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.diagnostic.defaults"
    target_root="target-diagnostic"
    export CARGO_TARGET_DIR="$PWD/$target_root"
elif [ "$build_profile" = "secure" ]; then
    if [ -z "${ESP_IDF_SDKCONFIG_DEFAULTS:-}" ]; then
        echo "secure profile requires ESP_IDF_SDKCONFIG_DEFAULTS" >&2
        exit 1
    fi
    target_root="target-secure"
    export CARGO_TARGET_DIR="$PWD/$target_root"
fi
partition_hash="v2:$(sha256sum partitions.csv | awk '{print $1}')"
partition_stamp="$target_root/.inkwash-partitions.sha256"
if [ -d "$target_root" ] && { [ ! -f "$partition_stamp" ] || [ "$(cat "$partition_stamp")" != "$partition_hash" ]; }; then
    cargo clean
fi
# INKWASH_CARGO_SUBCOMMAND=clippy runs the target-side lints in the same
# ESP-IDF environment, e.g. `... ./scripts/build-rust.sh --release --locked -- -D warnings`.
cargo "${INKWASH_CARGO_SUBCOMMAND:-build}" "${cargo_args[@]}"
if [ "${INKWASH_CARGO_SUBCOMMAND:-build}" != build ]; then
    exit 0
fi
mkdir -p "$target_root"
printf '%s\n' "$partition_hash" > "$partition_stamp"

if [ "$build_profile" = "secure" ]; then
    # Secure Boot V2 only boots images that are padded for, and carry, a
    # signature block. The cargo build produces an ELF and a signed
    # bootloader; turn the ELF into the signed factory image here and prove
    # both signatures and the security settings before anything is flashed.
    key="${INKWASH_SECURE_BOOT_SIGNING_KEY:?secure profile requires INKWASH_SECURE_BOOT_SIGNING_KEY}"
    out="$CARGO_TARGET_DIR/xtensa-esp32s3-espidf/release"
    elf="$out/inkwash-note4"
    for file in "$elf" "$out/bootloader.bin"; do
        if [ ! -f "$file" ]; then
            echo "secure profile: expected $file (build with --release)" >&2
            exit 1
        fi
    done

    sdkconfig=""
    for candidate in "$out"/build/esp-idf-sys-*/out/sdkconfig; do
        if [ -f "$candidate" ] && { [ -z "$sdkconfig" ] || [ "$candidate" -nt "$sdkconfig" ]; }; then
            sdkconfig="$candidate"
        fi
    done
    if [ -z "$sdkconfig" ]; then
        echo "secure profile: generated sdkconfig not found" >&2
        exit 1
    fi
    for setting in \
        CONFIG_SECURE_BOOT=y \
        CONFIG_SECURE_BOOT_V2_ENABLED=y \
        CONFIG_SECURE_BOOT_BUILD_SIGNED_BINARIES=y \
        CONFIG_SECURE_FLASH_ENC_ENABLED=y \
        CONFIG_SECURE_FLASH_ENCRYPTION_AES256=y \
        CONFIG_SECURE_FLASH_ENCRYPTION_MODE_RELEASE=y \
        CONFIG_NVS_ENCRYPTION=y \
        CONFIG_ESPTOOLPY_FLASHSIZE_16MB=y \
        CONFIG_ESPTOOLPY_FLASHMODE_DIO=y \
        CONFIG_ESPTOOLPY_FLASHFREQ_80M=y \
        '# CONFIG_SECURE_BOOT_INSECURE is not set'; do
        if ! grep -Fqx "$setting" "$sdkconfig"; then
            echo "secure profile: $sdkconfig lacks required setting: $setting" >&2
            exit 1
        fi
    done

    unsigned="$out/inkwash-note4-unsigned.bin"
    signed="$out/inkwash-note4-signed.bin"
    python3 -m esptool --chip esp32s3 elf2image \
        --flash_size 16MB --flash_mode dio --flash_freq 80m \
        --secure-pad-v2 -o "$unsigned" "$elf" >/dev/null
    python3 -m espsecure sign_data --version 2 --keyfile "$key" \
        --output "$signed" "$unsigned" >/dev/null
    python3 -m espsecure verify_signature --version 2 --keyfile "$key" "$signed" >/dev/null
    python3 -m espsecure verify_signature --version 2 --keyfile "$key" "$out/bootloader.bin" >/dev/null
    echo "==> secure profile: signed factory image $signed; bootloader signature verified"
fi
