#!/usr/bin/env bash
# Verify the release ELF embeds the current git revision.
#
# Root cause (P1#5): build.rs only declared rerun-if-changed on ../.git/HEAD
# and ../.git/refs. The refs *directory* is not recursively watched by
# Cargo, so committing on the current branch (rewriting
# .git/refs/heads/<branch>, while HEAD stays the same symbolic ref) never
# re-ran build.rs -> the embedded GIT_REV went stale and the ELF still said
# e.g. v0.5.0-59-g44c1d0c-dirty after dozens of commits.
#
# This gate makes a stale ELF revision a hard build failure:
#   scripts/check-git-rev.sh [--build] [elf-path]
#
#   --build  : run ./scripts/build-rust.sh --release first
#   default  : verify the existing release ELF only
#
# Exit 0 only when the ELF's boot banner contains the exact current
# `git describe --always --dirty --tags` of the repository.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

BUILD=0
ELF="${1:-$repo_root/rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4}"
if [ "${1:-}" = "--build" ]; then
    BUILD=1
    ELF="${2:-$repo_root/rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4}"
fi

if [ "$BUILD" -eq 1 ]; then
    ./scripts/build-rust.sh --release
fi

if [ ! -f "$ELF" ]; then
    echo "check-git-rev: ELF not found: $ELF" >&2
    exit 1
fi

expected="$(git describe --always --dirty --tags)"
echo "check-git-rev: expected git revision: $expected"

# The boot banner prints exactly `(git {GIT_REV})`; a stale build.rs leaves
# an older describe behind and this exact match fails. (Capture output into
# a variable instead of `grep -q` in a pipeline: under `set -o pipefail`,
# `grep -q` exits as soon as it matches, `strings` then dies on SIGPIPE and
# pipefail turns that into a pipeline failure.)
if strings "$ELF" | grep -F "bring-up starting (git $expected)" >/dev/null; then
    echo "check-git-rev: OK - ELF embeds current revision $expected"
    exit 0
fi

echo "check-git-rev: FAIL - ELF does not embed current revision $expected" >&2
echo "  (stale build.rs rerun-if-changed; the fix lives in rust-firmware/build.rs)" >&2
strings "$ELF" | grep -m5 "bring-up starting (git " >&2 || true
exit 1
