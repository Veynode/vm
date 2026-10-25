# Known issues

## Networking

- **DNS when host resolvers listen only on loopback.** Apple NAT hands guests
  the gateway (`192.168.65.1`) as resolver; its forwarder cannot reach host
  resolvers bound only to `127.x` (for example local DNS-over-HTTPS proxies),
  so lookups time out. Set explicit servers per container (`--dns 1.1.1.1`),
  for the engine (`veynodevm engine start --dns 1.1.1.1`), or as the default
  in runtime.json (`scripts/install-dev-runtime.py --dns 1.1.1.1`).
- **Path MTU on Apple NAT.** Inbound TCP segments larger than about 1300
  bytes are sometimes lost (TLS handshakes stall after connect; MTU 1280
  works, 1400 does not). First seen in the engine VM, later reproduced
  intermittently in plain containers, so it is not engine-specific; iptables,
  bridge and conntrack changes did not affect it. Mitigations: the guest init
  enables packetization-layer PMTU discovery (`tcp_mtu_probing=1`), which
  recovers at 1500 after a few seconds of blackhole detection, and the engine
  defaults to MTU 1280 for its NIC and dockerd bridges (`--mtu` overrides).

## Shared engine

- Only directories given to `engine start --share DIR` (repeatable, fixed when
  the engine is created) exist inside the engine VM, at the same path. A
  `docker run -v` or compose bind mount outside them silently mounts an empty
  directory, because dockerd cannot see macOS paths. Changing the list means
  `veynodevm rm -f veynodevm-engine` and starting again (engine images and
  containers are discarded).
- The host docker CLI needs the compose/buildx plugins for `docker compose`
  and BuildKit; without them, run compose inside the engine
  (`veynodevm exec veynodevm-engine docker compose ...`).

## Containers

- `exec` is bounded (30 s, 64 KiB output per stream) and not interactive;
  `-i/-t` on `run` are accepted but stdin is not connected yet.
- Named volumes are not implemented; use host bind mounts (`-v /host:/path`).
  Image `VOLUME`s become anonymous directories on the container's disk.
- Apple Virtualization publishing (`-p`) is TCP only; UDP needs QEMU.
- Hard links whose target is hidden by a later layer are skipped during
  flattening (reported as a warning).

## macOS containers

- Each Tart image takes ~26 GB on disk (Tahoe vanilla: 22 GB download, 46 GB
  sparse disk); `pull` refuses to start, and stops, when free space would
  fall below 8 GiB. Containers are APFS clones and grow only with changes.
- Apple allows at most two macOS VMs running at once per host (counting VMs
  from other tools); a third `run`/`start` fails with that message and
  leaves no container behind.
