#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ $# == 2 && "$1" == --output && "$2" == /* ]] || { echo 'Usage: build-guest-metal-presentation.sh --output NEW_ABSOLUTE_FILE' >&2; exit 2; }
OUTPUT="$2"
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo 'Refusing existing output' >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"
SCRATCH="$(mktemp -d "$(dirname "$OUTPUT")/.guest-presentation-build.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework AppKit -framework MetalKit "$ROOT/native/tools/guest-metal-presentation.swift" -o "$SCRATCH/probe"
ln "$SCRATCH/probe" "$OUTPUT"
