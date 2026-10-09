#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# == 2 && "$1" == "--output" && "$2" == /* ]]; then
  OUTPUT="$2"
elif [[ $# == 0 ]]; then
  OUTPUT="$ROOT/.local/guest-share/macos-input-oracle"
else
  echo "Usage: build-input-oracle.sh [--output NEW_ABSOLUTE_PATH]" >&2; exit 2
fi
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework AppKit -framework CoreGraphics \
 "$ROOT/native/guest-macos/input-oracle.swift" -o "$OUTPUT"
