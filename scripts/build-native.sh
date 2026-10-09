#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo 'Apple Silicon macOS is required' >&2; exit 1; }
if [[ $# -gt 0 ]]; then
  [[ $# -eq 2 && "$1" == --output-app && "$2" == /* && "$2" == *.app ]] || { echo 'Usage: build-native.sh [--output-app /absolute/fresh.app]' >&2; exit 1; }
  APP="$2"
  [[ ! -e "$APP" && ! -L "$APP" ]] || { echo 'Staged app output already exists; choose a fresh path' >&2; exit 1; }
  mkdir -p "$ROOT/.local"
  STAGE="$(mktemp -d "$ROOT/.local/native-build.XXXXXX")"
  trap 'rm -rf "$STAGE"' EXIT
  xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Virtualization -framework AppKit "$ROOT/native/Sources/main.swift" "$ROOT/native/Sources/GuestStreamBridge.swift" -o "$STAGE/vm-apple"
  "$ROOT/native/tools/package-app.sh" "$STAGE/vm-apple" "$APP"
  "$APP/Contents/MacOS/vm-apple" probe
  exit 0
fi
# Legacy publication replaces fixed development paths; do not touch a live worker.
if pgrep -x vm-apple >/dev/null; then
  echo 'A VM worker is running. Use --output-app /absolute/fresh.app for an isolated build.' >&2
  exit 1
fi
mkdir -p "$ROOT/native/.build" "$ROOT/.local/bin"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Virtualization -framework AppKit "$ROOT/native/Sources/main.swift" "$ROOT/native/Sources/GuestStreamBridge.swift" -o "$ROOT/native/.build/vm-apple"
codesign --force --sign - --entitlements "$ROOT/native/entitlements.plist" "$ROOT/native/.build/vm-apple"
ln -sf ../../native/.build/vm-apple "$ROOT/.local/bin/vm-apple"
"$ROOT/native/tools/package-app.sh"
"$ROOT/.local/bin/vm-apple" probe
