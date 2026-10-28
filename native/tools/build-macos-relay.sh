#!/bin/bash
# The macOS Guest's vsock port relay (native/guest-relay/relay.c), which the
# daemon writes into a macOS machine's disk at machine.create.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUTPUT="$ROOT/.local/bin/veynode-macos-relay"
if [[ $# != 0 ]]; then
 [[ $# == 2 && "$1" == --output && "$2" == /* ]] || { echo 'Usage: build-macos-relay.sh [--output ABSOLUTE_BINARY]' >&2; exit 2; }
 OUTPUT="$2"
fi
mkdir -p "$(dirname "$OUTPUT")"
xcrun clang -O2 -Wall -Wextra -Werror -target arm64-apple-macos13.0 "$ROOT/native/guest-relay/relay.c" -o "$OUTPUT"
codesign --force --sign - "$OUTPUT"
