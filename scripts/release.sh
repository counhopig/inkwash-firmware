#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
tag="${1:?Usage: scripts/release.sh vX.Y.Z}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    echo 'Invalid release tag.' >&2
    exit 1
fi
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    echo 'Release requires a clean working tree.' >&2
    exit 1
fi
source "${IDF_PATH:-$HOME/esp/esp-idf}/export.sh"
gh auth status >/dev/null
project="counhopig/inkwash-firmware"
if git rev-parse --verify "refs/tags/$tag" >/dev/null 2>&1 ||
   git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1 ||
   gh release view "$tag" --repo "$project" >/dev/null 2>&1; then
    echo 'Tag or release already exists.' >&2
    exit 1
fi
export SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
./scripts/build-cpp.sh
bash firmware/test/run.sh
build_dir="${INKWASH_CPP_BUILD_DIR:-$repo/firmware/build}"
./scripts/check-boot-ledger.sh "$build_dir/inkwash.elf"
./scripts/check-git-rev.sh "$build_dir/inkwash.elf"
for setting in 'CONFIG_IDF_TARGET="esp32s3"' 'CONFIG_ESPTOOLPY_FLASHSIZE_16MB=y' \
    'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y' 'CONFIG_ESPTOOLPY_FLASHFREQ_80M=y' \
    '# CONFIG_SECURE_BOOT is not set' '# CONFIG_SECURE_FLASH_ENC_ENABLED is not set' \
    '# CONFIG_ESP_COREDUMP_CAPTURE_DRAM is not set'; do
    grep -Fqx "$setting" firmware/sdkconfig || { echo "Missing required setting: $setting" >&2; exit 1; }
done
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
python "$IDF_PATH/components/partition_table/gen_esp32part.py" \
    firmware/partitions.csv "$stage/partition-table.bin" >/dev/null
cmp "$stage/partition-table.bin" "$build_dir/partition_table/partition-table.bin"
tar -czf "$stage/inkwash-note4-$tag.tar.gz" -C "$build_dir" \
    inkwash.bin inkwash.elf inkwash.map flash_args bootloader/bootloader.bin partition_table/partition-table.bin
gh release create "$tag" "$stage/inkwash-note4-$tag.tar.gz" \
    --repo "$project" --target "$(git rev-parse HEAD)" --draft \
    --title "Inkwash Firmware $tag" \
    --notes 'C++ firmware for Zectrix Note 4. ESP32-S3, 16 MB Flash, DIO, 80 MHz. Verify the authorized board identity before flashing. Configuration is stored unencrypted in NVS; Secure Boot and Flash encryption are disabled.'
gh release edit "$tag" --repo "$project" --draft=false
