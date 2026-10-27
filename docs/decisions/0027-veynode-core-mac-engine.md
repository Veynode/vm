# 0027 — VeynodeVM is Veynode core's only engine on a Mac; machines serve core, containers serve MCP

Date: 2026-10-27. Status: **accepted direction**; nothing in core or here is implemented yet. VeynodeVM is
infrastructure, not a product people install on its own. Two consumers use it: Veynode core (`veynode/core`), and
coding agents such as Codex and Claude Code through MCP.

## Decision

1. **On a macOS host, core's only virtual machine engine is VeynodeVM.** A `veynodevm` plugin replaces core's `tart`
   and `apple-container` plugins. The engines on other hosts stay as they are:

   | Host | Engines in core |
   |---|---|
   | macOS (Apple silicon) | VeynodeVM |
   | Linux | Docker |
   | Windows | WSL, Docker |

   The Docker plugin stops declaring a macOS host. Today `packages/plugins/docker/src/docker-vm.ts` in core lists
   `{ os: "darwin" }`, `{ os: "linux" }` and `{ os: "win32" }`; it keeps the last two.
2. **The two sides of VeynodeVM have separate consumers.**
   - **Machines** (`veynodevm machine …`): whole, persistent VMs. They are for core, which installs its own machine
     program in the guest (`guests.provision`) and drives it over OCP.
   - **Containers and Docker compatibility** (`run`, `exec`, `engine`, compose, `build`, the `container_*` MCP tools):
     disposable environments for coding agents over MCP. Core does not use them.
3. **Core's plugin drives VeynodeVM through `veynodevm --json`.** It works the way the Docker and Tart plugins drive
   their CLIs, through core's `@veynode/container-engine` runner. The program is installed by core's installer as a
   signed archive (`install.kind: "unpack"`, with our Team ID), and the GPU runtime comes separately
   ([0026](0026-gpu-runtime-source-build.md)). VeynodeVM's name appears only in that plugin, as core requires
   (`pnpm check:vm-neutrality`).
4. **Inside a guest, core's machine program owns the agent tools.** That means shell, files, desktop frames, input,
   holds and leases, so a Bot sees the same tools on a VM, a physical Mac and a Windows PC. VeynodeVM provides the
   machine, its GPU and its display. Its own host-side screenshot, input and accessibility tools stay on the MCP
   side. The one exception to decide later is macOS guests: there, VeynodeVM's Screen Sharing path needs no TCC
   grants in the guest, which core's macOS program would need. Whether core uses that path, through its optional
   desktop capability, is a separate design.

## Engines the plugin declares

One plugin may declare up to eight engines. Using separate engines avoids a protocol change: core's engine
capabilities have no GPU field.

| Engine kind | Guest | Backend | `form` | `desktop` | Notes |
|---|---|---|---|---|---|
| `veynodevm` | linux | Apple Virtualization | `virtual-machine` | true | default Linux computer, no GPU acceleration |
| `veynodevm-gpu` | linux | QEMU/HVF | `virtual-machine` | true | `--gpus virgl`; Venus on macOS 26 and later |
| `veynodevm-macos` | macos | Apple Virtualization | `virtual-machine` | true | paravirtualized Metal GPU |

For all three: `diskSize: true`, `restart: true`, and `reconfigure: { cpu: true, memory: true, disk: "grow" }`. The
snapshot capabilities are all false (see gap 3). Windows machines are not offered in 1.0. They need the user's own
ISO, have no TPM, and their only GPU profile (`--gpus neptune`) depends on DXMT and D3DMetal, which 0026 excludes.

## Protocol mapping

Core's VM protocol is `packages/shared/src/runtime/vm-protocol.ts` (protocol version 1).

| Method | VeynodeVM | State |
|---|---|---|
| `engine.status` | daemon status, plus whether the GPU runtime is installed | to add a `--json` status |
| `engine.prepare` | `veynodevm runtime install` (the GPU engine only) | to build (0026) |
| `engine.start` / `engine.stop` | `veynodevm daemon start` / `stop` | exists |
| `engine.remove` | — | not offered |
| `systems.list` | the systems each engine runs | static in the plugin |
| `images.list` / `images.delete` | machine base images | **gap 1** |
| `images.ensure` (`reference`, `archive`) | pull a machine base image | **gap 1**; the `build` source is refused as unsupported |
| `images.export` | — | not offered |
| `guests.create` | `machine create NAME --from IMAGE --cpus -m --disk-size` | `--from` is **gap 1**; today a machine is created from an ISO |
| `guests.create` `mounts` | shared host directories | to verify for machines; containers have `-v` |
| `guests.create` `ports` | `machine create -p` | QEMU only today; Apple Virtualization machines need forwarding, or the guest's NAT address returned from `guests.endpoints` |
| `guests.list` / `guests.inspect` | `machine ls` / `machine inspect` | exists |
| `guests.start` / `guests.stop` | `machine start` / `machine stop` | exists |
| `guests.delete` | `machine rm` | exists |
| `guests.exec` (with `stdin`, `stdinFile`, `stream`) | `machine exec` | **gap 2**; Windows has it over SSH, Linux and macOS machines do not |
| `guests.provision` | `machine cp` and `exec`, or core's `provisionThroughExec` over stdin | follows gap 2 |
| `guests.endpoints` | published ports, and the address of the host's loopback as the guest sees it | to add a `--json` answer |
| `guests.attach` | the `machine exec -i` command line | follows gap 2 |
| `guests.reconfigure` | `machine update --cpus -m --disk-size` (stopped, disk only grows) | exists |
| `guests.clone` | `machine clone SOURCE NAME` | **gap 3** |
| `guests.export` / `guests.import` | — | not offered in 1.0 |

