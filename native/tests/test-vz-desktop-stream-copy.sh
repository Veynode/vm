#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS is required' >&2; exit 1; }
BUILD="$(mktemp -d "${TMPDIR:-/tmp}/vz-desktop-copy.XXXXXX")"
trap 'rm -rf "$BUILD"' EXIT
xcrun clang -O2 -fobjc-arc -Wall -Wextra -Wno-unused-parameter -Wno-unused-function -Werror \
 -framework Foundation -framework VideoToolbox -framework CoreVideo -framework CoreMedia -framework IOSurface \
 "$ROOT/native/tests/vz-desktop-stream-copy-tests.m" -o "$BUILD/copy-tests"
"$BUILD/copy-tests"
