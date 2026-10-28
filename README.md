<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/veynode-icon-dark.svg">
    <img src="docs/assets/veynode-icon.svg" width="128" height="128" alt="Veynode">
  </picture>
</p>

<h1 align="center">VeynodeVM</h1>

<p align="center">
  Docker-like micro-VMs for Apple Silicon, with a <b>GPU inside the container</b>,<br>
  a <b>desktop</b> when you want one, and <b>MCP</b> for AI agents.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/macOS-14%2B-black?logo=apple" alt="macOS 14+">
  <img src="https://img.shields.io/badge/Apple%20Silicon-arm64-555" alt="Apple Silicon">
  <img src="https://img.shields.io/badge/Rust-2021-orange?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/Go-guest%20agent-00ADD8?logo=go&logoColor=white" alt="Go"></p>

<p align="center">
  <a href="#-quick-start">Quick start</a> ·
  <a href="#-documentation">Documentation</a> ·
  <a href="docs/known-issues.md">Known issues</a> ·
  <a href="docs/architecture.md">Architecture</a>
</p>

---

Docker on macOS gives containers no GPU. VeynodeVM runs each container as its own micro-VM, on QEMU/HVF with virtio-gpu (OpenGL/GLES through virgl, Vulkan through Venus → MoltenVK → Metal) or on Apple Virtualization. A control daemon owns every VM and serves the CLI, a web UI and MCP.

## ✨ Highlights

- 🐳 **Docker-like CLI.** OCI pull, `run` / `exec` / `logs` / `stop` / `rm`, interactive `-it` and `attach`, `-p` / `-v`, plus a shared docker engine for the `docker` CLI, compose and `build`.
- 🎮 **GPU in the container.** `--gpus virgl` (OpenGL 2.1 / GLES 3.0) and `--gpus venus` (Vulkan 1.3 on Metal). On an XFCE desktop, OpenGL runs through Zink on Venus and SuperTuxKart renders at about 115 FPS.
- 🖥️ **Desktops.** A native AppKit window and a low-latency [browser desktop](docs/browser-desktop.md) for QEMU, Apple Virtualization Linux and macOS containers.
- 🍎 **macOS containers.** Tart-compatible OCI images with the paravirtualized Metal GPU; a game-like Metal frame renders at 99% of the host's rate.
- 🐧 **Full Linux and Windows machines.** Ubuntu and Fedora install unattended from their own ISOs; Windows 11 ARM64 installs from yours, with experimental D3D11 on Metal.
- 🤖 **MCP for AI agents.** Screenshots, input, screen waits, OCR with clickable boxes, windows, apps and the accessibility tree; macOS guests need no Screen Recording or Accessibility grants.
- ⏪ **Checkpoints and forks.** APFS-clone checkpoints with memory, rollback with processes running, and forks with fresh identities.
- 🌐 **Web UI.** Containers, a terminal and the desktop in the browser.

## 🚀 Quick start

Needs an Apple Silicon Mac with macOS 14+ (Venus needs macOS 26+), the Swift command-line tools and `brew install rust go node qemu squashfs`.

```sh
cargo build -p veynodevm -p vm-control && ./scripts/build-native.sh
python3 guest/kernel/build.py                  # pinned kernel + initramfs with the agent as PID 1
python3 scripts/install-dev-runtime.py --dns 1.1.1.1
veynodevm daemon start                         # launchd user agent
```

```sh
veynodevm run --rm alpine echo hi              # one micro-VM per container
veynodevm run -d -p 8080:80 --name web nginx:alpine
veynodevm exec web nginx -v

veynodevm run -d --desktop --name desk veynodevm/alpine-desktop
veynodevm desktop desk                         # native window
veynodevm ui                                   # web UI in the browser

veynodevm mcp --lifecycle                      # stdio MCP server for agents
```

