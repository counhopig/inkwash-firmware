#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

key="${INKWASH_SECURE_BOOT_SIGNING_KEY:-}"
if [ -z "$key" ] || [ ! -f "$key" ]; then
    echo "Set INKWASH_SECURE_BOOT_SIGNING_KEY to an existing Secure Boot V2 RSA private key." >&2
    exit 1
fi
key="$(realpath "$key")"
openssl rsa -in "$key" -check -noout >/dev/null

secure_defaults="$(mktemp)"
trap 'rm -f "$secure_defaults"' EXIT
printf '%s\n' \
    'CONFIG_SECURE_BOOT=y' \
    'CONFIG_SECURE_BOOT_V2_ENABLED=y' \
    'CONFIG_SECURE_BOOT_BUILD_SIGNED_BINARIES=y' \
    "CONFIG_SECURE_BOOT_SIGNING_KEY=\"$key\"" \
    'CONFIG_SECURE_FLASH_ENC_ENABLED=y' \
    'CONFIG_SECURE_FLASH_ENCRYPTION_AES256=y' \
    'CONFIG_SECURE_FLASH_ENCRYPTION_MODE_RELEASE=y' \
    'CONFIG_NVS_ENCRYPTION=y' \
    'CONFIG_NVS_SEC_KEY_PROTECT_USING_FLASH_ENC=y' \
    'CONFIG_SECURE_BOOT_ALLOW_JTAG=n' \
    'CONFIG_SECURE_BOOT_ALLOW_ROM_BASIC=n' \
    > "$secure_defaults"

export INKWASH_BUILD_PROFILE=secure
export INKWASH_SECURE_BOOT_SIGNING_KEY="$key"
export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;$secure_defaults"
./scripts/build-rust.sh --release --locked
