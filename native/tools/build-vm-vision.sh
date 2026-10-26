#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUTPUT="$ROOT/.local/bin/vm-vision"
if [[ $# != 0 ]]; then
 [[ $# == 2 && "$1" == --output && "$2" == /* ]] || { echo 'Usage: build-vm-vision.sh [--output ABSOLUTE_BINARY]' >&2; exit 2; }
 OUTPUT="$2"
fi
mkdir -p "$(dirname "$OUTPUT")"
xcrun swiftc -O -warnings-as-errors "$ROOT/native/tools/vm-vision.swift" -o "$OUTPUT"
