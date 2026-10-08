#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# == 2 && "$1" == "--output" ]]; then
  OUTPUT="$2"
  [[ "$OUTPUT" == /* ]] || { echo "--output must be absolute" >&2; exit 2; }
  [[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
elif [[ $# == 0 ]]; then
  OUTPUT="$ROOT/.local/bin/macos-guest-agent"
else
  echo "Usage: build.sh [--output NEW_ABSOLUTE_PATH]" >&2; exit 2
fi
mkdir -p "$(dirname "$OUTPUT")"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Metal -framework ScreenCaptureKit -framework CoreGraphics -framework ImageIO "$ROOT/native/guest-macos/main.swift" "$ROOT/native/guest-macos/capture.swift" -o "$OUTPUT"
if [[ $# == 0 ]]; then
  mkdir -p "$ROOT/.local/guest-share"
  cp "$OUTPUT" "$ROOT/.local/guest-share/macos-guest-agent"
fi
