#!/bin/bash
# Fetch an upstream GPU evaluation candidate without modifying /Applications.
set -euo pipefail
cd "$(dirname "$0")/.."
dest="$PWD/.local/runtime/utm"
mkdir -p "$dest"
archive="$dest/UTM-5.0.5.dmg"
if [[ ! -f "$archive" ]]; then
  curl --fail --location --retry 2 --output "$archive.partial" \
    https://github.com/utmapp/UTM/releases/download/v5.0.5/UTM.dmg
  mv "$archive.partial" "$archive"
fi
expected=713afe73c711f01344b8766654be531cd391ed2e30931206f43b5159f143764f
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
[[ "$actual" == "$expected" ]] || { echo 'UTM digest mismatch; refusing extraction.' >&2; exit 1; }
if [[ ! -e "$dest/UTM.app" ]]; then
  mountdir=$(mktemp -d "$dest/mount.XXXXXX")
  mounted=false
  cleanup() { if $mounted; then hdiutil detach "$mountdir" >/dev/null; fi; rmdir "$mountdir" 2>/dev/null || true; }
  trap cleanup EXIT
  hdiutil attach -readonly -nobrowse -mountpoint "$mountdir" "$archive"
  mounted=true
  ditto "$mountdir/UTM.app" "$dest/UTM.app"
fi
codesign --verify --deep --strict "$dest/UTM.app"
echo "Verified upstream UTM 5.0.5: $dest/UTM.app"
echo 'No VM created or started. No GPU functionality or Windows license verified.'
