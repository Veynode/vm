# VeynodeVM

A Docker-like runtime for Apple Silicon Macs with a **GPU inside the container**, an optional **guest desktop**, and **MCP** for AI agents.
Docker on macOS gives containers no GPU. VeynodeVM runs each workload on QEMU/HVF with virtio-gpu, which provides OpenGL/GLES through virgl and Vulkan 1.2 through Venus → MoltenVK → Metal.

**Status: engineering preview, not a release.** The Docker-like layer works end to end on Apple Virtualization: OCI pull, micro-VM `run`/`exec`/`logs`/`stop`/`rm` with interactive `-it` sessions and `attach`, `-p`/`-v`, the shared docker engine (`docker` CLI, compose, `build`), the web UI, MCP container tools, the 2D guest desktop window and the low-latency [browser desktop](docs/browser-desktop.md) (QEMU, Apple Virtualization Linux and macOS containers). On QEMU/HVF with the locally fetched UTM runtime, `--gpus virgl` (OpenGL 2.1 / GLES 3.0) and `--gpus venus` (Vulkan 1.3, `Virtio-GPU Venus (Apple M3 Pro)`, pixel-checked offscreen rendering) are verified, including Vulkan on an XFCE desktop (`veynodevm/ubuntu-desktop`), where OpenGL runs through Zink on Venus (OpenGL 3.1 / GLES 3.1, pixel-checked) and SuperTuxKart renders with both its GLES (Zink) and Vulkan renderers at about 115 FPS, and offscreen Vulkan rendering runs at the host's rate (presenting to X11 costs about 0.6 ms a frame); direct-KMS clients that import GBM buffers into Venus do not work on macOS hosts. macOS containers run Tart-compatible OCI images (`ghcr.io/cirruslabs/macos-*`) on Apple Virtualization with the paravirtualized Metal GPU: `exec`/`cp` go over SSH, and MCP screenshots and input go through the guest's built-in Screen Sharing, so the guest needs no Screen Recording or Accessibility grants; on the first boot `run` itself grants SSH sessions Accessibility and System Events automation through that screen, so `exec` can script the desktop. Linux desktop containers on Apple Virtualization get MCP screenshots and input from the guest agent, and every desktop backend has `vm_screen_wait` (wait for the screen to change or settle) and `vm_screen_text` (host Vision OCR with clickable boxes); Linux desktop containers and macOS containers also report their windows, focus, keyboard layout and installed apps and can activate, move, resize, close windows and launch apps without screenshots, and expose their accessibility tree (AT-SPI on Linux, the Accessibility API on macOS: find controls by role and name, click or fill them by reference). A game-like Metal frame (4.7M triangles, 2048 draws, 4x MSAA) renders in the macOS guest at 99% of the host's rate and holds 60 fps on its 60 Hz display. See [known issues](docs/known-issues.md), the [product spec](docs/product-spec.md) and [architecture](docs/architecture.md).

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

veynodevm engine start --share ~/src            # shared docker-compatible VM (add --gpus venus for a GPU)
docker --context veynodevm compose up -d
veynodevm run --shared -p 8081:80 nginx:alpine
veynodevm build -t myimg .                     # build in the engine, import for micro-VMs

veynodevm run -d --desktop --name desk veynodevm/alpine-desktop
veynodevm run -d --desktop --gpus venus --name vkdesk veynodevm/ubuntu-desktop   # Vulkan desktop
veynodevm desktop desk                         # native AppKit window
veynodevm ui                                   # web UI; container page → Open desktop in browser
veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu   # vulkaninfo: Virtio-GPU Venus (Apple M…)
veynodevm update --gpus virgl --restart desk   # add, change or drop (none) a GPU later; keeps the disk
veynodevm update --cpus 4 -m 4g --disk-size 32 -p 8080:80 --restart desk   # hardware later, one restart; disks only grow

veynodevm pull ghcr.io/cirruslabs/macos-tahoe-vanilla   # macOS guest (~22 GB download, ~26 GB on disk)
veynodevm run -d --name mac --disk-size 100 -v $PWD:/Users/Shared/src ghcr.io/cirruslabs/macos-tahoe-vanilla
veynodevm exec mac sw_vers; veynodevm cp ./tool mac:/Users/Shared/tool; veynodevm desktop mac
veynodevm run -it --rm ghcr.io/cirruslabs/macos-tahoe-vanilla   # throwaway macOS login shell

