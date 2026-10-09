#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUTPUT="$ROOT/.local/bin/guest-video-stream"
if [[ $# != 0 ]]; then
 [[ $# == 2 && "$1" == --output && "$2" == /* && ! -e "$2" && ! -L "$2" ]] || { echo 'Usage: build-guest-video-stream.sh [--output NEW_ABSOLUTE_BINARY]' >&2; exit 2; }
 OUTPUT="$2"
fi
mkdir -p "$(dirname "$OUTPUT")"
xcrun clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter -Werror \
 -framework Foundation -framework VideoToolbox -framework CoreVideo -framework CoreMedia \
 "$ROOT/native/tools/guest-video-stream.m" -o "$OUTPUT"
