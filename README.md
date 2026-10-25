# VeynodeVM

A Docker-like runtime for Apple Silicon Macs with a **GPU inside the container**, an optional **guest desktop**, and **MCP** for AI agents.
Docker on macOS gives containers no GPU. VeynodeVM runs each workload on QEMU/HVF with virtio-gpu, which provides OpenGL/GLES through virgl and Vulkan 1.2 through Venus → MoltenVK → Metal.

**Status: engineering validation, not a release.** The control plane, the GPU path, the native and browser consoles and MCP all exist and have live evidence. The Docker-like layer (OCI pull, `veynodevm run`, shared-VM docker compatibility, web UI) has not been built yet. See the [product spec](docs/product-spec.md) and [architecture](docs/architecture.md).

## Pieces

| Area | Path | State |
|---|---|---|
| Control daemon, MCP (stdio + HTTPS), leases, jobs, files | `crates/control` | implemented |
| QEMU/HVF worker with virgl / Venus GPU profiles | `crates/control/src/bin/vm-qemu.rs`, `qemu_graphics.rs` | implemented (Venus experimental) |
| Apple Virtualization worker | `native/worker` | implemented (no GPU) |
| APFS base images and writable clones | `crates/control/src/bin/vm-image.rs` | implemented |
| Console broker, hardware H.264/HEVC WebRTC | `crates/media-local`, `browser/` | localhost implemented |
| Device pairing and signed sessions | `crates/session-auth` | localhost implemented |
| Native desktop console (AppKit/Metal) | `native/console`, `native/tools/spice-console.m` | implemented |
| Linux guest agent (exec, files, cgroups) | `guest/veynode-agent` | implemented |
| GPU runtime fetch/build (UTM QEMU, virglrenderer, SPICE) | `runtime/` | development only |
| OCI images, `veynodevm` CLI, shared-VM mode, web UI | `crates/oci`, `crates/cli`, `web/` | planned |
| macOS / Windows guests | `native/guest-macos`, `native/guest-launcher` | reserved, not on main path |

Experiments, benchmarks and validation evidence from the earlier phase are in [`archive/`](archive/README.md).

## Build and check

Host: Apple Silicon, macOS 14+ (Venus requires macOS 26+). Install the Swift command-line tools, then `brew install rust go qemu squashfs`.

```sh
./scripts/check.sh            # native build, cargo fmt/build/test/clippy, go, python, browser tests
./scripts/check.sh --real     # also boots real Alpine ARM64 VMs (~416 MiB download)
```

To get the GPU runtime (development only; redistribution is not yet reviewed):

```sh
./runtime/fetch-utm.sh        # pinned UTM 5.0.5 → .local/runtime/utm (verified by hash)
./runtime/build-utm-loader.sh
```

## Use the control path today

```sh
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp  --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

The last command is a stdio MCP server. Observation is on by default; lifecycle, execute, input and files are separate opt-ins. Read the [control usage and HTTPS security boundaries](crates/control/README.md) before exposing any listener. For QEMU/GPU registration flags, see the same document.

[Native backend](native/README.md) · [Security](docs/security.md) · [Linux graphics decision](docs/decisions/0002-linux-graphics.md) · [Decisions](docs/decisions/)