# Full system: systemd as PID 1, GDM logs in to GNOME (Ubuntu 24.04)
veynodevm build -t veynodevm/ubuntu-gnome guest/images/ubuntu-gnome
veynodevm run -d --init systemd --desktop --name gnome veynodevm/ubuntu-gnome   # add --gpus virgl for QEMU/virgl
veynodevm run -d --desktop --desktop-size 1920x1080 --gpus virgl --name big veynodevm/ubuntu-gnome   # QEMU desktop size, default 1280x800
veynodevm update --desktop-size 1280x800 --restart big   # resize later; --desktop on|off adds or removes the display
veynodevm desktop gnome; veynodevm exec gnome systemctl status gdm
veynodevm stop gnome                           # systemctl poweroff inside the guest

# Checkpoints and forks (Linux containers): APFS clones of the writable layer,
# plus memory on Apple Virtualization (--disk-only to skip it)
veynodevm checkpoint create web before-upgrade # ~0.25 s for a 512 MiB guest, no disk data copied
veynodevm rollback web before-upgrade          # restore it with processes running (~0.4 s; --cold boots it)
veynodevm fork -n 3 --checkpoint before-upgrade web   # 3 containers, fresh id/hostname/MAC/ports
veynodevm fork rm web                          # remove every fork of web

veynodevm ui                                   # TanStack Start web UI on 127.0.0.1, opens the browser
veynodevm mcp --lifecycle                      # stdio MCP server
veynodevm mcp --input --tools core,screen,input  # list only some tool groups
```

State lives in `~/Library/Application Support/VeynodeVM` (override with `VEYNODEVM_HOME`).

## Full Linux distributions (UEFI)

Besides micro-VMs, a *machine* boots a distribution's own kernel and bootloader through UEFI from its unmodified ISO and keeps a persistent disk:

```sh
veynodevm machine create ubuntu --iso ~/Downloads/ubuntu-24.04.5-desktop-arm64.iso \
    --disk-size 40G --memory 6g --cpus 4 --gpus virgl   # QEMU/HVF; omit --gpus for Apple Virtualization
veynodevm machine start ubuntu      # boots the ISO (the disk once installed)
veynodevm machine stop ubuntu       # ACPI power button; -f kills
veynodevm machine eject ubuntu      # detach the installer ISO after installing
veynodevm machine ls; veynodevm machine inspect ubuntu; veynodevm machine rm ubuntu
veynodevm machine rename ubuntu ubuntu-dev  # stopped only; moves its directory and VM id
veynodevm machine update ubuntu --cpus 8 -m 8g --disk-size 64G  # stopped only; disks only grow
veynodevm machine icon ubuntu-dev globe:pink  # the UI's sticker, as `veynodevm icon` for containers
veynodevm desktop ubuntu            # native window (Apple Virtualization machines)
```

Files live in `<home>/machines/NAME`: a sparse raw `disk.raw`, the per-VM firmware variable store `efi-vars.fd` (boot entries; created on first start) and `serial.log`. QEMU uses edk2 in read-only pflash plus a 64 MiB variable flash, the ISO on virtio-scsi, USB keyboard/tablet and virtio-gpu-gl (virgl). Apple Virtualization uses `VZEFIBootLoader` with a `VZEFIVariableStore` and attaches the ISO as a read-only USB mass-storage device. The daemon accepts the same fields in `vm create` configs: `"boot":"uefi"`, `"efi_vars"`, `"cdrom"` and, for QEMU, `"qemu_monitor":true` (private QMP socket in the runtime state for automation).

Unattended installs (QEMU, scripted through the ISO's GRUB and the distribution's own installer):

```sh
scripts/install-distro.py ubuntu --name ubuntu --iso .local/iso/ubuntu-24.04.5-desktop-arm64.iso \
    --evidence .local/evidence/ubuntu-installed.png        # autoinstall + CIDATA seed
scripts/install-distro.py fedora --name fedora --iso .local/iso/Fedora-Everything-netinst-aarch64-44-1.7.iso \
    --live-iso .local/iso/Fedora-Workstation-Live-44-1.7.aarch64.iso \
    --evidence .local/evidence/fedora-installed.png        # kickstart (OEMDRV) + liveimg, offline
