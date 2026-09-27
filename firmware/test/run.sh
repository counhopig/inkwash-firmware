#!/usr/bin/env bash
# Builds and runs the host tests for firmware/main/core.
set -euo pipefail
cd "$(dirname "$0")"
IDF="${IDF_PATH:-$HOME/esp/esp-idf-v5.5.5}"
out="$(mktemp -d)"
gcc -c -O1 -I"$IDF/components/json/cJSON" "$IDF/components/json/cJSON/cJSON.c" -o "$out/cJSON.o"
g++ -std=c++17 -Wall -Wextra -Werror -O1 -g \
    -I../main -I"$IDF/components/json/cJSON" \
    core_test.cc ../main/core/*.cc "$out/cJSON.o" \
    -o "$out/core_test"
"$out/core_test"
