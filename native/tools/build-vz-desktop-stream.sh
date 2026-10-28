#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUTPUT="$ROOT/.local/bin/vz-desktop-stream"
if [[ $# != 0 ]]; then
 [[ $# == 2 && "$1" == --output && "$2" == /* ]] || { echo 'Usage: build-vz-desktop-stream.sh [--output ABSOLUTE_BINARY]' >&2; exit 2; }
 OUTPUT="$2"
fi
mkdir -p "$(dirname "$OUTPUT")"
xcrun clang -O2 -fobjc-arc -Wall -Wextra -Wno-unused-parameter -Werror \
 -framework Foundation -framework VideoToolbox -framework CoreVideo -framework CoreMedia -framework IOSurface \
 "$ROOT/native/tools/vz-desktop-stream.m" -o "$OUTPUT"