```

Verified: Ubuntu Desktop 24.04.5 arm64 and Fedora Workstation 44 aarch64 install unattended and boot from disk into GNOME with the virgl renderer; see [known issues](docs/known-issues.md#full-linux-distributions-uefi-machines) for what is verified and the limits.

## Windows 11 ARM64 (UEFI machine)

Windows runs as a QEMU/HVF machine installed from your own Windows 11 ARM64 ISO (Apple Virtualization has no Windows support). `--os windows` puts the ISO and seed images on USB CD-ROMs (Setup reads them before any driver loads), the disk on virtio-blk (`--disk-bus nvme` for the inbox NVMe driver instead), and adds virtio-net, virtio-serial, balloon, RNG, virtio keyboard/tablet and pvpanic; their drivers come from the virtio-win ISO. The display is UTM's `virtio-ramfb` (Basic Display, no GPU acceleration); `--gpus neptune` is an experimental Triton profile with hardware D3D11 through DXMT on Metal, pixel-checked and benchmarked, which needs UTM's guest tools 0.1.273 in the guest. With Neptune the guest defaults to 1280x1024; `veynodevm machine display NAME 1920x1080` sets the mode (the driver lists up to 2048x1152; it persists across boots) through that SSH login. Guest reboots restart QEMU, because edk2 hangs after an in-process reset. `install-distro.py windows` also sets up Windows' OpenSSH Server with a generated key, forwarded to `127.0.0.1:2222` (`machine create --publish HOST:GUEST`), and records that login (`veynodevm machine ssh-key`) so the MCP tools `vm_exec`, file transfer, `vm_desktop_state`, window actions, app launch and the UI Automation accessibility tree work over it, without installing anything in the guest. There is no TPM, no Secure Boot key enrolment and no guest agent, so Setup's Windows 11 checks are bypassed by the answer file.

```sh
scripts/install-distro.py windows --name win11 --iso ~/Downloads/Windows11_Client_arm64_en-us.iso \
    --drivers-iso virtio-win.iso --evidence .local/evidence/windows-installed.png
