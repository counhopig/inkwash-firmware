#!/usr/bin/env bash
# Builds the release firmware locally and publishes it to a GitHub Release.
#
# Usage (run from the repo root):
#   ./scripts/release.sh v0.2.0
#
# Requires an authenticated `gh` CLI session.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:-}"
if [ -z "$TAG" ]; then
    echo "usage: $0 <tag>   e.g. $0 v0.2.0" >&2
    exit 1
fi
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    echo "tags should look like 'v1.2.3'" >&2
    exit 1
fi

REPO="counhopig/inkwash-firmware"
ELF="rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4"
BOOTLOADER="rust-firmware/target/xtensa-esp32s3-espidf/release/bootloader.bin"
PARTITIONS="rust-firmware/partitions.csv"
PARTITIONS_BIN="rust-firmware/target/xtensa-esp32s3-espidf/release/partition-table.bin"

if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    echo "release requires a clean working tree" >&2
    exit 1
fi
HEAD="$(git rev-parse --verify HEAD)"
command -v gh >/dev/null || {
    echo "gh is required to publish the release" >&2
    exit 1
}
gh auth status >/dev/null
gh repo view "$REPO" >/dev/null
if git rev-parse --verify "refs/tags/$TAG" >/dev/null 2>&1 \
    || git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1 \
    || gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    echo "tag or release already exists: $TAG" >&2
    exit 1
fi

echo "==> Building release firmware..."
# Stamp the build with the commit time so the same tag always rebuilds to
# the same image, and build only the dependency set CI has checked.
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
export SOURCE_DATE_EPOCH
./scripts/build-rust.sh --release --locked
test -f "$ELF" || { echo "expected firmware not found at $ELF" >&2; exit 1; }
test -f "$BOOTLOADER" || { echo "expected bootloader not found at $BOOTLOADER" >&2; exit 1; }
test -f "$PARTITIONS" || { echo "expected partition table not found at $PARTITIONS" >&2; exit 1; }
test -f "$PARTITIONS_BIN" || { echo "expected generated partition table not found at $PARTITIONS_BIN" >&2; exit 1; }

sdkconfigs=(rust-firmware/target/xtensa-esp32s3-espidf/release/build/esp-idf-sys-*/out/sdkconfig)
SDKCONFIG=""
for candidate in "${sdkconfigs[@]}"; do
    if [ -f "$candidate" ] && { [ -z "$SDKCONFIG" ] || [ "$candidate" -nt "$SDKCONFIG" ]; }; then
        SDKCONFIG="$candidate"
    fi
done
if [ -z "$SDKCONFIG" ]; then
    echo "release sdkconfig not found" >&2
    exit 1
fi

require_config() {
    if ! grep -Fqx "$1" "$SDKCONFIG"; then
        echo "release sdkconfig does not contain required setting: $1" >&2
        exit 1
    fi
}

require_config 'CONFIG_IDF_TARGET="esp32s3"'
require_config 'CONFIG_ESPTOOLPY_FLASHSIZE_16MB=y'
require_config 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y'
require_config 'CONFIG_ESPTOOLPY_FLASHFREQ_80M=y'
require_config 'CONFIG_ESP_COREDUMP_ENABLE_TO_FLASH=y'
require_config '# CONFIG_ESP_COREDUMP_CAPTURE_DRAM is not set'
# The public release is the developer build: it must never carry Secure Boot
# or flash encryption, which would burn one-way eFuses on first boot of
# every device flashed from it.
require_config '# CONFIG_SECURE_BOOT is not set'
require_config '# CONFIG_SECURE_FLASH_ENC_ENABLED is not set'

command -v espflash >/dev/null || {
    echo "espflash is required to verify the generated partition table" >&2
    exit 1
}
verify_dir="$(mktemp -d)"
trap 'rm -rf "$verify_dir"' EXIT
espflash partition-table --skip-update-check --to-binary \
    --output "$verify_dir/partition-table.bin" "$PARTITIONS" >/dev/null
if ! cmp -s "$verify_dir/partition-table.bin" "$PARTITIONS_BIN"; then
    echo "generated partition table does not match $PARTITIONS" >&2
    exit 1
fi
echo "==> Verifying the boot ledger survives the application image"
./scripts/check-boot-ledger.sh "$ELF"

espflash save-image --skip-update-check --chip esp32s3 \
    --flash-size 16mb --flash-mode dio --flash-freq 80mhz \
    --bootloader "$BOOTLOADER" --partition-table "$PARTITIONS" \
    --partition-table-offset 0x10000 \
    --target-app-partition factory \
    "$ELF" "$verify_dir/inkwash-note4.bin" >/dev/null
factory_size=$((0x400000))
if ! grep -Eq '^factory,[[:space:]]*app,[[:space:]]*factory,[[:space:]]*0x30000,[[:space:]]*0x400000,' "$PARTITIONS"; then
    echo "partitions.csv no longer has the 4 MiB factory slot this size check assumes" >&2
    exit 1
fi
app_size="$(wc -c < "$verify_dir/inkwash-note4.bin")"
echo "==> Application image: $app_size of $factory_size bytes ($((app_size * 100 / factory_size))%)"
if [ "$app_size" -gt "$factory_size" ]; then
    echo "application image does not fit the factory partition" >&2
    exit 1
fi

echo "==> Creating draft GitHub Release and uploading firmware"
gh release create "$TAG" "$ELF" "$BOOTLOADER" "$PARTITIONS" \
    --repo "$REPO" \
    --target "$HEAD" \
    --draft \
    --title "Inkwash Firmware $TAG" \
    --notes "Firmware for the Zectrix Note 4 e-paper device.

**Security boundary:** this is the developer build. Secure Boot, flash
encryption and NVS encryption are off, so anyone with physical access to the
device can read the stored Wi-Fi password and server token from flash and can
replace the firmware. Use a dedicated server token for each device and revoke
it if the device is lost. Build the secure profile (\`scripts/build-secure.sh\`)
with your own signing key if that is not acceptable.

Identify the board first; a port name is not an identity. Flash only an
ESP32-S3 with 16 MB flash whose MAC is your Note 4 (never a Note 4C):

\`\`\`bash
esptool.py --chip esp32s3 --port <port> read_mac
esptool.py --chip esp32s3 --port <port> flash_id
\`\`\`

Then flash with:

\`\`\`bash
espflash flash --chip esp32s3 --flash-size 16mb --flash-mode dio --flash-freq 80mhz \\
  --bootloader bootloader.bin \\
  --partition-table partitions.csv --partition-table-offset 0x10000 --non-interactive \\
  inkwash-note4
\`\`\`
"

echo "==> Publishing GitHub Release"
gh release edit "$TAG" --repo "$REPO" --draft=false

git fetch origin "refs/tags/$TAG:refs/tags/$TAG"
if git remote get-url github >/dev/null 2>&1; then
    git push github "refs/tags/$TAG"
fi

echo "==> Done: https://github.com/$REPO/releases/tag/$TAG"
