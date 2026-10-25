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

## Containers

- `exec` is bounded (30 s, 64 KiB output per stream) and not interactive;
  `-i/-t` on `run` are accepted but stdin is not connected yet.
- Named volumes are not implemented; use host bind mounts (`-v /host:/path`).
  Image `VOLUME`s become anonymous directories on the container's disk.
- Apple Virtualization publishing (`-p`) is TCP only; UDP needs QEMU.
- Hard links whose target is hidden by a later layer are skipped during
  flattening (reported as a warning).

## GPU and desktop

- **GPU status on QEMU/HVF** (UTM 5.0.5 runtime, Apple M3 Pro, macOS 27):
  - `--gpus virgl`: `/dev/dri/renderD128`, EGL/GLX renderer `virgl`, OpenGL 2.1
    and OpenGL ES 3.0 (Mesa 25.2.8 in `veynodevm/ubuntu-gpu`, 25.1.9 in
    `alpine-desktop`).
  - `--gpus venus`: `vulkaninfo` reports `Virtio-GPU Venus (Apple M3 Pro)`,
    driver `venus`, Vulkan 1.3; the offscreen raster/readback oracle
    (`scripts/linux-vulkan-triangle.c`) passes 3 frames with 0 pixel
    mismatches. Presenting to the KMS scanout fails: the render server
    cannot import the guest-allocated resource (`vkr: failed to import
    resource: invalid res_id`), so `vkmark --winsys kms` aborts with
    `vn_ring_submit abort on fatal`. Vulkan WSI/display output on Venus is
    therefore not usable yet; compute and offscreen rendering are.
  - Venus admission needs `runtime/register-venus.py`. Its loader, host
    virglrenderer, render server and patched SPICE library are rebuilt
    locally (`runtime/linux-vulkan-build-host-runtime.py`,
    `runtime/build-spice-reconnect-server.py`, Python 3.12 with meson 1.12.1
    and PyYAML); Apple clang output is not reproducible across toolchains, so
    the catalog hashes for those four roles were re-pinned on 2026-10-25 to an
    Apple clang 21 build. Vendor roles and the 27 framework pins still match
    UTM 5.0.5.
  - The engine VM's GPU path (`engine start --gpus`, CDI device) is not yet
    verified.
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