# or by hand: veynodevm machine create win11 --os windows --iso WIN.iso --attach answer.iso --monitor
```

The script writes an `Autounattend.xml` seed (Windows 11 Pro with the generic, non-activating key unless `--edition`/`--product-key` say otherwise; local administrator `veynode` with automatic logon), taps Enter for the CD's "Press any key" prompt, loads the virtio storage and network drivers in Setup and the rest offline, waits for the first logon to power off, ejects the media and boots the disk. See [known issues](docs/known-issues.md#windows-11-arm64-machines) for what is verified.

## Pieces

| Area | Path | State |
|---|---|---|
| `veynodevm` CLI (run, ps, logs, exec, build, engine, desktop, ui, mcp): a client of local API 1 | `crates/cli` | implemented |
| Local API 1 (the daemon's socket: envelope, codes, frames, client) | `crates/api`, [docs/api.md](docs/api.md) | implemented |
| OCI registry client, blob store, layer flattening, squashfs roots | `crates/oci` | implemented |
| Pinned kernel, initramfs, agent as container init | `guest/kernel`, `guest/veynode-agent` | implemented |
| Shared engine VM (dockerd, docker context, dynamic port publishing, `--share`) | `crates/control/src/services/engine.rs`, `crates/cli/src/engine.rs` (docker side) | implemented; Venus GPU via CDI verified |
| Web management UI | `web/` (TanStack Start; shadcn/ui on Base UI in Veynode Soft, vendored from core: [SOURCE.md](web/src/vendor/veynode-ui/SOURCE.md)) | implemented, including the WebCodecs browser desktop and an xterm.js terminal |
| Official desktop and GPU images | `guest/images` | alpine-desktop verified on VZ and virgl; ubuntu-gpu verified on virgl and Venus; ubuntu-desktop verified with Venus vkcube, Zink OpenGL and SuperTuxKart on XFCE |
| Control daemon (the core: every record, API 1 services, the MCP server), jobs, files | `crates/control` | implemented |
| MCP HTTPS endpoint (`vm-mcp-http`) and the MCP protocol tables | `crates/mcp` | implemented |
| QEMU/HVF worker with virgl / Venus GPU profiles | `crates/control/src/bin/vm-qemu.rs`, `qemu_graphics.rs` | implemented (Venus experimental) |
| Apple Virtualization worker | `native/worker` | implemented (2D display, no GPU) |
| Full-distribution machines (UEFI boot from ISO/disk, unattended installers) | `crates/control/src/services/machines.rs`, `crates/control/src/qemu_uefi.rs`, `native/worker`, `scripts/install-distro.py` | implemented; Ubuntu 24.04 and Fedora 44 verified on QEMU |
| APFS base images and writable clones | `crates/control/src/bin/vm-image.rs` | implemented |
| Console broker, hardware H.264/HEVC WebRTC | `crates/media-local`, `browser/` | localhost implemented, not yet in the web UI |
| Device pairing and signed sessions | `crates/session-auth` | localhost implemented |
| Native desktop console (AppKit/Metal) | `native/console`, `native/tools/spice-console.m` | implemented |
| GPU runtime fetch/build (UTM QEMU, virglrenderer, SPICE) | `runtime/` | development only |
| macOS containers (Tart OCI images, SSH exec/cp, Screen Sharing MCP) | `crates/oci/src/tart.rs`, `crates/control/src/services/macos.rs`, `native/worker/ScreenSharing.swift` | implemented; Metal at game load verified on Tahoe 26.6 (99% of host offscreen, 60 fps on screen); OpenGL is software-only; `native/guest-metal-fix` works around a paravirtual `baseVertex` bug that breaks MoltenVK games |
| Windows 11 ARM64 machines (QEMU/HVF, unattended install from the user's ISO) | `crates/control/src/services/machines.rs`, `crates/control/src/qemu_uefi.rs`, `scripts/install-distro.py` | implemented; see known issues for what is verified |
| Signed-in macOS Guest service | `native/guest-macos`, `native/guest-launcher` | reserved, not on main path |

Experiments, benchmarks and validation evidence from the earlier phase are in [`archive/`](archive/README.md).

## Build and check

Host: Apple Silicon, macOS 14+ (Venus requires macOS 26+). Install the Swift command-line tools, then `brew install rust go node qemu squashfs`.

```sh
./scripts/check.sh            # native build, cargo fmt/build/test/clippy, go, python, browser and web tests
./scripts/check.sh --real     # also builds the kernel and boots real containers (and the engine if docker is installed)
```

A release directory for the standalone product and for Veynode core's plugin (nothing is built, notarized or
published; the QEMU/GPU runtime is left out):

```sh
cargo build --release -p veynodevm -p vm-control -p veynodevm-mcp -p vm-media-local
python3 scripts/package-release.py --output /tmp/VeynodeVM --web [--sign "Developer ID Application: …"]
/tmp/VeynodeVM/install.py && /tmp/VeynodeVM/bin/veynodevm daemon start
```

Day to day, `scripts/check-changed.py` runs only the checks that the files changed against `main` need (`--quick` while editing, `--dry-run` to see the plan); see [AGENTS.md](AGENTS.md#verification).

For the browser desktop on Apple Virtualization containers, build the host
encoder and register it (QEMU desktops use the console helper below):

```sh
./native/tools/build-vz-desktop-stream.sh   # → .local/bin/vz-desktop-stream
./native/tools/build-vm-vision.sh           # → .local/bin/vm-vision (vm_screen_text, scaled screenshots, observe)
./native/tools/build-vm-macos-desktop.sh    # → .local/bin/vm-macos-desktop (macOS desktop/accessibility tools)
python3 scripts/install-dev-runtime.py
```

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

## Low-level control path

```sh
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp  --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

The last command is a stdio MCP server. Observation is on by default; lifecycle, execute, input and files are separate opt-ins. `native/tools/build-vm-vision.sh` builds the host text recognizer behind `vm_screen_text`; rerun `scripts/install-dev-runtime.py` to register it. Read the [control usage and HTTPS security boundaries](crates/control/README.md) before exposing any listener. For QEMU/GPU registration flags, see the same document.

[Native backend](native/README.md) · [Security](docs/security.md) · [Linux graphics decision](docs/decisions/0002-linux-graphics.md) · [Decisions](docs/decisions/)
