#!/bin/bash
# Build the QEMU console helper with the native AppKit window into
# .local/bin/VeynodeQEMUConsole.app (development bundle, ad-hoc signed, rpath to
# .local/runtime/utm/UTM.app frameworks). scripts/install-dev-runtime.py --qemu
# registers its Contents/MacOS/spice-console with target/debug/vm-console.
# A running helper keeps its old executable; restart the daemon's VMs to use
# a rebuilt one.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ -d .local/runtime/utm/UTM.app ]] || { echo 'Run runtime/fetch-utm.sh first' >&2; exit 1; }
runtime/fetch-cocoaspice.sh >/dev/null
mkdir -p .local/bin
app="$PWD/.local/bin/VeynodeQEMUConsole.app"
new="$PWD/.local/bin/VeynodeQEMUConsole.new.app"
rm -rf "$new"
native/console/build.sh --output-app "$new"
"$new/Contents/MacOS/spice-console" --probe-capabilities
if [[ -e "$app" ]]; then
  old=$(mktemp -d "$PWD/.local/bin/.console-old.XXXXXXXX")
  mv "$app" "$old/"
  rm -rf "$old"
fi
mv "$new" "$app"
echo "$app/Contents/MacOS/spice-console"