## Gaps to close

These four are what stands between today's machines and a computer that can be created from core in one step.
Gaps 1 and 2 block that; gaps 3 and 4 complete it.

1. **Base images.** Machines are installed from an ISO with an unattended installer
   ([scripts/install-distro.py](../../scripts/install-distro.py)), which takes tens of minutes. Core creates a guest
   from an image.
   - Publish pre-installed Linux desktop disks as OCI images, the way Tart's macOS images are published: start with
     Ubuntu Desktop, with Fedora optional.
   - Add `machine create NAME --from IMAGE`, which pulls once per digest and gives each machine an APFS clone of the
     disk, with a fresh machine identity, MAC address and EFI variable store.
   - Keep ISO installs as an advanced path.
2. **Command execution in Linux and macOS machines.** Core installs and starts its machine program with `guests.exec`
   and `guests.provision`, so a machine needs a command channel that handles stdin of any size and streamed output.
   - Linux: ship the guest agent in the base image as a system service. Containers already have one; it runs as PID 1
     there and would run as a service here.
   - macOS: SSH with a per-home key, as macOS containers do today.
   - Without `-i`, today's exec is bounded to 30 s and 64 KiB per stream ([known issues](../known-issues.md#containers)).
     Provisioning must use the streaming path.
3. **Clones.** Checkpoints and forks exist only for Linux containers, where they clone the writable layer. A machine's
   disk is a raw file on APFS, so `machine clone` of a stopped machine is an APFS clone plus a new identity.
   Snapshots are declared false until core's protocol has methods for them.
4. **macOS as a machine.** Today a macOS guest is `veynodevm run <Tart image>` with container semantics: `rm`
   discards it. It becomes `machine create mac --from ghcr.io/cirruslabs/macos-…` with a persistent disk. The SSH
   key, Screen Sharing input and first-boot grants move over from the container path unchanged. Apple's licence
   allows up to two additional macOS VMs on each Mac; the plugin should say so where it offers the engine.

## Container side (for MCP)

The 1.0 surface for coding agents is the following:
- `run`, `exec -i`, `-p` and `-v`.
- The shared engine: `docker`, compose and `build`.
- The MCP container tools, with least-privilege scopes by default.

Container desktops, browser WebRTC desktops and container GPUs are kept, but they are not 1.0 requirements for this
side. The Codex and Claude Code integration starts `veynodevm mcp` through a thin npm launcher. If the program is
missing, the launcher installs the same signed archive core installs. Both consumers share one state directory and
one daemon, so the daemon and CLI need a version handshake and versioned state migrations before either one ships.

## Retiring Tart and Apple container in core

Core is before its first stable release, so the engines can be replaced. The order below keeps a Mac from being left
without any engine:

1. Release VeynodeVM as a signed and notarized archive, and add `packages/plugins/veynodevm` to core with the three
   engines above. That needs gaps 1 and 2 closed. Add conformance tests against `@veynode/plugin-sdk`'s VM
   conformance suite.
2. In the same core change, drop `darwin` from the Docker plugin's hosts.
3. Set `listed: false` on `tart`, and `apple-container` already has it. Installed copies keep working from the user's
   data directory, and no new computers are offered on them.
4. Migration: existing Tart macOS images run on `veynodevm-macos` (gap 4). Computers made with Apple container or Tart
   Linux images have no migration and are created again.
5. Remove both packages, and the parked macOS VM code under `packages/plugins/tart/parked/`, which VeynodeVM
   replaces. Lower the `vm-neutrality-allowlist.json` baseline (`--baseline`) for every name they took.

## Consequences

- **Core's Mac support now depends on VeynodeVM's releases.** Signing and notarization, a frozen `--json` contract
  for the commands above, and the version handshake are now blocking items for core, not only for VeynodeVM.
- **What VeynodeVM ships in 1.0 is split by consumer.** The web UI (`web/`), the gateway (`crates/gateway`,
  `deploy/gateway`) and browser pairing duplicate core's console, gateway and leases. They stay development tools
  and are not part of the 1.0 surface.
- **Base images become our release artifacts.** They are OS images that we build, sign by digest and update, and the
  GPL obligations for what is inside them apply, as for the kernel and initramfs (0026's open questions).

## Open questions

- Whether core uses VeynodeVM's Screen Sharing path for macOS guests, or provisions its own macOS machine program and
  asks for TCC grants.
- Port publishing on Apple Virtualization machines: forwarding by VeynodeVM, or returning the guest's NAT address.
- Which Linux distribution, or distributions, the base images use, and how they are updated.
