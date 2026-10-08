#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo 'Apple Silicon macOS is required' >&2; exit 1; }
mkdir -p "$ROOT/native/.build" "$ROOT/.local/bin"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Virtualization -framework AppKit "$ROOT/native/Sources/main.swift" "$ROOT/native/Sources/GuestStreamBridge.swift" -o "$ROOT/native/.build/vm-apple"
codesign --force --sign - --entitlements "$ROOT/native/entitlements.plist" "$ROOT/native/.build/vm-apple"
ln -sf ../../native/.build/vm-apple "$ROOT/.local/bin/vm-apple"
"$ROOT/native/tools/package-app.sh"
"$ROOT/.local/bin/vm-apple" probe
