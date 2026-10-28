#!/bin/bash
# Offline checks only: no NSWindow, SPICE connection or VM. The draw-completion regression test submits small offscreen GPU command buffers.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ $# == 1 && "$1" == /*.app && -x "$1/Contents/MacOS/spice-console" ]] || { echo 'Usage: test.sh ABSOLUTE_STAGED_APP' >&2; exit 2; }
APP="$1"
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/veynode-console-test.XXXXXXXX")
trap 'rm -rf "$TEMP"' EXIT
clang -fobjc-arc -Wall -Wextra -Werror -framework Foundation "$ROOT/native/console/CopyTiming.m" "$ROOT/native/console/CopyTimingTests.m" -o "$TEMP/copy-timing-tests"
clang -fobjc-arc -Wall -Wextra -Werror -framework Foundation "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/FrameAcquisition.m" "$ROOT/native/console/InputFrameDiagnostic.m" "$ROOT/native/console/InputFrameDiagnosticTests.m" -o "$TEMP/input-frame-tests"
clang -fobjc-arc -Wall -Wextra -Werror -framework Foundation "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/FrameAcquisition.m" "$ROOT/native/console/FrameAcquisitionTests.m" -o "$TEMP/frame-acquisition-tests"
"$TEMP/frame-acquisition-tests"
"$TEMP/input-frame-tests"
"$TEMP/copy-timing-tests"
clang -fobjc-arc -Wall -Wextra -Werror -framework Foundation "$ROOT/native/console/DrawTiming.m" "$ROOT/native/console/DrawTimingTests.m" -o "$TEMP/draw-timing-tests"
"$TEMP/draw-timing-tests"
# Includes the production Display.m for its publication state; see DrawCompletionOwnershipTests.m.
clang -fobjc-arc -fblocks -Wall -Wextra -Werror -Wno-unused-parameter -DVEYNODE_NATIVE_DISPLAY -framework AppKit -framework MetalKit -framework QuartzCore -framework Foundation -framework AVFoundation -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework ImageIO -framework CoreGraphics -framework IOSurface -framework Metal -framework QuartzCore "$ROOT/native/console/FramePool.m" "$ROOT/native/console/Presented.m" "$ROOT/native/console/CopyTiming.m" "$ROOT/native/console/AsyncScreenshot.m" "$ROOT/native/console/RecordWriter.m" "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/FrameAcquisition.m" "$ROOT/native/console/InputFrameDiagnostic.m" "$ROOT/native/console/DrawTiming.m" "$ROOT/native/console/NativeControl.m" "$ROOT/native/console/DrawCompletionOwnershipTests.m" -o "$TEMP/draw-completion-ownership-tests"
"$TEMP/draw-completion-ownership-tests"
clang -fobjc-arc -Wall -Wextra -framework Foundation "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/DisplayedFrameTests.m" -o "$TEMP/displayed-frame-tests"
clang -fobjc-arc -Wall -Wextra -framework Foundation "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/FrameAcquisition.m" "$ROOT/native/console/InputFrameDiagnostic.m" "$ROOT/native/console/NativeControl.m" "$ROOT/native/console/NativeControlTests.m" -o "$TEMP/native-control-tests"
"$TEMP/native-control-tests"
"$TEMP/displayed-frame-tests"
"$APP/Contents/MacOS/spice-console" --input-frame-diagnostic-self-test
"$APP/Contents/MacOS/spice-console" --draw-timing-status-self-test
"$APP/Contents/MacOS/spice-console" --display-headless-self-test
"$APP/Contents/MacOS/spice-console" --display-self-test
"$APP/Contents/MacOS/spice-console" --display-metal-self-test
codesign --verify --strict "$APP"
"$APP/Contents/MacOS/spice-console" --probe-capabilities > "$TEMP/display.json"
BASE="$ROOT/.local/runtime/utm"
HEADERS="$BASE/CocoaSpice-127033fa3e59cd49678f49ed54f8adfc060afb56/Sources/CocoaSpice/ExternalHeaders"
FRAMEWORKS="$BASE/UTM.app/Contents/Frameworks"
clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter \
 -I"$HEADERS" -I"$HEADERS/glib-2.0" -I"$HEADERS/spice-client-glib-2.0" -I"$HEADERS/spice-1" \
 -F"$FRAMEWORKS" -Wl,-rpath,"$FRAMEWORKS" \
 -framework Foundation -framework AVFoundation -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework ImageIO -framework CoreGraphics -framework IOSurface -framework Metal \
 -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 \
 "$ROOT/native/tools/spice-console.m" -o "$TEMP/headless"
"$TEMP/headless" --probe-capabilities > "$TEMP/headless.json"
python3 - "$TEMP/display.json" "$TEMP/headless.json" <<'PY'
import json,sys
view,headless=(json.load(open(p)) for p in sys.argv[1:])
for caps in (view,headless):
    assert type(caps['native_window']) is bool
    assert caps['input_enabled'] is False and caps['display_only'] is True
assert headless['native_window_protocol']==1 and view['native_window_protocol']==2
assert view['native_input_protocol']==1 and view['native_input_supported'] is True and view['native_input_available'] is False
assert len(view['display_instance'])==32
assert headless['native_window'] is False
assert view['displayed_frame_protocol']==1
assert view['owned_slot_limit']==3 and view['display_gpu_inflight_limit']==1
assert view['capture_limit_fps']==240 and view['native_window_capture_limit_fps']==240
assert view['background_capture_limit_fps']==30 and view['encoder_limit_fps']==240
assert view['idle_encode_interval_ms']==1000 and view['display_refresh_preference_hz']==240
assert view['physical_presentation_measurement']=='unavailable'
assert view['presented_time_sample_limit']==120
assert view['presented_time_source']=='CAMetalDrawable.addPresentedHandler/presentedTime'
print('PASS independent headless build and typed capability contract; device availability:', view['native_window'])
PY
BEFORE=$(shasum -a 256 "$APP/Contents/MacOS/spice-console")
if "$ROOT/native/console/build.sh" --output-app "$APP" > "$TEMP/refuse.txt" 2>&1; then
 echo 'FAIL build replaced existing staged app' >&2; exit 1
fi
[[ "$BEFORE" == "$(shasum -a 256 "$APP/Contents/MacOS/spice-console")" ]]
echo 'PASS existing staged app refused and hash unchanged'