GPU containers need the QEMU runtime: see [building the GPU runtime](docs/guides/building.md#gpu-runtime). Every other command is in the [usage guide](docs/guides/usage.md).

## 📚 Documentation

| Guide | |
|---|---|
| [Usage](docs/guides/usage.md) | Containers, the docker engine, desktops, GPUs, macOS containers, checkpoints, MCP |
| [Linux machines](docs/guides/linux-machines.md) | Full distributions booted through UEFI from their ISOs |
| [Windows 11 ARM64](docs/guides/windows.md) | Unattended install from your own ISO |
| [Building and checking](docs/guides/building.md) | Checks, release directory, desktop helpers, GPU runtime |
| [Low-level control path](docs/guides/low-level-control.md) | Driving `vm-control` directly |

| Reference | |
|---|---|
| [Product spec](docs/product-spec.md) | What VeynodeVM is for, and its release gates |
| [Architecture](docs/architecture.md) | Daemon, workers, guest agent and where capabilities live |
| [Local API 1](docs/api.md) | The daemon's socket protocol |
| [Computer use](docs/computer-use.md) | MCP desktop tools |
| [Security](docs/security.md) | Trust boundaries |
| [Known issues](docs/known-issues.md) | What is verified, and the limits |
| [Decisions](docs/decisions/) | Architecture decision records |

Experiments, benchmarks and validation evidence from the earlier phase are kept in a separate, unpublished archive repository. Paths written as `vm-archive/...` in the docs, and the `evidence` values in capability reports, name records in that archive.

## 🧩 Architecture at a glance

The control daemon owns every record and serves local API 1 on a Unix socket; the CLI, web UI and MCP servers are its clients. Each VM runs in its own worker: `vm-qemu` for QEMU/HVF with virgl or Venus, `vm-apple` for Apple Virtualization. Inside Linux micro-VMs, the Go guest agent is PID 1.

| Component | Path | State |
|---|---|---|
| `veynodevm` CLI | `crates/cli` | implemented |
| Local API 1 | `crates/api`, [docs/api.md](docs/api.md) | implemented |
| Control daemon, services, MCP server | `crates/control` | implemented |
| MCP HTTPS endpoint | `crates/mcp` | implemented |
| OCI registry, blob store, layers | `crates/oci` | implemented |
| QEMU/HVF worker (virgl, Venus) | `crates/control/src/bin/vm-qemu.rs` | implemented (Venus experimental) |
| Apple Virtualization worker | `native/worker` | implemented (2D display, no GPU) |
| Guest kernel, initramfs and agent | `guest/kernel`, `guest/veynode-agent` | implemented |
| Official desktop and GPU images | `guest/images` | verified on VZ, virgl and Venus |
| Shared docker engine | `crates/control/src/services/engine.rs` | implemented |
| Full Linux and Windows machines | `crates/control/src/services/machines.rs` | implemented |
| macOS containers | `crates/oci/src/tart.rs`, `crates/control/src/services/macos.rs` | implemented |
| Web UI | `web/` | implemented |
| Native desktop console | `native/console` | implemented |
| Console broker, H.264/HEVC WebRTC | `crates/media-local`, `browser/` | localhost only |
| Device pairing and signed sessions | `crates/session-auth` | localhost only |
| GPU runtime (UTM QEMU, virglrenderer, SPICE) | `runtime/` | development only |

## 🛠️ Building from source

```sh
./scripts/check.sh              # native build, cargo fmt/build/test/clippy, go, python, browser and web tests
scripts/check-changed.py        # only the checks the files changed against main need
```

See [building and checking](docs/guides/building.md) for the release directory, the desktop helpers and the GPU runtime.

## 📄 License

VeynodeVM is licensed under the [Apache License 2.0](LICENSE). The code that runs inside the GPU runtime's QEMU or SPICE process (the loaders, `spice-scanout-generation.c` and the runtime patches) is licensed under [GPL-2.0-or-later](licenses/GPL-2.0.txt), and the DXMT patch under [LGPL-2.1-or-later](licenses/LGPL-2.1.txt), as [decision 0026](docs/decisions/0026-gpu-runtime-source-build.md) records. [NOTICE](NOTICE) lists those files and the third-party code in this repository. The GPU runtime itself is not part of this repository and keeps its own licenses.
