#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# == 2 && "$1" == "--output" ]]; then
  OUTPUT="$2"
  [[ "$OUTPUT" == /* ]] || { echo "--output must be absolute" >&2; exit 2; }
  [[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
elif [[ $# == 0 ]]; then
  OUTPUT="$ROOT/.local/bin/macos-guest-agent"
  [[ ! -e "$ROOT/.local/guest-share/macos-guest-agent" && ! -L "$ROOT/.local/guest-share/macos-guest-agent" ]] || { echo "Refusing existing shared output; use --output with a fresh path" >&2; exit 2; }
else
  echo "Usage: build.sh [--output NEW_ABSOLUTE_PATH]" >&2; exit 2
fi
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"
BUILD_DIR="$(mktemp -d "$(dirname "$OUTPUT")/.guest-build.XXXXXX")"
trap 'rm -rf "$BUILD_DIR"' EXIT
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Metal -framework ScreenCaptureKit -framework CoreMedia -framework CoreGraphics -framework ImageIO -framework Security -framework SystemConfiguration "$ROOT/native/shared/MetalCapabilities.swift" "$ROOT/native/guest-macos/graphics-tests.swift" "$ROOT/native/guest-macos/main.swift" "$ROOT/native/guest-macos/files.swift" "$ROOT/native/guest-macos/files-tests.swift" "$ROOT/native/guest-macos/capture.swift" "$ROOT/native/guest-macos/audio.swift" "$ROOT/native/guest-macos/windows.swift" "$ROOT/native/guest-macos/windows-tests.swift" "$ROOT/native/guest-macos/input.swift" "$ROOT/native/guest-macos/input-tests.swift" "$ROOT/native/guest-macos/console-binding.swift" "$ROOT/native/guest-macos/console-service.swift" "$ROOT/native/guest-macos/console-integration-tests.swift" -o "$BUILD_DIR/macos-guest-agent"
ln "$BUILD_DIR/macos-guest-agent" "$OUTPUT"
if [[ $# == 0 ]]; then
  mkdir -p "$ROOT/.local/guest-share"
  ln "$OUTPUT" "$ROOT/.local/guest-share/macos-guest-agent"
fi
