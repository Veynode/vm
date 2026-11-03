#!/bin/bash
# Build the GPU drivers the Venus and Neptune profiles load, from the source
# UTM 5.0.8 pins (docs/decisions/0026-gpu-runtime-source-build.md), into
# .local/runtime/gpu-drivers/:
#   MoltenVK.framework     UTM's MoltenVK (Vulkan on Metal: Venus's driver)
#   dxmt-native.framework  UTM's DXMT (Direct3D on Metal: Neptune's backend),
#                          with LLVM 15 built from source and linked in statically
#   MoltenVK_icd.json      the Vulkan ICD, pointing at the framework as UTM's does
#   manifest.json          sources, commits and SHA-256 of what was built
# scripts/package-release.py --qemu-runtime ships them with the render server
# (runtime/linux-vulkan-build-host-runtime.py). Unsigned; the package signs.
#   runtime/build-gpu-drivers.sh [WORK]
# WORK (default a new /private/tmp directory, removed afterwards) keeps the
# sources and builds; a step whose output is already in WORK is skipped.
# Needs Xcode with its Metal Toolchain (xcodebuild -downloadComponent
# MetalToolchain), cmake, ninja, meson, and runtime/fetch-utm.sh's patches.
set -euo pipefail
cd "$(dirname "$0")/.."
llvm_version=15.0.7
llvm_sha=8b5fcb24b4128cf04df1b0b9410ce8b1a729cb3c544e6da885d234280dedeac6
moltenvk_commit=0790efd55b6b302e19487e5654f7fc30f0bdf3df
dxmt_commit=66e27f3759ff79e41a791ec40e2a70ef6d0c964c
patches="$PWD/.local/runtime/utm/UTM-5.0.8/patches"
out="$PWD/.local/runtime/gpu-drivers"
[[ -f "$patches/llvm-project-$llvm_version.src.patch" ]] || { echo "run runtime/fetch-utm.sh first: $patches" >&2; exit 1; }
[[ ! -e "$out" ]] || { echo "$out exists; move it aside" >&2; exit 1; }
if [[ $# -gt 0 ]]; then
  work=$(cd "$1" && pwd)
else
  work=$(mktemp -d /private/tmp/gpu-drivers.XXXXXX)
  trap 'rm -rf "$work"' EXIT
fi
# DXMT's minimum, as UTM builds it; MoltenVK keeps its project's own.
export MACOSX_DEPLOYMENT_TARGET=13.0
# A stalled GitHub transfer fails instead of hanging; each fetch retries.
export GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=60
retry() { for _ in 1 2 3 4; do "$@" && return; sleep 5; done; return 1; }
clone() { # repo commit dir
  [[ -d "$3/.git" ]] && return
  git init -q "$3"
  retry git -C "$3" fetch -q --depth 1 "$1" "$2"
  git -C "$3" checkout -q FETCH_HEAD
}

# LLVM 15, static, only what DXMT's shader converter links (UTM's flags and patch).
llvm="$work/llvm15"
if [[ ! -f "$llvm/lib/libLLVMCore.a" ]]; then
  src="$work/llvm-project-$llvm_version.src"
  archive="$work/llvm-project-$llvm_version.src.tar.xz"
  [[ -f "$archive" ]] || retry curl -sSfL -o "$archive" \
    "https://github.com/llvm/llvm-project/releases/download/llvmorg-$llvm_version/llvm-project-$llvm_version.src.tar.xz"
  [[ "$(shasum -a 256 "$archive" | awk '{print $1}')" == "$llvm_sha" ]] || { echo "LLVM source digest mismatch" >&2; exit 1; }
  rm -rf "$src"
  tar -xJf "$archive" -C "$work"
  patch -d "$src" -p1 -s < "$patches/llvm-project-$llvm_version.src.patch"
  cmake -G Ninja -S "$src/llvm" -B "$src/build" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$llvm" \
    -DCMAKE_OSX_ARCHITECTURES=arm64 -DLLVM_ENABLE_ZSTD=Off -DLLVM_TARGETS_TO_BUILD="" -DLLVM_BUILD_TOOLS=Off \
    -DLLVM_VERSION_PRINTER_SHOW_HOST_TARGET_INFO=Off -DLLVM_ENABLE_TERMINFO=Off -DLLVM_ENABLE_LIBXML2=Off \
    -DLLVM_INCLUDE_TESTS=Off -DLLVM_INCLUDE_BENCHMARKS=Off -DLLVM_INCLUDE_EXAMPLES=Off >"$work/llvm.log"
  ninja -C "$src/build" install >>"$work/llvm.log"
fi

# DXMT's native Direct3D library (no Wine), against that LLVM.
dxmt="$work/dxmt-build/src/dxmt-native/libdxmt-native.dylib"
if [[ ! -f "$dxmt" ]]; then
  clone https://github.com/utmapp/dxmt.git "$dxmt_commit" "$work/dxmt"
  retry git -C "$work/dxmt" submodule update -q --init --depth 1 include/native/directx
  rm -rf "$work/dxmt-build"
  meson setup "$work/dxmt-build" "$work/dxmt" -Dbuildtype=release -Dnative_llvm_path="$llvm" >"$work/dxmt.log"
  ninja -C "$work/dxmt-build" >>"$work/dxmt.log"
fi

# MoltenVK and the SPIRV-Cross, SPIRV-Tools and headers its ExternalRevisions pin.
moltenvk="$work/MoltenVK/Package/Release/MoltenVK/dynamic/dylib/macOS/libMoltenVK.dylib"
if [[ ! -f "$moltenvk" ]]; then
  clone https://github.com/utmapp/MoltenVK.git "$moltenvk_commit" "$work/MoltenVK"
  (cd "$work/MoltenVK" && retry ./fetchDependencies --macos && make macos) >"$work/moltenvk.log" 2>&1
fi

# Each library as the framework UTM ships it as.
framework() { # dylib name
  local dir="$out/$2.framework"
  mkdir -p "$dir/Versions/A/Resources"
  lipo "$1" -thin arm64 -output "$dir/Versions/A/$2" 2>/dev/null || cp "$1" "$dir/Versions/A/$2"
  install_name_tool -id "@rpath/$2.framework/Versions/A/$2" "$dir/Versions/A/$2"
  cat >"$dir/Versions/A/Resources/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>$2</string>
<key>CFBundleIdentifier</key><string>com.veynode.vm.$2</string>
<key>CFBundleName</key><string>$2</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleShortVersionString</key><string>$3</string>
<key>CFBundleVersion</key><string>$3</string>
</dict></plist>
EOF
  ln -s A "$dir/Versions/Current"
  ln -s "Versions/Current/$2" "$dir/$2"
  ln -s Versions/Current/Resources "$dir/Resources"
}
mkdir -p "$out"
framework "$moltenvk" MoltenVK "${moltenvk_commit:0:8}"
framework "$dxmt" dxmt-native "${dxmt_commit:0:8}"
sed -E 's|("library_path"[[:space:]]*:[[:space:]]*")[^"]*(")|\1../../../Frameworks/MoltenVK.framework/Versions/Current/MoltenVK\2|' \
  "$work/MoltenVK/MoltenVK/icd/MoltenVK_icd.json" >"$out/MoltenVK_icd.json"
for binary in "$out"/*.framework/Versions/A/*; do
  [[ -f "$binary" && "$binary" != */Resources ]] || continue
  if otool -L "$binary" | tail -n +2 | grep -Ev '^\s+(/System/Library/|/usr/lib/|@rpath/(MoltenVK|dxmt-native)\.framework)' ; then
    echo "$binary links outside the system and itself" >&2; exit 1
  fi
done
python3 - "$out" "$llvm_version" "$llvm_sha" "$moltenvk_commit" "$dxmt_commit" "$work/MoltenVK/ExternalRevisions" <<'EOF'
import hashlib, json, pathlib, sys
out, llvm, llvm_sha, moltenvk, dxmt, revisions = sys.argv[1:]
out = pathlib.Path(out)
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
manifest = {
    'sources': {
        'MoltenVK': {'repo': 'https://github.com/utmapp/MoltenVK', 'commit': moltenvk,
                     'external': {p.name.removesuffix('_repo_revision'): p.read_text().strip()
                                  for p in sorted(pathlib.Path(revisions).glob('*_repo_revision'))}},
        'DXMT': {'repo': 'https://github.com/utmapp/dxmt', 'commit': dxmt},
        'LLVM': {'archive': f'llvm-project-{llvm}.src.tar.xz', 'sha256': llvm_sha,
                 'patch': f'UTM 5.0.8 patches/llvm-project-{llvm}.src.patch'},
    },
    'files': {str(p.relative_to(out)): sha(p) for p in sorted(out.rglob('*')) if p.is_file() and not p.is_symlink()},
}
(out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
EOF
echo "built $out"
