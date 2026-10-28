#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ $# == 2 && "$1" == --output-app && "$2" == /*.app ]] || { echo 'Usage: build.sh --output-app NEW_ABSOLUTE.app' >&2; exit 2; }
OUTPUT="$2"
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo 'Refusing existing app' >&2;exit 2; }
BASE="$ROOT/.local/runtime/utm"
HEADERS="$BASE/CocoaSpice-127033fa3e59cd49678f49ed54f8adfc060afb56/Sources/CocoaSpice/ExternalHeaders"
FRAMEWORKS="$BASE/UTM.app/Contents/Frameworks"
[[ -d "$HEADERS" && -d "$FRAMEWORKS" ]] || { echo 'Run runtime/fetch-utm.sh and runtime/fetch-cocoaspice.sh first (or runtime/build-qemu-console.sh)' >&2; exit 1; }
mkdir -p "$(dirname "$OUTPUT")"
TEMP=$(mktemp -d "$(dirname "$OUTPUT")/.console-build.XXXXXXXX")
trap 'rm -rf "$TEMP"' EXIT
mkdir -p "$TEMP/Console.app/Contents/MacOS"
clang -fobjc-arc -fblocks -Wall -Wextra -Wno-unused-parameter -DVEYNODE_NATIVE_DISPLAY \
 -I"$HEADERS" -I"$HEADERS/glib-2.0" -I"$HEADERS/spice-client-glib-2.0" -I"$HEADERS/spice-1" \
 -F"$FRAMEWORKS" -Wl,-rpath,"$FRAMEWORKS" \
 -framework AppKit -framework MetalKit -framework QuartzCore -framework Foundation -framework AVFoundation -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework ImageIO -framework CoreGraphics -framework IOSurface -framework Metal \
 -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 \
 "$ROOT/native/tools/spice-console.m" "$ROOT/native/console/FramePool.m" "$ROOT/native/console/Presented.m" "$ROOT/native/console/CopyTiming.m" "$ROOT/native/console/AsyncScreenshot.m" "$ROOT/native/console/RecordWriter.m" "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/FrameAcquisition.m" "$ROOT/native/console/InputFrameDiagnostic.m" "$ROOT/native/console/DrawTiming.m" "$ROOT/native/console/NativeControl.m" "$ROOT/native/console/Media.m" "$ROOT/native/console/Display.m" \
 -o "$TEMP/Console.app/Contents/MacOS/spice-console"
cat > "$TEMP/Console.app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>CFBundleExecutable</key><string>spice-console</string><key>CFBundleIdentifier</key><string>com.veynode.qemu-console.dev</string><key>CFBundleName</key><string>Veynode VM Console</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>1</string><key>LSUIElement</key><true/><key>NSHighResolutionCapable</key><true/><key>NSMicrophoneUsageDescription</key><string>The VM hears your microphone while you turn it on in the console window.</string></dict></plist>
PLIST
codesign --force --sign - "$TEMP/Console.app"
codesign --verify --strict "$TEMP/Console.app"
mv "$TEMP/Console.app" "$OUTPUT"
shasum -a 256 "$OUTPUT/Contents/MacOS/spice-console"
