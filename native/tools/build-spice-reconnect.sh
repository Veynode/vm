#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="$ROOT/.local/runtime/utm"
HEADERS="$BASE/CocoaSpice-127033fa3e59cd49678f49ed54f8adfc060afb56/Sources/CocoaSpice/ExternalHeaders"
FRAMEWORKS="$BASE/UTM.app/Contents/Frameworks"
clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter -I"$HEADERS" -I"$HEADERS/glib-2.0" -I"$HEADERS/spice-client-glib-2.0" -I"$HEADERS/spice-1" -F"$FRAMEWORKS" -Wl,-rpath,"$FRAMEWORKS" -framework Foundation -framework Metal -framework IOSurface -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 "$ROOT/native/tools/spice-reconnect-probe.m" -o "$ROOT/.local/bin/spice-reconnect-probe"
