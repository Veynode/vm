#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS is required' >&2; exit 1; }
mkdir -p "$ROOT/native/.build"
BUILD="$(mktemp -d "$ROOT/native/.build/guest-stream-test.XXXXXX")"
trap 'rm -rf "$BUILD"' EXIT
xcrun swiftc -swift-version 5 "$ROOT/native/worker/GuestStreamBridge.swift" \
  "$ROOT/native/tests/guest-stream-bridge.swift" -o "$BUILD/host-tests"
"$BUILD/host-tests"
