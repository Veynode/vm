#!/bin/sh
# Fetch the pinned CocoaSpice source (SPICE client headers matching UTM 5.0.5's
# frameworks) into .local/runtime/utm. Verified by hash; nothing is built here.
set -eu
cd "$(dirname "$0")/.."
base="$PWD/.local/runtime/utm"
revision=127033fa3e59cd49678f49ed54f8adfc060afb56
archive="$base/cocoaspice.tar.gz"
expected=4e9f787d6edc21287f5648570ecd7fca07640c559b00c269503798f592783373
mkdir -p "$base"
if ! test -f "$archive"; then
  curl -fL --retry 2 "https://codeload.github.com/utmapp/CocoaSpice/tar.gz/$revision" -o "$archive.part"
  mv "$archive.part" "$archive"
fi
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
test "$actual" = "$expected" || { echo 'CocoaSpice source hash mismatch' >&2; exit 1; }
test -d "$base/CocoaSpice-$revision" || tar -xzf "$archive" -C "$base"
echo "$base/CocoaSpice-$revision/Sources/CocoaSpice/ExternalHeaders"
