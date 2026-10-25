#!/bin/sh
# Build only our own developer diagnostic; bundled upstream code stays untouched.
set -eu
cd "$(dirname "$0")/.."
base="$PWD/.local/runtime/utm"
frameworks="$base/UTM.app/Contents/Frameworks"
test -d "$frameworks" || { echo 'Run runtime/fetch-utm.sh first' >&2; exit 1; }
clang -Wall -Wextra -Werror -Wl,-rpath,"$frameworks" runtime/utm-loader.c -o "$base/probe-loader"
cat > "$base/loader.entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>
PLIST
codesign --force --sign - --entitlements "$base/loader.entitlements" "$base/probe-loader"
"$base/probe-loader" "$frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu" --version
