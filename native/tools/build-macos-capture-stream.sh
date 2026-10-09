#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# == 2 && "$1" == "--output" && "$2" == /* ]]; then
  OUTPUT="$2"
elif [[ $# == 0 ]]; then
  OUTPUT="$ROOT/.local/bin/macos-capture-stream"
  [[ ! -e "$ROOT/.local/guest-share/macos-capture-stream" && ! -L "$ROOT/.local/guest-share/macos-capture-stream" ]] || { echo "Refusing existing shared output; use --output NEW_ABSOLUTE_PATH" >&2; exit 2; }
else
  echo "Usage: build-macos-capture-stream.sh [--output NEW_ABSOLUTE_PATH]" >&2; exit 2
fi
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"
SCRATCH="$(mktemp -d "$(dirname "$OUTPUT")/.capture-build.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 \
  -framework ScreenCaptureKit -framework CoreGraphics -framework CoreMedia -framework CoreVideo \
  -framework AppKit -framework Security -framework SystemConfiguration \
  "$ROOT/native/tools/macos-capture-stream.swift" "$ROOT/native/tools/macos-capture-lock-tests.swift" "$ROOT/native/guest-macos/console-binding.swift" -o "$SCRATCH/capture"
ln "$SCRATCH/capture" "$OUTPUT"
if [[ $# == 0 ]]; then
  mkdir -p "$ROOT/.local/guest-share"
  ln "$OUTPUT" "$ROOT/.local/guest-share/macos-capture-stream"
fi
