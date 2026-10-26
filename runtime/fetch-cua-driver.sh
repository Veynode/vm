#!/bin/sh
# Fetches the pinned cua-driver for a Guest platform (runtime/cua-driver.json)
# into .local/runtime/cua-driver/<platform>/, checking the archive and the
# binary against the pin. The Linux image pins the same archive itself; the
# daemon copies the Windows and macOS binaries into Guests, from the paths
# scripts/install-dev-runtime.py registers (docs/computer-use.md).
#   runtime/fetch-cua-driver.sh windows-arm64
set -eu
platform=${1:?usage: fetch-cua-driver.sh linux-arm64|windows-arm64|darwin-arm64}
root=$(cd "$(dirname "$0")/.." && pwd)
pin="$root/runtime/cua-driver.json"
field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["assets"][sys.argv[2]][sys.argv[3]])' "$pin" "$platform" "$1"; }
url=$(field url)
expected=$(field sha256)
binary=$(field binary)
binary_expected=$(field binary_sha256)
out="$root/.local/runtime/cua-driver/$platform"
archive="$out/$(basename "$url")"
mkdir -p "$out"
if [ ! -f "$archive" ] || [ "$(shasum -a 256 "$archive" | cut -d' ' -f1)" != "$expected" ]; then
    curl -fsSL --retry 3 -o "$archive.part" "$url"
    actual=$(shasum -a 256 "$archive.part" | cut -d' ' -f1)
    if [ "$actual" != "$expected" ]; then
        rm -f "$archive.part"
        echo "fetch-cua-driver: $url has sha256 $actual, pinned $expected" >&2
        exit 1
    fi
    mv "$archive.part" "$archive"
fi
case "$archive" in
*.zip) unzip -o -q "$archive" "$binary" "$(dirname "$binary")/LICENSE" "$(dirname "$binary")/THIRD_PARTY_NOTICES.md" -d "$out" ;;
*) tar -xzf "$archive" -C "$out" "$binary" "$(dirname "$binary")/LICENSE" "$(dirname "$binary")/THIRD_PARTY_NOTICES.md" ;;
esac
actual=$(shasum -a 256 "$out/$binary" | cut -d' ' -f1)
if [ "$actual" != "$binary_expected" ]; then
    echo "fetch-cua-driver: $binary has sha256 $actual, pinned $binary_expected" >&2
    exit 1
fi
echo "$out/$binary"
