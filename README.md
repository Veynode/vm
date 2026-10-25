# VeynodeVM

A Docker-like runtime for Apple Silicon Macs with a **GPU inside the container**, an optional **guest desktop**, and **MCP** for AI agents.
Docker on macOS gives containers no GPU. VeynodeVM runs each workload on QEMU/HVF with virtio-gpu, which provides OpenGL/GLES through virgl and Vulkan 1.2 through Venus → MoltenVK → Metal.

**Status: engineering preview, not a release.** The Docker-like layer works end to end on Apple Virtualization: OCI pull, micro-VM `run`/`exec`/`logs`/`stop`/`rm`, `-p`/`-v`, the shared docker engine (`docker` CLI, compose, `build`), the web UI, MCP container tools and the 2D guest desktop window. On QEMU/HVF with the locally fetched UTM runtime, `--gpus virgl` (OpenGL 2.1 / GLES 3.0) and `--gpus venus` (Vulkan 1.3, `Virtio-GPU Venus (Apple M3 Pro)`, pixel-checked offscreen rendering) are verified; Venus display presentation is not working yet. See [known issues](docs/known-issues.md), the [product spec](docs/product-spec.md) and [architecture](docs/architecture.md).

## Quick start

```sh
cargo build -p veynodevm -p vm-control && ./scripts/build-native.sh
python3 guest/kernel/build.py                  # pinned kernel + initramfs with the agent as PID 1
python3 scripts/install-dev-runtime.py --dns 1.1.1.1
veynodevm daemon start                         # launchd user agent

veynodevm run --rm alpine echo hi              # one micro-VM per container
veynodevm run -d -p 8080:80 -v $PWD:/data:ro --name web nginx:alpine
veynodevm ps -a; veynodevm logs -f web; veynodevm exec web nginx -v
veynodevm stop web && veynodevm rm web && veynodevm image gc

veynodevm engine start                         # shared docker-compatible VM
docker --context veynodevm compose up -d
veynodevm run --shared -p 8081:80 nginx:alpine
veynodevm build -t myimg .                     # build in the engine, import for micro-VMs

veynodevm run -d --desktop --name desk veynodevm/alpine-desktop
veynodevm desktop desk                         # native AppKit window
veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu   # vulkaninfo: Virtio-GPU Venus (Apple M…)

veynodevm ui --open                            # TanStack Start web UI on 127.0.0.1
veynodevm mcp --lifecycle                      # stdio MCP server
```

State lives in `~/Library/Application Support/VeynodeVM` (override with `VEYNODEVM_HOME`).

## Pieces

| Area | Path | State |
|---|---|---|
| `veynodevm` CLI (run, ps, logs, exec, build, engine, desktop, ui, mcp) | `crates/cli` | implemented |
| OCI registry client, blob store, layer flattening, squashfs roots | `crates/oci` | implemented |
| Pinned kernel, initramfs, agent as container init | `guest/kernel`, `guest/veynode-agent` | implemented |
| Shared engine VM (dockerd, docker context, dynamic port publishing) | `crates/cli/src/engine.rs` | implemented (no GPU verified) |
| Web management UI | `web/` (TanStack Start) | implemented (no exec TTY, no WebRTC desktop yet) |
| Official desktop and GPU images | `guest/images` | alpine-desktop verified on VZ and virgl; ubuntu-gpu verified on virgl and Venus |
| Control daemon, MCP (stdio + HTTPS), leases, jobs, files | `crates/control` | implemented |
| QEMU/HVF worker with virgl / Venus GPU profiles | `crates/control/src/bin/vm-qemu.rs`, `qemu_graphics.rs` | implemented (Venus experimental) |
| Apple Virtualization worker | `native/worker` | implemented (2D display, no GPU) |
| APFS base images and writable clones | `crates/control/src/bin/vm-image.rs` | implemented |
| Console broker, hardware H.264/HEVC WebRTC | `crates/media-local`, `browser/` | localhost implemented, not yet in the web UI |
| Device pairing and signed sessions | `crates/session-auth` | localhost implemented |
| Native desktop console (AppKit/Metal) | `native/console`, `native/tools/spice-console.m` | implemented |
| GPU runtime fetch/build (UTM QEMU, virglrenderer, SPICE) | `runtime/` | development only |
| macOS / Windows guests | `native/guest-macos`, `native/guest-launcher` | reserved, not on main path |

Experiments, benchmarks and validation evidence from the earlier phase are in [`archive/`](archive/README.md).

## Build and check

Host: Apple Silicon, macOS 14+ (Venus requires macOS 26+). Install the Swift command-line tools, then `brew install rust go node qemu squashfs`.

```sh
./scripts/check.sh            # native build, cargo fmt/build/test/clippy, go, python, browser and web tests
./scripts/check.sh --real     # also builds the kernel and boots real containers (and the engine if docker is installed)
```

To get the GPU runtime (development only; redistribution is not yet reviewed):

```sh
./runtime/fetch-utm.sh        # pinned UTM 5.0.5 → .local/runtime/utm (verified by hash)
./runtime/build-utm-loader.sh
python3.12 runtime/linux-vulkan-build-host-runtime.py --output-dir "$PWD/.local/runtime/venus/host"   # Venus only
python3.12 runtime/build-spice-reconnect-server.py
python3.12 runtime/register-venus.py --host-dir .local/runtime/venus/host
python3 scripts/install-dev-runtime.py --qemu
```

The Venus build scripts need meson 1.12.1 and PyYAML on PATH (a Python 3.12 venv works) and UTM's source `patches/` in `.local/runtime/utm/UTM-5.0.5`.

## Low-level control path

```sh
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp  --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

The last command is a stdio MCP server. Observation is on by default; lifecycle, execute, input and files are separate opt-ins. Read the [control usage and HTTPS security boundaries](crates/control/README.md) before exposing any listener. For QEMU/GPU registration flags, see the same document.

[Native backend](native/README.md) · [Security](docs/security.md) · [Linux graphics decision](docs/decisions/0002-linux-graphics.md) · [Decisions](docs/decisions/)
