#!/bin/sh
# Fetch the pinned CocoaSpice source (SPICE client headers matching UTM 5.0.8's
# frameworks) into .local/runtime/utm. Verified by hash; nothing is built here.
set -eu
cd "$(dirname "$0")/.."
base="$PWD/.local/runtime/utm"
revision=d8d29fc810047a3ddcebb351cdadc8fe4b4f308d
archive="$base/cocoaspice-$revision.tar.gz"
expected=13af3b3fa4e6ba9711f5c4942d513c6ab484d7f142aa801e26d72585be180cc4
mkdir -p "$base"
if ! test -f "$archive"; then
  curl -fL --retry 2 "https://codeload.github.com/utmapp/CocoaSpice/tar.gz/$revision" -o "$archive.part"
  mv "$archive.part" "$archive"
fi
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
test "$actual" = "$expected" || { echo 'CocoaSpice source hash mismatch' >&2; exit 1; }
test -d "$base/CocoaSpice-$revision" || tar -xzf "$archive" -C "$base"
echo "$base/CocoaSpice-$revision/Sources/CocoaSpice/ExternalHeaders"
