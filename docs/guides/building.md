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
published). `--qemu-runtime` ships the QEMU/GPU runtime this Mac uses (the [GPU runtime](#gpu-runtime) below, with
`.local/bin/VeynodeQEMUConsole.app`, the Venus host runtime and `runtime/build-gpu-drivers.sh`'s MoltenVK and DXMT)
under `libexec/qemu/` and its licences under `LICENSES/`; `--archive` also
writes `OUTPUT.tar.gz` and its SHA-256. Its helpers, `native/.build/vm-apple` and `.local/bin/veynode-guest` come from
[the desktop helpers](#desktop-helpers), `scripts/build-native.sh` and the guest agent's `go build`; the macOS guest's
port relay from `native/tools/build-macos-relay.sh`.

```sh
cargo build --release -p veynodevm -p vm-control -p veynodevm-mcp -p vm-media-local
python3 scripts/package-release.py --output /tmp/VeynodeVM --web [--qemu-runtime] [--archive] [--sign "Developer ID Application: …"]
/tmp/VeynodeVM/bin/veynodevm daemon start
```

`--from-release DIR` makes the release of a version that changed only `crates/` on a Mac without the runtime's
build: everything but the Rust programs (the worker, helpers, boot files, guest agent, QEMU/GPU runtime and
licences) is copied from the earlier release directory `DIR`, after each file is checked against that release's
`MANIFEST.json`; the new manifest names it in `runtime_from`. `DIR` must be signed by the same identity.

With `--qemu-runtime` the release carries Venus and Neptune: `libexec/qemu/venus/` holds the render server, host
library and loader with a `registration.json` relative to itself, and the drivers sit in `libexec/qemu/Frameworks/`.
Signing changes their bytes, so the compiled Venus catalog cannot name them: a daemon admits the GPU runtime of the
package it runs from by that package's `MANIFEST.json` (every role and every file under `Frameworks/`), and any
other registration by the catalog as before. A daemon on macOS older than 26 still starts; `hello` reports Venus and
Neptune unavailable with the reason.

The directory needs no registration: the daemon reads `share/runtime.json`, whose paths are relative to it, and
`install.py` is only for pointing an existing home's `runtime.json` at it. A home's registered `runtime.json` wins
field by field (all of `qemu` at once). `daemon start` run from another directory than the one the home's
LaunchAgent names rewrites the agent and restarts the daemon onto it, which stops the machines that daemon runs.

## Desktop helpers

For the browser desktop on Apple Virtualization containers, build the host
encoder and register it (QEMU desktops use the console helper below):

```sh
./native/tools/build-vz-desktop-stream.sh   # → .local/bin/vz-desktop-stream
./native/tools/build-vm-vision.sh           # → .local/bin/vm-vision (vm_screen_text, scaled screenshots, observe)
./native/tools/build-vm-macos-desktop.sh    # → .local/bin/vm-macos-desktop (macOS desktop/accessibility tools)
./native/tools/build-macos-relay.sh         # → .local/bin/veynode-macos-relay (macOS machines' vsock port relay)
python3 scripts/install-dev-runtime.py
```

## GPU runtime

To get the GPU runtime (development only; redistribution is not yet reviewed):

```sh
./runtime/fetch-utm.sh        # pinned UTM 5.0.8 and its source patches/ → .local/runtime/utm (verified by hash)
./runtime/build-utm-loader.sh
python3.12 runtime/linux-vulkan-build-host-runtime.py --output-dir "$PWD/.local/runtime/venus/host"   # Venus only
./runtime/build-gpu-drivers.sh   # MoltenVK, DXMT and LLVM 15 from source → .local/runtime/gpu-drivers (releases only; ~1 h)
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

The Venus build scripts need meson 1.12.1 and PyYAML on PATH (a Python 3.12 venv works) and UTM's source `patches/` in `.local/runtime/utm/UTM-5.0.8` (`fetch-utm.sh` extracts them).

Moving to a new pin: `fetch-utm.sh` refuses an existing `.local/runtime/utm/UTM.app` of another version, so move it aside first, then rebuild everything above (the loaders, the Venus host runtime, the SPICE library from a fresh `.local/spice-reconnect-build`, the console) and run `register-venus.py --repin`.
