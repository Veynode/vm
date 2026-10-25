#!/bin/bash
# No VM, SPICE server, NSWindow, Metal device or GPU command submission.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/veynode-screenshot-test.XXXXXXXX")
trap 'rm -rf "$TEMP"' EXIT
clang -fobjc-arc -fblocks -Wall -Wextra -Werror -framework Foundation "$ROOT/native/console/RecordWriter.m" "$ROOT/native/console/RecordWriterAdversarialTests.m" -o "$TEMP/writer"
"$TEMP/writer"
clang -fobjc-arc -fblocks -Wall -Wextra -Werror -DVEYNODE_SCREENSHOT_TESTING -framework Foundation -framework CoreVideo -framework CoreGraphics -framework ImageIO -framework IOSurface "$ROOT/native/console/FramePool.m" "$ROOT/native/console/AsyncScreenshot.m" "$ROOT/native/console/AsyncScreenshotTests.m" -o "$TEMP/screenshot"
"$TEMP/screenshot"
BASE="$ROOT/.local/runtime/utm"
HEADERS="$BASE/CocoaSpice-127033fa3e59cd49678f49ed54f8adfc060afb56/Sources/CocoaSpice/ExternalHeaders"
FRAMEWORKS="$BASE/UTM.app/Contents/Frameworks"
clang -fobjc-arc -fblocks -Wall -Wextra -Wno-unused-parameter -DVEYNODE_NATIVE_DISPLAY -DVEYNODE_SCREENSHOT_TESTING \
 -I"$HEADERS" -I"$HEADERS/glib-2.0" -I"$HEADERS/spice-client-glib-2.0" -I"$HEADERS/spice-1" \
 -F"$FRAMEWORKS" -Wl,-rpath,"$FRAMEWORKS" \
 -framework AppKit -framework MetalKit -framework Foundation -framework AVFoundation -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework ImageIO -framework CoreGraphics -framework IOSurface -framework Metal \
 -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 \
 "$ROOT/native/tools/spice-console.m" "$ROOT/native/console/FramePool.m" "$ROOT/native/console/Presented.m" "$ROOT/native/console/CopyTiming.m" "$ROOT/native/console/AsyncScreenshot.m" "$ROOT/native/console/RecordWriter.m" "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/NativeControl.m" "$ROOT/native/console/Display.m" \
 -o "$TEMP/owner"
"$TEMP/owner" --screenshot-pipeline-self-test
