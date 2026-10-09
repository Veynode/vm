#!/bin/bash
# Offline checks only: no NSWindow, SPICE connection, VM or GPU command.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ $# == 1 && "$1" == /*.app && -x "$1/Contents/MacOS/spice-console" ]] || { echo 'Usage: test.sh ABSOLUTE_STAGED_APP' >&2; exit 2; }
APP="$1"
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/veynode-console-test.XXXXXXXX")
trap 'rm -rf "$TEMP"' EXIT
clang -fobjc-arc -Wall -Wextra -framework Foundation "$ROOT/native/console/DisplayedFrame.m" "$ROOT/native/console/DisplayedFrameTests.m" -o "$TEMP/displayed-frame-tests"
clang -fobjc-arc -Wall -Wextra -framework Foundation "$ROOT/native/console/NativeControl.m" "$ROOT/native/console/NativeControlTests.m" -o "$TEMP/native-control-tests"
"$TEMP/native-control-tests"
"$TEMP/displayed-frame-tests"
"$APP/Contents/MacOS/spice-console" --display-headless-self-test
"$APP/Contents/MacOS/spice-console" --display-self-test
"$APP/Contents/MacOS/spice-console" --display-metal-self-test
codesign --verify --strict "$APP"
"$APP/Contents/MacOS/spice-console" --probe-capabilities > "$TEMP/display.json"
BASE="$ROOT/.local/windows-gpu"
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
assert view['capture_limit_fps']==60 and view['native_window_capture_limit_fps']==60
assert view['background_capture_limit_fps']==30 and view['encoder_limit_fps']==30
assert view['idle_encode_interval_ms']==1000 and view['display_refresh_preference_hz']==60
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
