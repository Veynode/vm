#!/bin/bash
# The desktop and accessibility helper that macOS containers run over SSH.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUTPUT="$ROOT/.local/bin/vm-macos-desktop"
if [[ $# != 0 ]]; then
 [[ $# == 2 && "$1" == --output && "$2" == /* ]] || { echo 'Usage: build-vm-macos-desktop.sh [--output ABSOLUTE_BINARY]' >&2; exit 2; }
 OUTPUT="$2"
fi
mkdir -p "$(dirname "$OUTPUT")"
# Guests run Tart images from macOS 14 on.
xcrun swiftc -O -warnings-as-errors -target arm64-apple-macos14 "$ROOT/native/tools/vm-macos-desktop.swift" -o "$OUTPUT"
