#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP="$ROOT/.local/VeynodeVM.app"
mkdir -p "$APP/Contents/MacOS"
cp "$ROOT/native/.build/vm-apple" "$APP/Contents/MacOS/vm-apple"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.veynode.vmworker.dev</string>
<key>CFBundleExecutable</key><string>vm-apple</string>
<key>CFBundleName</key><string>Veynode VM Worker</string>
<key>CFBundleDisplayName</key><string>Veynode VM Worker</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
codesign --force --sign - --entitlements "$ROOT/native/entitlements.plist" "$APP"
