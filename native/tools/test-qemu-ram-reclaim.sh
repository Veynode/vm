#!/bin/sh
# Builds and runs qemu-ram-reclaim-test.c against a stand-in QEMU image.
# Usage: test-qemu-ram-reclaim.sh [--output DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
if [ "${1:-}" = --output ]; then out=$2; mkdir -p "$out"; fi
flags="-Wall -Wextra -Werror -O1"
clang $flags -DQRR_TEST_IMAGE -dynamiclib -framework Hypervisor "$here/qemu-ram-reclaim-test.c" -o "$out/qemu-ram-reclaim-image.dylib"
clang $flags "$here/qemu-ram-reclaim-test.c" -o "$out/qemu-ram-reclaim-test"
cat > "$out/hypervisor.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>
PLIST
codesign --force --sign - --entitlements "$out/hypervisor.plist" "$out/qemu-ram-reclaim-test" 2>/dev/null
"$out/qemu-ram-reclaim-test" "$out/qemu-ram-reclaim-image.dylib"
