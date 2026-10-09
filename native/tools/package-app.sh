#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP="$ROOT/.local/VeynodeVM.app"
SOURCE="$ROOT/native/.build/vm-apple"
if [[ $# -gt 0 ]]; then
  [[ $# -eq 2 && "$1" == /* && "$2" == /* && "$2" == *.app ]] || { echo 'Usage: package-app.sh [absolute-binary /absolute/fresh.app]' >&2; exit 1; }
  SOURCE="$1"; APP="$2"
  [[ ! -e "$APP" && ! -L "$APP" ]] || { echo 'Staged app output already exists' >&2; exit 1; }
elif pgrep -x vm-apple >/dev/null; then
  echo 'Refusing to replace the development worker app while a VM worker is running.' >&2
  exit 1
fi
mkdir -p "$APP/Contents/MacOS"
cp "$SOURCE" "$APP/Contents/MacOS/vm-apple"
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
