# Building and checking

## Prerequisites

Host: Apple Silicon, macOS 14+ (Venus requires macOS 26+). Install the Swift command-line tools, then `brew install rust go node qemu squashfs`.

## Checks

```sh
./scripts/check.sh            # native build, cargo fmt/build/test/clippy, go, python, browser and web tests
./scripts/check.sh --real     # also builds the kernel and boots real containers (and the engine if docker is installed)
```

Day to day, `scripts/check-changed.py` runs only the checks that the files changed against `main` need (`--quick` while editing, `--dry-run` to see the plan); see [AGENTS.md](../../AGENTS.md#verification).

## Release directory

A release directory for the standalone product and for Veynode core's plugin (nothing is built, notarized or
published; the QEMU/GPU runtime is left out):

```sh
cargo build --release -p veynodevm -p vm-control -p veynodevm-mcp -p vm-media-local
python3 scripts/package-release.py --output /tmp/VeynodeVM --web [--sign "Developer ID Application: …"]
/tmp/VeynodeVM/install.py && /tmp/VeynodeVM/bin/veynodevm daemon start
```

## Desktop helpers

For the browser desktop on Apple Virtualization containers, build the host
encoder and register it (QEMU desktops use the console helper below):

```sh
./native/tools/build-vz-desktop-stream.sh   # → .local/bin/vz-desktop-stream
./native/tools/build-vm-vision.sh           # → .local/bin/vm-vision (vm_screen_text, scaled screenshots, observe)
./native/tools/build-vm-macos-desktop.sh    # → .local/bin/vm-macos-desktop (macOS desktop/accessibility tools)
python3 scripts/install-dev-runtime.py
```

## GPU runtime

To get the GPU runtime (development only; redistribution is not yet reviewed):

```sh
./runtime/fetch-utm.sh        # pinned UTM 5.0.5 → .local/runtime/utm (verified by hash)
./runtime/build-utm-loader.sh
python3.12 runtime/linux-vulkan-build-host-runtime.py --output-dir "$PWD/.local/runtime/venus/host"   # Venus only
python3.12 runtime/build-spice-reconnect-server.py
python3.12 runtime/register-venus.py --host-dir .local/runtime/venus/host
./runtime/build-qemu-console.sh   # pinned CocoaSpice headers (hash-checked) + native window helper → .local/bin/VeynodeQEMUConsole.app
python3 scripts/install-dev-runtime.py --qemu
```

`build-qemu-console.sh` fetches the CocoaSpice source with `runtime/fetch-cocoaspice.sh`
and builds `native/console` against UTM's SPICE client frameworks; `install-dev-runtime.py`
then registers it with the console broker (`target/debug/vm-console`, from `cargo build`).
That pair gives QEMU containers the persistent console (screenshots) and the `veynodevm desktop` window.
Check the helper offline with `native/console/test.sh "$PWD/.local/bin/VeynodeQEMUConsole.app"`;
restart the daemon after rebuilding it.

The Venus build scripts need meson 1.12.1 and PyYAML on PATH (a Python 3.12 venv works) and UTM's source `patches/` in `.local/runtime/utm/UTM-5.0.5`.
