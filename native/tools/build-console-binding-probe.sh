#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# == 2 && "$1" == "--output" && "$2" == /* ]]; then
  OUTPUT="$2"
elif [[ $# == 0 ]]; then
  OUTPUT="$ROOT/.local/guest-share/console-binding-probe-stage1"
else
  echo "Usage: build-console-binding-probe.sh [--output NEW_ABSOLUTE_PATH]" >&2; exit 2
fi
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Refusing existing output" >&2; exit 2; }
mkdir -p "$(dirname "$OUTPUT")"
SCRATCH="$(mktemp -d "$(dirname "$OUTPUT")/.console-binding-build.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 \
  -framework Security -framework CoreGraphics -framework SystemConfiguration \
  "$ROOT/native/guest-macos/console-binding.swift" "$ROOT/native/tools/console-binding-probe.swift" \
  -o "$SCRATCH/probe"
# link is no-clobber even if another builder creates OUTPUT during compilation.
ln "$SCRATCH/probe" "$OUTPUT"
