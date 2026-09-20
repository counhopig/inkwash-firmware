#!/usr/bin/env bash
# Builds the release firmware locally (this repo builds with a real ESP-IDF
# toolchain, which is impractical on CI) and publishes it to a GitHub Release.
#
# Usage (run from the repo root):
#   ./scripts/release.sh v0.2.0
#
# Steps: build the release ELF -> create/verify the tag -> push it to both
# remotes -> create the GitHub Release and upload the firmware. Requires
# `gh` authenticated (see `gh auth status`).
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:-}"
if [ -z "$TAG" ]; then
    echo "usage: $0 <tag>   e.g. $0 v0.2.0" >&2
    exit 1
fi
if [[ "$TAG" != v* ]]; then
    echo "tags should look like 'v1.2.3'" >&2
    exit 1
fi

REPO="counhopig/inkwash-firmware"
ELF="rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4"
BOOTLOADER="rust-firmware/target/xtensa-esp32s3-espidf/release/bootloader.bin"
PARTITIONS="rust-firmware/partitions.csv"

echo "==> Building release firmware..."
./scripts/build-rust.sh --release
test -f "$ELF" || { echo "expected firmware not found at $ELF" >&2; exit 1; }
test -f "$BOOTLOADER" || { echo "expected bootloader not found at $BOOTLOADER" >&2; exit 1; }
test -f "$PARTITIONS" || { echo "expected partition table not found at $PARTITIONS" >&2; exit 1; }

echo "==> Tagging $TAG"
if ! git rev-parse "$TAG" >/dev/null 2>&1; then
    git tag -a "$TAG" -m "inkwash-firmware $TAG"
fi

echo "==> Pushing tag to origin and github"
git push origin "$TAG"
git push github "$TAG" 2>/dev/null || true

echo "==> Creating GitHub Release and uploading firmware"
gh release create "$TAG" "$ELF" "$BOOTLOADER" "$PARTITIONS" \
    --repo "$REPO" \
    --title "Inkwash Firmware $TAG" \
    --notes "Firmware for the Zectrix Note 4 e-paper device. Flash with:

\`\`\`bash
espflash flash --chip esp32s3 --flash-size 16mb --flash-mode dio --flash-freq 80mhz \\
  --bootloader bootloader.bin \\
  --partition-table partitions.csv --non-interactive \\
  inkwash-note4
\`\`\`
" || gh release upload "$TAG" "$ELF" "$BOOTLOADER" "$PARTITIONS" --repo "$REPO" --clobber

echo "==> Done: https://github.com/$REPO/releases/tag/$TAG"
