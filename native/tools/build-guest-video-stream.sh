#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/.local/bin"
xcrun clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter -Werror \
 -framework Foundation -framework VideoToolbox -framework CoreVideo -framework CoreMedia \
 "$ROOT/native/tools/guest-video-stream.m" -o "$ROOT/.local/bin/guest-video-stream"
