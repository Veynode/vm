#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/.local/bin" "$ROOT/.local/guest-share"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 \
 -framework ScreenCaptureKit -framework CoreGraphics -framework ImageIO -framework CryptoKit \
 "$ROOT/native/tools/macos-desktop-probe.swift" -o "$ROOT/.local/bin/macos-desktop-probe"
cp "$ROOT/.local/bin/macos-desktop-probe" "$ROOT/.local/guest-share/macos-desktop-probe"
