#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/.local/bin" "$ROOT/.local/guest-share"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 \
  -framework ScreenCaptureKit -framework CoreGraphics -framework CoreMedia -framework CoreVideo \
  "$ROOT/native/tools/macos-capture-stream.swift" -o "$ROOT/.local/bin/macos-capture-stream"
cp "$ROOT/.local/bin/macos-capture-stream" "$ROOT/.local/guest-share/macos-capture-stream"
