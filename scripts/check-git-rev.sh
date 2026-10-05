#!/usr/bin/env bash
# Verify that the C++ application embeds the current Git revision.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
if [ "${1:-}" = "--build" ]; then
    ./scripts/build-cpp.sh
    shift
fi
elf="${1:-${INKWASH_CPP_BUILD_DIR:-$repo/firmware/build}/inkwash.elf}"
expected="$(git describe --always --dirty --tags)"
strings "$elf" | grep -Fx "$expected" >/dev/null || {
    echo "Firmware does not embed current revision: $expected" >&2
    exit 1
}
echo "Firmware revision verified: $expected"
