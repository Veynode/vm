#!/bin/bash
# Fetch the pinned upstream UTM app, and its source's patches/ (the dependency
# pins and patches the runtime builds start from), without modifying /Applications.
set -euo pipefail
cd "$(dirname "$0")/.."
version=5.0.8
dest="$PWD/.local/runtime/utm"
mkdir -p "$dest"
fetch() { # url file sha256
  if [[ ! -f "$2" ]]; then
    curl --fail --location --retry 2 --output "$2.partial" "$1"
    mv "$2.partial" "$2"
  fi
  [[ "$(shasum -a 256 "$2" | awk '{print $1}')" == "$3" ]] || { echo "digest mismatch: $2; refusing extraction." >&2; exit 1; }
}
archive="$dest/UTM-$version.dmg"
fetch "https://github.com/utmapp/UTM/releases/download/v$version/UTM.dmg" "$archive" \
  7b2d7b2e2a02b3de1757ef19a8f36573e6d9cceae8c8cca0abcbcaa4a852019a
source="$dest/UTM-$version-src.tar.gz"
fetch "https://codeload.github.com/utmapp/UTM/tar.gz/refs/tags/v$version" "$source" \
  a264c638b8fa4f2f2fda605cd5d4a83199da87ce80787c22444dd0a6ffac6dab
[[ -d "$dest/UTM-$version/patches" ]] || tar -xzf "$source" -C "$dest" "UTM-$version/patches"
installed=$( [[ -e "$dest/UTM.app" ]] && defaults read "$dest/UTM.app/Contents/Info.plist" CFBundleShortVersionString || true)
if [[ -n "$installed" && "$installed" != "$version" ]]; then
  echo "$dest/UTM.app is UTM $installed; move it aside to fetch $version." >&2
  exit 1
fi
if [[ -z "$installed" ]]; then
  mountdir=$(mktemp -d "$dest/mount.XXXXXX")
  mounted=false
  cleanup() { if $mounted; then hdiutil detach "$mountdir" >/dev/null; fi; rmdir "$mountdir" 2>/dev/null || true; }
  trap cleanup EXIT
  hdiutil attach -readonly -nobrowse -mountpoint "$mountdir" "$archive"
  mounted=true
  ditto "$mountdir/UTM.app" "$dest/UTM.app"
fi
codesign --verify --deep --strict "$dest/UTM.app"
echo "Verified upstream UTM $version: $dest/UTM.app"
echo 'No VM created or started. No GPU functionality or Windows license verified.'
