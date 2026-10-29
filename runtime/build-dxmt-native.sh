#!/bin/sh
# Build UTM's DXMT (the commit UTM 5.0.8 ships) for the host, optionally with
# runtime/dxmt-gpu-completed-event.patch, into .local/runtime/dxmt/<variant>/.
#   runtime/build-dxmt-native.sh [stock|event]
# Needs Homebrew llvm@15, meson and ninja, and Xcode's Metal Toolchain
# (xcodebuild -downloadComponent MetalToolchain). Not wired into the worker:
# the Neptune profile still loads UTM's dxmt-native until a live guest run
# shows the patched build helps (docs/known-issues.md, Windows GPU).
set -eu
cd "$(dirname "$0")/.."
variant=${1:-event}
case $variant in stock|event) ;; *) echo "usage: $0 [stock|event]" >&2; exit 2 ;; esac
repo=https://github.com/utmapp/dxmt.git
commit=66e27f3759ff79e41a791ec40e2a70ef6d0c964c
llvm=$(brew --prefix llvm@15)
patch="$PWD/runtime/dxmt-gpu-completed-event.patch"
out="$PWD/.local/runtime/dxmt/$variant"
# Under /private/tmp, outside the checkout: meson and the Metal compiler
# write several hundred MB of objects.
work=$(mktemp -d /private/tmp/dxmt-build.XXXXXX)
trap 'rm -rf "$work"' EXIT
git init -q "$work/src"
git -C "$work/src" fetch -q --depth 1 "$repo" "$commit"
git -C "$work/src" checkout -q FETCH_HEAD
git -C "$work/src" submodule update -q --init --depth 1 include/native/directx
if [ "$variant" = event ]; then
  git -C "$work/src" apply "$patch"
fi
meson setup "$work/build" "$work/src" -Dbuildtype=release -Dnative_llvm_path="$llvm" >/dev/null
ninja -C "$work/build" >/dev/null
mkdir -p "$out"
cp "$work/build/src/dxmt-native/libdxmt-native.dylib" "$out/dxmt-native"
shasum -a 256 "$out/dxmt-native"
