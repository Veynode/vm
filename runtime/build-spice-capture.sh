#!/bin/sh
# Developer-only consumer; does not modify or re-sign upstream dependencies.
set -eu
cd "$(dirname "$0")/.."
base="$PWD/.local/runtime/utm"
headers=$(runtime/fetch-cocoaspice.sh)
frameworks="$base/UTM.app/Contents/Frameworks"
clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter \
  -I"$headers" -I"$headers/glib-2.0" -I"$headers/spice-client-glib-2.0" -I"$headers/spice-1" \
  -F"$frameworks" -Wl,-rpath,"$frameworks" \
  -framework Foundation -framework Metal -framework CoreImage -framework ImageIO -framework IOSurface -framework CoreGraphics \
  -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 \
  runtime/spice-capture.m -o "$base/spice-capture"
