#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/.local/bin"
xcrun clang -std=c11 -O2 -Wall -Wextra -Werror "$ROOT/native/tools/resource-probe.c" -o "$ROOT/.local/bin/resource-probe"