- Containers keep the image's machine identity (`machine-id.bin`), as Tart
  does: a new identity makes the Guest start Setup Assistant ("Update Mac
  Automatically") in front of every window. Each container gets its own
  hostname (its name) at the first boot instead.
- **GPU** (Apple's paravirtualized Metal, measured with
  `scripts/macos-gpu-bench.swift` on an M3 Pro host, macOS 26.6.2 Guest):
  a game-like frame (4.7M triangles, 2048 draws, 4x MSAA HDR, post pass,
  1920x1080) renders offscreen at 99% of the host's rate (275 vs 278 fps);
  a draw-call-bound frame (8192 draws) reaches 65-80%. On screen the Guest
  display runs at 60 Hz (Virtualization has no refresh-rate setting): with
  vsync the same frame holds 58.5 fps, frame time p50 16.6 ms, p99 20 ms;
  without vsync it renders about 260 fps. The Guest reports no
  `presentedTime` for drawables, so measure pacing on the CPU side. The first
  minute after boot is slower while the Guest indexes.
- GUI apps and games start in the Guest's login session with
  `veynodevm exec CONTAINER open -a APP`; a bare executable that opens
  windows needs `sudo launchctl asuser $(id -u) PATH`, since an SSH session
  cannot reach the window server. Pointer input is Apple's virtual trackpad
  (absolute positions); games that need relative mouse motion are not
  verified.
- While MCP observes or controls the screen the guest shows "Your screen is
  being observed" in its menu bar, because input and screenshots go through
  Screen Sharing.
- The default login is the Tart one (`admin`/`admin`); `-u USER` and
  `VEYNODEVM_MACOS_PASSWORD` override it at `run`. The password is used once
  to install the SSH key and for Screen Sharing; it is stored in the
  container's private directory.
- There is no entrypoint. `run IMAGE COMMAND` runs COMMAND over SSH; in the
  foreground the container stops when it exits (`--rm` removes it), and
  `run -it IMAGE` opens a login shell. With `-d`, COMMAND runs in the
  background and again on every `start`; `logs` shows its combined
  stdout/stderr from `~/.veynodevm/main.log` in the guest (`-f` ends when it
  exits), and the VM keeps running afterwards. A container started without a
  COMMAND has no logs. `--entrypoint` and `--gpus` other than `all`/`metal`
  are refused.
- `--disk-size` (GiB, larger than the image's 50 GB) grows the clone on the
  host and moves the APFS Recovery container to the new end of the disk,
  because SIP forbids removing it inside the guest; the first boot then runs
  `diskutil apfs resizeContainer`, which adds about 20 s before the first
  command.
- `--dns`/`--mtu` (or the runtime.json defaults) are applied once, at the
  first boot, with `networksetup` on every network service.
- `image gc` removes abandoned partial pulls (`images/macos/.partial-*`) unless
  a pull is still running; until then an interrupted `pull` resumes from them.
- Volumes are mounted with `sudo mount_virtiofs` over SSH when the guest is
  ready, so the image's user needs passwordless sudo (Tart images have it).
- `stop` shuts the guest down over SSH, then falls back to the virtual power
  button and finally a forced stop.

## GPU and desktop

- **GPU status on QEMU/HVF** (UTM 5.0.5 runtime, Apple M3 Pro, macOS 27):
  - `--gpus virgl`: `/dev/dri/renderD128`, EGL/GLX renderer `virgl`, OpenGL 2.1
    and OpenGL ES 3.0 (Mesa 25.2.8 in `veynodevm/ubuntu-gpu`, 25.1.9 in
    `alpine-desktop`).
  - `--gpus venus`: `vulkaninfo` reports `Virtio-GPU Venus (Apple M3 Pro)`,
    driver `venus`, Vulkan 1.3; the offscreen raster/readback oracle
    (`scripts/linux-vulkan-triangle.c`) passes 3 frames with 0 pixel
    mismatches. Presenting works through X11: on Xorg (modesetting on
    virtio-gpu) Vulkan WSI over DRI3 runs vkmark at ~1400 FPS and `vkcube`
    renders correctly in an XFCE session (`veynodevm/ubuntu-desktop`);
    `MESA_VK_WSI_DEBUG=sw` (CPU copy, ~350 FPS) also works, including on
    Xvfb, which has no DRI3.
  - **Venus cannot import buffers allocated by another guest driver.**
    Applications that allocate with GBM and import into Vulkan (for example
    `vkmark --winsys kms`, or any direct-KMS client built that way) abort with
    `vn_ring_submit abort on fatal`; the host logs `vkr: failed to import
    resource: invalid res_id N`. Root cause: GBM on virtio-gpu is Mesa's virgl
    driver, so the buffer is a classic virgl 3D resource
    (`DRM_IOCTL_VIRTGPU_RESOURCE_CREATE` on the GBM fd, then `PRIME` handed to
    the Venus fd). With `render-server-mode=process`,
    `proxy_context_attach_resource` (virglrenderer `src/proxy/proxy_context.c`)
    forwards a resource to the Venus render server only as a dma-buf or shm
    fd; a GL-backed virgl resource must be exported with
    `virgl_resource_export_fd`, which has no dma-buf on macOS, so the resource
    is never attached and `vkr_get_fd_info_from_resource_info` finds no
    `res_id`. MoltenVK could not import a dma-buf/opaque fd either. Memory
    that Venus allocates itself crosses the boundary as MTLBuffer-backed shm
    (UTM's `vkr_metal_helpers`), which is why offscreen rendering and DRI3
    presentation work. Fixing the import needs a non-fd sharing path
    (IOSurface/MTLTexture) in virglrenderer and its render-server protocol.
  - Venus admission needs `runtime/register-venus.py`. Its loader, host
    virglrenderer, render server and patched SPICE library are rebuilt
    locally (`runtime/linux-vulkan-build-host-runtime.py`,
    `runtime/build-spice-reconnect-server.py`, Python 3.12 with meson 1.12.1
    and PyYAML); Apple clang output is not reproducible across toolchains, so
    the catalog hashes for those four roles were re-pinned on 2026-10-25 to an
    Apple clang 21 build. Vendor roles and the 27 framework pins still match
    UTM 5.0.5.
  - Shared engine (`engine start --gpus venus`): `docker run --device
    veynodevm.dev/gpu=all` and compose `devices: [veynodevm.dev/gpu=all]`
    report the Venus device, and the triangle oracle passes inside a docker
    container; containers without the device see no `/dev/dri`. All engine
    containers share the one GPU.
- `exec` right after `run -d` waits (up to 60 s) for the guest service. On
  QEMU the daemon now refuses `exec_start` until the worker reports its serial
  transport `ready`, before recording an execution scope; previously an exec
  sent during boot recorded a scope that could only be retired by terminating
  QEMU, which killed the container.
- QEMU containers stop with a hard stop: container-init does not handle the
  virt power button and the serial agent has no shutdown permission, so the
  entrypoint gets no SIGTERM. `veynodevm start` reconciles the daemon's exec
  journal for the stopped VM before booting it again.
- No native window for QEMU desktops unless the SPICE console helper
  (`native/console/build.sh`, CocoaSpice headers) is built and registered.
- `--desktop` without `--gpus` runs on Apple Virtualization with a fixed
  1280x800 2D scanout and llvmpipe rendering; no dynamic resolution.
- `veynodevm desktop` opens the Apple Virtualization window. The web UI does
  not embed the WebRTC desktop yet.
- Alpine 3.22 packages no Venus ICD; Vulkan GPU workloads need an image with
  Mesa's virtio driver (`veynodevm/ubuntu-gpu`).
