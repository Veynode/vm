#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/.local/bin"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework Metal "$ROOT/native/tools/metal-probe.swift" -o "$ROOT/.local/bin/metal-probe"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx14.0 -framework VideoToolbox "$ROOT/native/tools/h264-probe.swift" -o "$ROOT/.local/bin/h264-probe"
