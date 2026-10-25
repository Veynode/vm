#!/bin/sh
# Developer-only consumer; does not modify or re-sign upstream dependencies.
set -eu
cd "$(dirname "$0")/.."
base="$PWD/.local/runtime/utm"
revision=127033fa3e59cd49678f49ed54f8adfc060afb56
archive="$base/cocoaspice.tar.gz"
expected=4e9f787d6edc21287f5648570ecd7fca07640c559b00c269503798f592783373
if ! test -f "$archive"; then
  curl -fL --retry 2 "https://codeload.github.com/utmapp/CocoaSpice/tar.gz/$revision" -o "$archive.part"
  mv "$archive.part" "$archive"
fi
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
test "$actual" = "$expected" || { echo 'CocoaSpice source hash mismatch' >&2; exit 1; }
test -d "$base/CocoaSpice-$revision" || tar -xzf "$archive" -C "$base"
headers="$base/CocoaSpice-$revision/Sources/CocoaSpice/ExternalHeaders"
frameworks="$base/UTM.app/Contents/Frameworks"
clang -fobjc-arc -Wall -Wextra -Wno-unused-parameter \
  -I"$headers" -I"$headers/glib-2.0" -I"$headers/spice-client-glib-2.0" -I"$headers/spice-1" \
  -F"$frameworks" -Wl,-rpath,"$frameworks" \
  -framework Foundation -framework Metal -framework CoreImage -framework ImageIO -framework IOSurface -framework CoreGraphics \
  -framework spice-client-glib-2.0.8 -framework glib-2.0.0 -framework gobject-2.0.0 -framework gio-2.0.0 \
  runtime/spice-capture.m -o "$base/spice-capture"
