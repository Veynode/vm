#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$ROOT/.local/bin}"
mkdir -p "$OUT"
xcrun clang -fobjc-arc -dynamiclib -O2 -Wall -Werror -target arm64-apple-macosx14.0 -framework Metal -framework Foundation \
  "$ROOT/native/guest-metal-fix/metalfix.m" -o "$OUT/metalfix.dylib"
codesign --force --sign - "$OUT/metalfix.dylib"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 "$ROOT/native/guest-metal-fix/probe.swift" -o "$OUT/metal-probe"
echo "$OUT/metalfix.dylib"
echo "$OUT/metal-probe"
