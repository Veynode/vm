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
- **Local Network privacy blocks a released daemon's SSH to Apple Virtualization guests.** `machine.exec`,
  `files.copy` and published ports on a Linux machine on Apple Virtualization connect to the guest's vmnet address
  (`192.168.64.x`). From a signed release's daemon started by launchd, those connections time out until macOS allows
  the daemon on the local network (Privacy & Security, Local Network), while the same SSH from a shell works. QEMU
  machines (loopback forwards) and the Apple Virtualization display (vsock) are not affected. Seen with the 0.1.0
  archive on macOS 27; a vsock transport for exec and copy would avoid it.

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

- `exec` without `-i`/`-t` (and MCP `vm_exec`) stays bounded (30 s, 64 KiB
  output per stream). `exec -i/-t`, `run -i/-t`, `attach` and the web UI
  terminal use a streaming session: the guest agent listens on TCP 52022 and
  requires the container's random token (`session.token`, never in the image).
  Apple Virtualization reaches it at the guest NAT address, QEMU through a
  hidden `127.0.0.1` forward. Containers created before this was added have no
  token and need to be re-created.
- Closing a session (or the browser tab) sends SIGHUP to its process group;
  jobs started with `&` that ignore it keep running in the container.
- Detach from `attach` with Ctrl-P Ctrl-Q; only the last 64 KiB of a `-t`
  container's output is replayed on attach.
- The web UI (`veynodevm ui`) has no sign-in. It binds only 127.0.0.1 and
  checks the Host header (its WebSockets also the Origin), but another site
  open in the browser, or any process on this Mac, can send its HTTP requests
  and so start, stop, remove or exec in containers.
- Named volumes are not implemented; use host bind mounts (`-v /host:/path`).
  Image `VOLUME`s become anonymous directories on the container's disk.
- Apple Virtualization publishing (`-p`) is TCP only; UDP needs QEMU.
- Hard links whose target is hidden by a later layer are skipped during
  flattening (reported as a warning).

## Overhead and power

Measured live on Alpine containers (M-series host under other load, 3
interleaved rounds per variant, medians; the full rows are in
`.local/evidence/perf-overhead-2026-10-29.jsonl`).

- Idle cost, as host CPU of the daemon and every process it starts: an Apple
  Virtualization container's worker 0.02% of a core, plus 0.33% in the
  system's `com.apple.Virtualization.VirtualMachine` process that runs its
  vCPUs; a QEMU container with `--gpus virgl` ~4.1% (was ~10.3%). Of the QEMU
  4.1%, 3.7% is QEMU itself: every Guest timer interrupt and IPI exits to
  QEMU (~100 per second idle, against VZ's in-kernel handling), and SPICE's
  GL display refresh runs every 30 ms in the prebuilt runtime. The Guest's
  own wakeups are mostly the serial execution heartbeat (500 ms, decision
  0015). Removing the rest needs runtime (QEMU/SPICE) changes.
- Later the same day the QEMU process of that idle container went from
  ~3.6% to ~2.1% and then ~1.1% of a core. The Guest agent no longer
  reopens the cua-driver port every 250 ms while no host is connected (it
  waits for the host's open on the port), and new containers boot with
  `kfence.sample_interval=0 nowatchdog sysctl.vm.compaction_proactiveness=0
  sysctl.vm.stat_interval=10`: traced in the Guest, KFENCE's sampling (a work
  item every 100 ms), the once-a-second vmstat fold, proactive compaction and
  the soft-lockup watchdog were the largest timed wakeups left. Containers
  created before keep their command line.
- Most of the rest was the runtime's interrupt controller. UTM 5.0.5's
  QEMU 10.0.12 emulates the GIC, so every Guest timer interrupt and IPI exits
  to QEMU, and a Guest that wakes every 10 ms costs ~174% of a host core
  (headless Alpine kernel, 3 rounds; the same in a container). UTM 5.0.8
  (the same QEMU with HVF's in-kernel GICv3, on by default) costs ~11–14%;
  upstream QEMU 11.1.0 with `-accel hvf,kernel-irqchip=on` costs ~5–14%. With
  the in-kernel GIC turned off, 5.0.8 and 11.1.0 cost the same as 5.0.5, so
  the gain is the in-kernel GIC, not the GIC version. On 5.0.8 the idle
  container's QEMU uses ~0.5% of a core instead of ~1.0%, and `exec true` takes
  ~5 ms instead of ~8 ms (p50). The idle Windows 11 desktop (2 rounds, 6 minutes
  to settle): QEMU ~5–8% instead of ~10–12%, `machine exec` unchanged
  (~250 ms). A Windows installed on 5.0.5 boots on 5.0.8 and the other way
  round. The runtime is pinned to 5.0.8 since 2026-10-29
  (`runtime/fetch-utm.sh`): the same QEMU 10.0.12 with UTM's in-kernel GICv3
  patches, virglrenderer `451505ad` (Neptune wire format 2), MoltenVK
  `0790efd5`, DXMT `66e27f37`, CocoaSpice `d8d29fc8`, and one more SPICE
  server patch (a sound fix). Rebuilt on it and checked live the same day: a
  `--gpus virgl` container (`+virgl +context_init`, 2 capsets, free page
  reporting returning memory as on 5.0.5), a `--gpus venus` container
  (`vulkaninfo`: "Virtio-GPU Venus (Apple M3 Pro)", Mesa 26.1.6 venus), the
  Windows 11 machine on Basic Display, and Neptune D3D11 with guest tools
  0.1.274 (pixel test passed; see the Windows GPU entry).
- Memory the Guest frees goes back to macOS on QEMU. Linux containers get a
  virtio balloon with free page reporting (never inflated), and the agent
  loads `virtio_balloon` for it. QEMU discards a reported range with
  `madvise(MADV_DONTNEED)`, which macOS ignores for anonymous memory, so the
  QEMU loader (`native/tools/qemu-ram-reclaim.h`) replaces such a range inside
  HVF-mapped Guest RAM with fresh zero-fill pages (unmap it from the VM, map
  anonymous memory over it, map it back). In a 2 GiB container with `--gpus
  virgl`, writing 1.2 GB to tmpfs and deleting 1 GB of it left the QEMU process
  at 1,022 MB instead of 1,817 MB, and after five 900 MB write/hash/delete rounds
  it held 1,181 MB instead of 2,473 MB; the data that stayed hashed the same and
  the Guest logged no errors. Windows' virtio-win balloon negotiates only the
  stats queue, so Windows returns nothing by itself; instead the QEMU worker
  drives its balloon (`crates/control/src/qemu_balloon.rs`) from the statistics
  virtio-win's `BalloonService` (`blnsvr.exe`) sends every second. It inflates
  1 GiB at a time while the Guest's available memory is above a reserve (an
  eighth of RAM, 512 MiB–2 GiB) and its pagefile has been quiet for 30 s, never
  below a quarter of RAM (at least 2 GiB), and gives everything back at once
  when the Guest writes to its pagefile (other than the 30–110 MB it writes to
  make room right after a step), runs low, or stops sending statistics for
  15 s. New installs register `BalloonService` at first logon; an existing
  machine needs `blnsvr.exe -i` from the virtio-win ISO, and without it the
  balloon is never touched. An idle 8 GiB Windows 11 settled at 3 GiB within a
  minute and the host freed ~4.3 GB. The cost is a burst larger than the
  reserve: Windows trims to its pagefile faster than the balloon deflates
  (~1.5 GB/s), so writing 4 GiB in one go from the settled state took 4.6 s and
  wrote ~5 GB to the pagefile (the data was intact). Reading statistics costs
  `blnsvr` ~47 ms of CPU a minute.
  Apple Virtualization has no free page reporting, and its balloon gives the
  host nothing back: in a 4 GiB Alpine container (`virtio_balloon` loaded by
  hand), lowering `targetVirtualMachineMemorySize` by 2 GiB took the memory
  from the Guest, but the `com.apple.Virtualization.VirtualMachine` process
  stayed at 2.6–2.7 GB (2026-10-29). Guest RAM lives in that process, so the
  balloon stays unloaded (see the blacklist below). macOS 27's
  `VZCustomVirtioDevice` opens a way, tried only in a throwaway worker (not in
  the tree): a virtio balloon of our own (device 5,
  `VIRTIO_BALLOON_F_MUST_TELL_HOST`) maps Guest RAM into the worker with
  `guestMemoryMapping(atPhysicalAddress:length:)` and, once all four 4 KiB
  Guest pages of a 16 KiB host page are inflated, marks it
  `MADV_FREE_REUSABLE` (`MADV_FREE_REUSE` before deflating it).
  `MADV_DONTNEED`, `MADV_FREE` and `MADV_ZERO` free nothing in that shared
  mapping. A 2 GiB inflation gave back 2047 MiB as whole host pages, and six
  inflate/deflate rounds each followed by 2.5 GB of tmpfs churn kept hashed
  data intact with no Guest errors. But macOS takes reusable pages back only
  when it needs memory: with 3 GiB inflated the Virtualization process held
  3,359 MB for 30 s and 2,762 MB at 45 s. Free page reporting can't be built
  on this API, which hands a device copies of the Guest's writable buffers,
  not their addresses. Shipping it would need macOS 27, a host policy like
  the Windows one, and a run on the distribution images whose memory the
  blacklisted balloon and RNG corrupted.
- An idle QEMU container's own memory, beyond its Guest's: ~150 MB of QEMU
  heap holds the boot images it keeps for a reset (the 64 MiB edk2 image, the
  kernel and the initramfs), and the firmware is copied once more into its
  ROM.
- The Guest agent turns off fbcon's blinking cursor when virtio-gpu is
  loaded: it redrew and flushed the scanout every 200 ms (~3% of a host core
  on QEMU). The console broker and the serial transport block on events
  instead of stepping.
- Start to first `exec`: Apple Virtualization ~560 ms (was 1.2 s; 6.5 s
  before the host-seeded CRNG), QEMU ~1.1 s (was 1.7 s; 8.3 s). `exec true`
  24 ms on both (QEMU was ~140 ms). A refused first `exec` retries after
  5 ms, doubling to 20 ms, instead of 500 ms. Of the Apple Virtualization
  start, ~100 ms is Virtualization's own XPC launch (the worker now overlaps
  its AppKit setup with it), ~130 ms the kernel to init, and ~120 ms the
  Guest's module loads and root assembly. The container initramfs is
  uncompressed cpio (unpacked in ~40 ms instead of ~120 ms) and keeps only
  the modules needed before the modloop, so an idle container uses ~88 MiB
  of Guest memory instead of ~109 MiB. DHCP from Apple's NAT is ~30 ms,
  except ~250 ms for the first container after the daemon starts.
- Apple Virtualization writable disks are cached and sync with `fsync`, not
  `F_FULLFSYNC` (the default full sync also corrupted ext4 under sustained
  writes): a Guest flush takes 0.1–0.3 ms instead of 6.7 ms, fsync-heavy
  4 KiB writes 154 → ~5,000–9,000 IOPS, small-file creation 163 →
  ~2,000–3,500/s. A host power loss can lose writes the Guest flushed in the
  last moments (the drive cache), which can leave the Guest's ext4 needing its
  journal replay or fsck; a host crash or forced quit cannot.
- QEMU writable disks run on their own I/O thread with discard passed
  through: random 4 KiB reads ~35k → ~43k IOPS, others within noise.
- `stop` of an idle container: 0.36 s on both backends (QEMU was a 10 s
  timeout, then a hard stop).
- A synchronous `exec` polls its Guest job every 50 ms for its first half
  second, then every tenth of its run time (at most 250 ms): during a 10 s
  command the Guest takes ~114 timer interrupts a second on Apple
  Virtualization (was ~338) and ~277 on QEMU (was ~430). An idle Linux Guest
  takes ~34 a second on Apple Virtualization and ~90 on QEMU (GICv2 and the
  serial heartbeat); setting the agent's GOMAXPROCS to 1 changed neither.
- Windows 11 ARM64 on QEMU, idle at the desktop: the QEMU process ~12% of a
  core (unchanged; its vCPUs and the runtime's display refresh), the SPICE
  console helper 0.6% (was 3.6%, ~1,000 wakeups a second, now ~30). `machine
  exec` of `cmd /c exit 0`: ~1.3 s with Windows PowerShell JIT-compiling,
  ~0.4 s once .NET's ARM64 native images exist (the installer now builds
  them at first logon), ~0.3 s with the shared SSH connection and the exit
  seen at once. The High performance plan and Balanced measured the same
  within noise.
- Published ports (Apple Virtualization's publisher) block in `poll` between
  connections instead of a 100 ms accept step: a
  new connection's connect, one-byte echo and close went from ~108 ms to
  ~0.46 ms; an open connection's round trip is unchanged (~0.25 ms). QEMU
  publishes through its own `hostfwd` (~0.45 ms).

## Checkpoints, rollback and forks

- Memory is saved only on Apple Virtualization, for containers created since
  warm checkpoints (they need a persistent machine identifier and fixed MAC).
  QEMU checkpoints are disk-only because virgl and Venus block migration;
  rolling back to a disk-only checkpoint cold-boots and runs the entrypoint
  again (about 0.8 s on Apple Virtualization, 1.3 s on QEMU/virgl, alpine).
- Warm restore is for rollback only. VZ restores a state file only with the
  source's MAC and machine identifier, and the guest cannot change its MAC
  without losing its DHCP lease, so forks always cold-boot.
- The state file is about the size of the guest memory in use (34 MiB idle at
  512 MiB, 1.1 GiB with 1 GiB of tmpfs) and costs about 1.2 s per GiB to save
  or restore; past roughly half a GiB in use, warm rollback is slower than a
  cold boot. Saving pauses the VM for that time.
- After a warm rollback the wall clock is stepped to host time by the guest
  agent; the monotonic clock resumes from the checkpoint, and open TCP
  connections to the outside have usually timed out. Guests whose agent
  predates `clock_sync` keep the checkpoint's time.
- A checkpoint of a running container is crash-consistent (guest `sync`, then
  one atomic APFS clone), not application-consistent: a database that has
  not flushed its own buffers recovers as after a power cut.
- Host bind mounts (`-v`) are shared between a container and its forks, not
  copied; only the writable layer is forked.
- Linux micro-VM containers only: macOS containers, the shared engine and
  `machine`s have no checkpoints.
- A layer first booted by an agent older than the fork-aware one has no owner
  record, so its first fork keeps `/etc/machine-id`; later forks reset it.
  Images that ship `/etc/machine-id` in their read-only root keep that value
  in every container, as with Docker.

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
- macOS machines (`machine.create {from}`) reuse the container path, but
  have only unit and fixture evidence: no live create, first boot, exec,
  copy or clone has run. Their first boot finishes on the first
  `machine.exec` or `files.copy`, not at `machine.start`; `machine.update`
  grows the disk while stopped and APFS at that next command.
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
- **OpenGL in a macOS Guest is software-only.** The paravirtual GPU publishes
  only a Metal plugin, no OpenGL bundle, so CGL offers just "Apple Software
  Renderer" (OpenGL 4.1 APPLE-23.1.1) and refuses an accelerated pixel format.
  SuperTuxKart 1.5 (the official signed build) in its default OpenGL mode
  runs its `--benchmark` race at 0.9 fps at 1024x768. Its Vulkan renderer
  (`--render-driver=vulkan`, bundled MoltenVK) reaches the GPU ("Apple
  Paravirtual device", Vulkan 1.2) and holds the 60 Hz display at 1920x1080
  (typical 59 fps).
- **The paravirtual Metal device gets `baseVertex` wrong in direct indexed
  draws.** It reads the value as an unsigned 16-bit number, so only 0…65535
  arrive intact. Indirect draws are correct. Through MoltenVK this corrupts
  SuperTuxKart's 3D scenes (stretched triangles, white stripes); the same
  build renders correctly on the host. `native/guest-metal-fix` builds a
  library that turns every direct indexed draw into an indirect one, and a
  probe that shows the fault. With the library, an ad-hoc re-signed copy of
  SuperTuxKart renders like the host at the same frame rate. Rewriting only
  the out-of-range draws was not enough, and that second fault is not
  isolated. Hardened-runtime apps ignore `DYLD_INSERT_LIBRARIES`, so the
  library needs an ad-hoc re-signed copy of the app.
- Games that read a key's held state (steering, accelerating) need
  `vm_input_key` with `hold_ms`; the default 20 ms tap was missed by
  SuperTuxKart, a 900 ms hold drove the kart. Menus accept taps and clicks.
- GUI apps and games start in the Guest's login session with
  `veynodevm exec CONTAINER open -a APP`. A bare executable that opens
  windows needs `sudo -n launchctl asuser $(id -u) sudo -n -u "#$(id -u)" PATH`,
  since an SSH session cannot reach the window server. `launchctl asuser`
  alone runs it as root, and root then owns the files the app writes.
  Pointer input is Apple's virtual trackpad (absolute positions); games that
  need relative mouse motion are not verified.
- While MCP observes or controls the screen the guest shows "Your screen is
  being observed" in its menu bar, because input and screenshots go through
  Screen Sharing.
- `run` grants the guest's SSH sessions Accessibility and Automation of
  System Events at the first boot by clicking through System Settings over
  Screen Sharing (SIP blocks editing the TCC database). It reads English UI
  text and types the login password on a US layout; otherwise it warns and
  `exec` scripts that drive the UI get "not allowed assistive access" until
  the switch in Privacy & Security › Accessibility is turned on by hand.
  Automation covers System Events only: scripting another app directly asks
  for consent again on screen. The MCP desktop and accessibility tools need
  the Accessibility grant.
- MCP desktop tools in macOS containers: `vm_window_action unmaximize` is
  unsupported (macOS has no maximized state); window types come from AX
  subroles, which macOS reports as `dialog` while a window is minimized;
  background applications' menu bars are left out unless `include_hidden`.
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

## Full Linux distributions (UEFI machines)

- **Verified (2026-10-26, Apple M3 Pro, macOS 27, UTM 5.0.5 QEMU runtime):**
  - QEMU/HVF, `--gpus virgl`: official, checksum- and GPG-verified ISOs boot
    through edk2 (pflash code + per-VM 64 MiB variable store) into their own
    GRUB. `scripts/install-distro.py` installs unattended and boots the
    installed disk into an auto-logged-in GNOME session; a GL probe run
    inside the session (`gnome-session-check-accelerated-gl-helper
    --print-renderer` on Ubuntu's GNOME 46, `glxinfo -B` on Fedora's GNOME 50,
    which dropped that helper) reports renderer `virgl`.
    - Ubuntu Desktop 24.04.5 arm64: autoinstall (`autoinstall` typed on the
      GRUB command line, CIDATA seed, `ubuntu-desktop-minimal`), 8 to 13
      minutes (two runs); installed kernel 7.0.0-38-generic.
    - Fedora Workstation 44 aarch64: the Everything netinstall ISO boots
      Anaconda, which reads `ks.cfg` from an OEMDRV seed and installs the
      unmodified Workstation Live ISO's root filesystem (`liveimg`) offline,
      6 to 14 minutes (three runs); installed kernel 6.19.10-300.fc44.
  - Apple Virtualization: `VZEFIBootLoader` + `VZEFIVariableStore` boots the
    Ubuntu and Fedora disks installed under QEMU (APFS clones, fresh
    variable stores, so shim's removable-media fallback path is used) into
    an auto-logged-in GNOME session; serial (`hvc0`) shows the boot marker
    and renderer `llvmpipe` (no 3D on VZ), and both guests answer ping on
    the NAT network. The Ubuntu ISO also starts on VZ
    (attached as read-only USB mass storage), but VZ offers no screenshot or
    keyboard injection for Linux guests, so its GRUB/live session is not
    machine-verified and unattended installs are QEMU-only.
- **Screenshots under virgl.** QMP `screendump` returns a black frame for
  virtio-gpu-gl scanouts (it reads the unused 2D surface). The install script
  uses the SPICE GL scanout helper (`native/tools/build-spice-screenshot.sh`)
  and falls back to `screendump`; the helper only captures on a new frame, so
  the script moves the USB tablet while it waits and retries.
- **Installer automation is keystroke-based.** The script waits for the GRUB
  menu on the serial console, presses `c` and types `linux`/`initrd`/`boot`
  through QMP `send-key` (US layout). The ISO files are never modified; a new
  ISO whose kernel/initrd paths or volume label differ needs script updates.
- `machine eject` re-registers the VM without `cdrom`/seed images (daemon VM
  configs are immutable). Eject after installing: booting an installed disk
  with the ISO still attached was not tested.
- `machine rename` works on a stopped machine only: the name is the daemon
  VM id and the directory name, so it moves `<home>/machines/NAME` and
  re-registers the VM under the new id with its paths moved. Checked against
  a disposable daemon with a registered, never-booted machine; not yet on a
  machine with an installed system, snapshots or a Windows SSH login.
- `efi_vars` files are backend specific: a QEMU store is a 64 MiB pflash
  image, a VZ store is created by Virtualization.framework. Moving a disk
  between backends needs a fresh `efi_vars` (fallback boot path).
- Distributions with no agent: no `exec`, files or MCP guest services; the
  serial log (`<home>/machines/NAME/serial.log`) and the QMP monitor
  (`--monitor`) are the automation surface. QEMU UEFI machines use USB
  keyboard/tablet (edk2 has no virtio-input driver).
- Ubuntu's minimal desktop and Fedora's live root filesystem are installed
  offline; neither install applies updates. The installed systems use QEMU
  user networking (DNS through slirp) or Apple NAT like containers do.
- Fedora's GRUB prints `serial port 'auto'` errors on QEMU (Anaconda adds a
  serial terminal because of `console=ttyAMA0`); boot is unaffected.

## Windows 11 ARM64 machines

- **Verified once (2026-10-26, Apple M3 Pro, macOS 27, UTM 5.0.5 QEMU
  runtime, `Windows11_Client_arm64_en-us_26300_9457.iso`, an Insider build):**
  `scripts/install-distro.py windows` with the virtio-win ISO. edk2 boots the
  ISO from the USB CD-ROM, the Enter taps get past "Press any key to boot from
  CD or DVD", Setup reads `Autounattend.xml` from the VEYNODEWIN seed CD,
  partitions the NVMe disk, installs Windows 11 Pro without a TPM or Secure
  Boot (LabConfig bypass) and reboots into Windows Boot Manager from the disk.
  After `machine eject`, the installed disk boots to the automatically logged-in
  desktop of local administrator `veynode`. The virtio-win NetKVM driver is
  installed, and the "Red Hat VirtIO Ethernet Adapter" gets a QEMU
  user-network DHCP lease (10.0.2.15). Evidence is screenshots only:
  `.local/evidence/windows-installed.png`.
- **That run was not fully unattended.** About 25 minutes into Setup, Windows
  recorded bugcheck `0x6B` (PROCESS1_INITIALIZATION_FAILED, parameter
  `0xC0000185`, STATUS_IO_DEVICE_ERROR) in its UEFI variables and reset. From
  then on, edk2 hung on its logo before choosing a boot entry (one vCPU spinning
  in firmware), across `system_reset` and a fresh QEMU process. Moving
  `efi-vars.fd` aside (the worker recreates it from the template) let edk2 boot
  the disk through the removable-media path, and Setup then finished: the
  first logon powered the machine off. The cause, and whether it recurs, is
  unknown. No second install was run, because the host had 4.7 GiB free.
- **SSH into the guest:** `install-distro.py windows` now installs Windows'
  OpenSSH Server at first logon (a Windows Update download), authorizes a key
  it generates in the work dir and creates the machine with `--publish
  2222:22`. That sequence was verified by hand on `win11v` (`ssh -p 2222
  veynode@127.0.0.1` lands in PowerShell as the local administrator), not yet
  through a fresh unattended install. QMP `send-key` typing dropped characters
  and once left a key repeating at 40 ms holds 60 ms apart; it now uses 80 ms
  holds 180 ms apart.
- **Second install, virtio devices (verified once, 2026-10-26, same host and
  ISO):** machine `win11v` with the default virtio-blk disk and virtio-win
  0.1.302 (`.local/iso/virtio-win-0.1.302.iso`, SHA-256
  `303f7ae4…867949d`, computed locally: Fedora publishes no checksum for the
  ISO, only per-RPM ones). Setup loads viostor/vioscsi/NetKVM from the
  answer file's `PnpCustomizationsWinPE` DriverPaths, sees the virtio disk in
  WinPE and installs to it; the other drivers come from `offlineServicing`.
  On the installed desktop, `Get-PnpDevice -PresentOnly` reports Status OK
  for: Red Hat VirtIO SCSI controller and disk (viostor, the boot disk), Red Hat
  VirtIO Ethernet Adapter (NetKVM, DHCP lease 10.0.2.15), VirtIO Serial
  Driver, VirtIO Balloon Driver, VirtIO RNG Device, VirtIO Input Driver
  (keyboard and tablet), QEMU PVPanic Device. Evidence:
  `.local/evidence/windows-virtio-desktop.png` and
  `windows-virtio-devices.png`. Not exercised: vioscsi with a virtio-scsi
  disk, viosock, viofs (needs virtiofsd and WinFsp), viomem, and anything
  over virtio-serial: **qemu-guest-agent has no ARM64 Windows build** in
  virtio-win (x86/x64 MSIs only), so there is no guest agent. Display stays
  on Basic Display unless the Neptune profile below is used; viogpudo is
  never installed. `--disk-bus nvme` keeps the inbox NVMe driver as an
  alternative (the first install used it).
- **Windows `vm_screenshot` over 2 MiB (fixed in the native console).** A
  photographic Windows desktop (Neptune 1280x1024) encodes to 2.6 MB of PNG,
  which the native console refused at its old 2 MiB bound while the documented
  contract (decision 0024) was 8 MiB PNG / 12 MiB JSON. `VEYNODE_SCREENSHOT_MAX_BYTES`
  and `spice-screenshot` are now 8 MiB, and `VNRecordWriter` admits one JSON
  record up to 12 MiB at a time beside its 8 MiB queue (kinds other than JSON
  stay at 4 MiB). Live: 2.62 MB PNG, 3.5 MB reply, 0.44 s. Needs the rebuilt
  `VeynodeQEMUConsole.app`; an older staged app still returns
  `screenshot_exceeds_2MiB_limit`.
- **Warm guest reboots hang edk2; reboots are cold.** In both installs, the
  reboot from WinPE into the installed system (a guest reset inside the same
  QEMU process) left vCPU0 spinning in firmware at the same PC
  (`0x23f6b0924`, the other vCPUs at 0), while a new QEMU process booted the
  same disk and variables normally. UTM users report the same restart hang.
  For `guest: windows`, the worker passes `-action reboot=shutdown`; when
  QEMU exits after a SHUTDOWN event with reason `guest-reset`, `vm-qemu`
  emits state `starting` (reason `guest-reset`) and re-execs itself with the
  same arguments, so the PID and daemon pipe stay and a fresh QEMU starts.
  Verified once: `Restart-Computer` in the guest came back to the desktop
  with a new QEMU PID. `scripts/install-distro.py` also samples vCPU PCs over
  QMP and reports a hang when they don't move for 4 minutes.
- UTM's `edk2-arm-vars.fd` template is qcow2, not raw; it is now decoded
  (`qemu_uefi::template_flash`) into the 64 MiB variable store. The first
  install's store was a raw copy of the qcow2 bytes, which may explain that
  run's edk2 hang below.
- Devices: the ISO and seed images are USB CD-ROMs (`usb-bot` + `scsi-cd`),
  because Setup reads the answer file before any virtio driver is loaded.
  The display is UTM's `virtio-ramfb` (plain `ramfb` makes SPICE log
  "Setting device address of a display device: Not a PCI device"), Windows
  Basic Display at 800x600 as observed.
- **GPU (`--gpus neptune`, experimental): D3D11 is hardware-accelerated.** The
  `neptune-dxmt-experimental` profile matches UTM 5.0.5's and 5.0.8's default Triton
  arguments (`Configuration/UTMQemuConfiguration+Arguments.swift`):
  `virtio-ramfb-gl,hostmem=8G,blob=on,venus=on,neptune=on`, `-accel
  hvf,ipa-granule-size=4096`, SPICE `gl=es` with ANGLE on Metal,
  `VK_DRIVER_FILES` at UTM's MoltenVK ICD, and `NPT_BACKEND=dxmt` with the
  `NPT_*_LIBRARY_PATH` variables at `dxmt-native.framework`. The render
  server is the reviewed Venus registration's `virgl_render_server` (built
  from the virglrenderer commit the pinned UTM ships, with Neptune: `482f9d8b`
  for 5.0.5, `451505ad` for 5.0.8);
  UTM's own `QEMURenderServer` is App-Sandbox-signed and traps in
  `_libsecinit_appsandbox` outside UTM's QEMUHelper, so it is not used.
  Findings from `win11v` (M3 Pro, macOS 27, 4 vCPUs, 8 GiB), 2026-10-26:
  - **Driver version must match the host.** Guest tools 0.1.274 speak Neptune
    wire format 2, which the pinned UTM 5.0.8 runtime speaks; 0.1.273 speaks
    format 1, which UTM 5.0.5 spoke (`neptune: virtgpu: wire format mismatch
    host=0x00000001 guest=0x00000002`). The installer pins **0.1.274**
    (143,898,624 bytes, SHA-256 `ab6add47…b78f407a`). A machine installed
    with 0.1.273 needs viogpu3d from 0.1.274 (`pnputil /add-driver
    viogpu3d.inf /install`) before D3D works on 5.0.8; done that way on a
    clone of the test machine on 2026-10-29, the pixel test passed (host log:
    `renderer ready (wire=0x00000002 …)`). `d3d11-bench`, p50 of 3 rounds:
    64x64 `fill` waiting every frame 1.63–1.64 ms, 1080p `fill` waiting every
    frame 3.56–3.63 ms and every 4 frames 1.78–1.98 ms, `draws` 2000 waiting
    every 4 frames 1.41–1.76 ms and every frame 2.10–2.16 ms; close to the
    5.0.5 stock column below (not measured side by side).
    `scripts/neptune-umd-patch.py` (decision 0029) still accepts only the
    0.1.273 UMD builds.
  - **Licence.** The owner states Neptune is MIT, and the driver repositories
    (`virtio-win/kvm-guest-drivers-windows`, UTM's fork) carry a BSD 3-Clause
    `LICENSE` (checked 2026-10-27; not legal advice). VeynodeVM ships no
    guest tools: an unattended Neptune install has the daemon download
    0.1.274 from UTM's release (pinned size and SHA-256) and run `pnputil`
    at the first logon. Its `viogpu3d.cat` is signed through Microsoft's
    hardware program (Windows Hardware Compatibility Publisher), so it
    installs without a prompt or test signing.
  - viogpu3d is the only display adapter: dxdiag in the desktop session
    names "Red Hat VirtIO GPU 3D controller", chip "QEMU VIRTIO GPU",
    `neptune_umd.dll`, driver 100.6.101.58000, WDDM 2.2, DDI 11.1, feature
    levels 11_1 down to 9_1, 1024x768 at 60 Hz
    (`.local/evidence/windows-neptune-dxdiag.txt`).
  - **Pixel check passes** (`guest/windows/d3d11-pixel-test.c`, feature level
    11_1 on that adapter, WARP refused): a red triangle over a known clear
    colour, centre and corner pixels exact
    (`.local/evidence/windows-d3d11-triangle.png` and `.json`). D3D only works
    in the interactive desktop session (run it as a scheduled task with an
    Interactive principal); over plain SSH the user-mode driver can't open the
    GPU.
  - **Benchmark** (`guest/windows/d3d11-bench.c`, offscreen 1920x1080,
    10 warm-up frames, each measurement ended by reading back one pixel,
    because a D3D11 event query reports completion at once under DXMT; p50
    frame time, ranges over 3 runs):

    | Workload | Neptune/DXMT | WARP (CPU, same guest) |
    |---|---|---|
    | `fill` (full-screen, 64-step sin/cos per pixel), wait every frame | 298–320 FPS, 3.0 ms | 3.0–3.2 FPS, 304–332 ms |
    | `fill`, wait every 4th frame | 475–545 FPS, 1.88–1.97 ms | — |
    | `draws` (2000 draws, a constant-buffer map per draw), every frame | 213–455 FPS, 1.75–3.5 ms | 122 FPS |
    | `draws`, every 4th frame | 493–745 FPS, 1.21–1.66 ms | — |
    | `present` (flip swapchain, 1024x768 window), no vsync | 370–857 FPS, 0.96–1.53 ms | — |
    | `present`, vsync | 58.5–59.4 FPS, 16.2–16.4 ms | — |

    Every synchronous guest→host→guest round trip costs about 1.4 ms (64x64
    `fill`: 619 FPS, 1.40 ms; 3840x2160: 145 FPS, 6.5 ms), so a game that
    waits on the GPU each frame loses that much; pipelined frames hide it.
    No host-only run of the same D3D11 workload exists (macOS has no D3D
    without Wine/DXMT); WARP in the guest is the baseline. Run-to-run noise
    is ±15% (Windows background work).
  - **Tuning, one change at a time** against the 4-vCPU/8G/ramfb baseline
    (pipelined 1080p `fill`/`draws` p50): 8 vCPUs 1.90–1.99/1.10–2.24 ms,
    `hostmem=2G` 1.95–2.06/1.38–1.91 ms, `virtio-gpu-gl-pci` instead of
    `virtio-ramfb-gl` 1.99–2.04/1.56–1.82 ms: none is better beyond the noise,
    so the profile keeps UTM's values (ramfb also gives the firmware and
    Setup a display). Blob resources can't be turned off (Venus and Neptune
    need them). D3DMetal (`NPT_BACKEND=d3dmetal`) doesn't work with this
    render server: Apple's D3DMetal is x86_64-only and the render server
    would have to be a universal binary running that slice under Rosetta; the
    arm64 slice of `d3dmetal-native` exports no D3D entry points, so the guest
    falls back to Basic Render. DXMT has no steady-state performance option
    (its variables cover logging, frame capture, the shader cache, BC
    emulation and MetalFX upscaling); its log now goes to the worker's
    runtime directory (`DXMT_LOG_PATH`) instead of the current directory.
  - **Where the 1.4 ms goes** (2026-10-28; a fresh install of the same
    Windows build with tools 0.1.273, plus host-only microbenchmarks). The
    guest driver, not QEMU, sets the floor: in `neptune_umd_arm64.dll`
    (disassembly, matching Neptune's public `npt_ring.c`/`npt_common.h`) every
    synchronous reply wait (Map, QueryInterface, blob create) yields 256 times
    (`SwitchToThread`), then sleeps `160 << n` µs raised to at least 1 ms on a
    high-resolution waitable timer. Live: 256 yields take 0.25–0.26 ms; a
    1 ms high-resolution timer wakes after 1.50–1.58 ms p50 and a 0.2 ms one
    after 0.51–0.56 ms (guest wakeups fall on a ~0.5 ms grid, the same with
    the process at 0.5 ms or 1 ms timer resolution). The driver's own profile
    (`NPT_DEBUG=profile` in the app's environment, printed to stderr) shows one
    slow sync call per 64x64 `fill` frame, the staging Map (transport command
    0x30001), at 1466 µs, while the other sync calls answer in 5–17 µs; the
    render server's profile (`NPT_DEBUG=profile`) spends 547 µs per Map
    dispatch on average. A reply that misses the 0.26 ms yield budget costs a
    whole timer sleep, which is the 64x64 `fill`'s 1.38–1.52 ms (3 runs on this
    install). The host can't answer inside that budget through DXMT: on the host (same
    `dxmt-native`, its own `D3D11CreateDevice`), a clear, a one-pixel copy
    and `Map(READ)` take 250–310 µs p50 at 64x64 and 1080p alike (3 runs of
    1000), the same as Metal's own command-buffer completion
    (`waitUntilCompleted` 253 µs p50); spinning on an `MTLSharedEvent`
    signalled by the same command buffer sees it at 132 µs, but that wait is
    inside DXMT. Waiting on an `ID3D11Fence` (`Signal`, `Flush`, spin on
    `GetCompletedValue`) before the `Map` is no faster (503–581 vs 358–495 µs
    p50 on a loaded host), so the render server can't take that route either.
    QEMU's side is not it: UTM's QEMU 10.0.12 delivers render-server fences
    asynchronously ("async fence delivery enabled (render-server proxy)"),
    and Venus on the same QEMU/HVF/render server waits per frame at host
    parity (1922 vs 1876 FPS, [GPU and desktop](#gpu-and-desktop)). So no QEMU, vCPU or HVF option removes
    the floor; it needs a guest driver that waits in sub-millisecond steps or
    spins longer, or a DXMT whose `Map` waits on a shared event.
  - **A patched guest driver confirms it** (2026-10-28, live, experiment
    only; nothing ships). Two instructions per reply-wait site in tools
    0.1.273's `neptune_umd_arm64.dll` and `neptune_umd_ec.dll` (two sites
    each) were changed: the yield count (256) and the sleep floor (1000 µs).
    The Authenticode entry was dropped, and Windows 11 26300 loads the
    unsigned UMD (copied over the DriverStore file after `takeown`); the
    pixel test passed with every variant. Three interleaved rounds, guest
    rebooted after each swap, p50 ms:

    | Case | Stock | Floor 200 µs | 640 yields + floor 200 µs |
    |---|---|---|---|
    | 64x64 `fill`, wait every frame | 1.38–1.57 | 1.05–1.58 | 0.41–0.45 |
    | 1080p `fill`, wait every frame | 3.10–4.55 | 2.26–3.37 | 2.39–2.70 |
    | 1080p `fill`, wait every 4 frames | 1.96–2.01 | 1.66–1.69 | 1.66–1.67 |
    | `draws` 2000, wait every 4 frames | 2.04–2.37 | 1.45–2.46 | 1.36–1.46 |
    | `draws` 2000, wait every frame | 2.01–3.91 | 2.34–2.90 | 1.93–2.75 |

    The longer spin catches the host's ~0.5 ms reply without sleeping; the
    cost is guest CPU during the wait (the 64x64 loop's CPU time per wall
    time rose from 0.18–0.28 to 0.48–0.92 of one core). A 1024-yield spin
    alone (floor kept at 1 ms, before the reboot-per-swap method) reached
    0.41–0.51 ms at 64x64, but made the 1080p wait-every-frame case slower
    (3.04–4.38 ms), presumably because a ~2 ms wait then spins ~1 ms
    before its first 1.5 ms sleep. The fix belongs upstream in Neptune's
    `npt_ring.c` relax policy; a modified third-party driver isn't something
    VeynodeVM can ship. Once the guest hung (black screen, no SSH) running
    the stock UMD while processes started earlier still had a patched one
    mapped; twice more SSH timed out just after a reboot (patched UMDs) and
    the VM was force-restarted without a screenshot. The causes were not
    found.

    The same policy built from Neptune's source (osy/virtio-win-mesa
    `44d3259`, which matches 0.1.273; clang-cl with the xwin Windows SDK
    and WDK headers on the Mac; native ARM64 DLL only) gives the same
    result. It is 640 yields, then 8 sleeps of 200 µs, then the old
    backoff, tunable with `NPT_RELAX_YIELDS`, `NPT_RELAX_FAST_SLEEPS` and
    `NPT_RELAX_FAST_US`. In three rounds, with the VM force-restarted after
    each swap, the 64x64 case went from 1.35–1.38 ms to 0.43–0.49 ms and
    1080p `fill` waiting every frame from 3.32–4.31 ms to 2.26–3.05 ms. An
    unmodified source build matched stock (1.38–1.61 ms), and the pixel
    test passed 9 of 9.
  - **The render server's extra ~250 µs is the GPU waking up** (2026-10-28,
    host microbenchmarks; 3 interleaved rounds on a host at load 10–29).
    A Map dispatch is DXMT's `Map` plus one memcpy
    (`npt_resource_map`), so the gap between the live 547 µs and the
    host's ~280 µs isn't queueing. Idling between round trips, as the stock
    guest does while it sleeps ~1.5 ms, slows the same DXMT readback from
    260–314 µs p50 to 640–752 µs. Plain Metal behaves the same:
    `waitUntilCompleted` goes from 259–268 µs to 615–800 µs. Committing a
    one-pixel clear and `Flush` every 250 µs during the idle period (a
    render-server keep-alive through D3D11 alone) brings DXMT back to
    282–308 µs. That is still above the 0.26 ms yield budget, so a
    keep-alive alone doesn't spare the stock driver its 1 ms sleep, and it
    costs GPU power while the guest is idle. It isn't implemented.
  - **DXMT's staging `Map` waits for command-buffer completion, not the
    GPU.** In utmapp/dxmt `bcdcf6da` (UTM 5.0.5) a staging `Map` waits on
    `cpu_coherent`. The finish thread advances that only after
    `waitUntilCompleted` on the chunk's command buffer, so the wait is
    completion plus two thread wake-ups. Plain Metal sees a shared event
    signalled at the end of the same command buffer at 130 µs p50, whether
    it spins, calls `waitUntilSignaledValue:timeoutMS:` or uses
    `notifyListener`; `waitUntilCompleted` takes 215–271 µs.
    [`runtime/dxmt-gpu-completed-event.patch`](../runtime/dxmt-gpu-completed-event.patch)
    makes every chunk signal such an event as its last command and makes
    the staging `Map` wait on it, falling back to `cpu_coherent` after
    100 ms. Allocators still use `cpu_coherent`.
    [`runtime/build-dxmt-native.sh`](../runtime/build-dxmt-native.sh)
    builds it; an unmodified source build matches UTM's binary (251–253
    vs 250–254 µs). With the patch, the host readback drops from 256–343 µs
    to 141–148 µs at 64x64 and from 266 µs to 145 µs at 1080p, with the
    pixel identical in every run. After a 1.5 ms idle it is still 461–599
    µs, because the GPU wake-up remains, and with the keep-alive as well it
    landed at 168–300 µs. Live (2026-10-28, Windows 11 ARM64, 3
    interleaved rounds, host load 7–25, pixel test 12 of 12; a worker copy
    whose DXMT path points at the build, confirmed with `lsof`): with the
    patched guest UMD (variant C), 1000-frame rt64 p50 drops from
    0.40–0.49 ms to 0.16–0.31 ms and 300-frame 1080p fill from 2.37–3.30
    ms to 1.80–1.86 ms in every round, with less bench CPU per frame. The
    400-frame rt64 agreed in two rounds of three (0.25–0.26 vs 0.46–0.58
    ms). With the stock UMD the 1 ms sleep hides it (rt64 1.35 ms either
    way in the suite). Present doesn't move in any configuration. The
    Neptune profile still loads UTM's `dxmt-native`: shipping a patched
    DXMT needs a decision (it changes the reviewed runtime).
  - **Fewer guest spins trade round-trip latency for CPU** (2026-10-28,
    live, one round only, host loaded). With the patched DXMT, variant C
    (640 spins) rt64 0.33 ms at 0.69 bench cpu/wall; D (384) 0.48 ms at
    0.39; A (256) 0.84 ms at 0.37. Idle dwm CPU is ~0% with any variant
    and with the stock UMD. One round isn't enough to change the default
    from C.
  - **Present has no fixed sleep floor of its own** (static: Neptune
    `44d3259f888` `triton/tritonDxgi.c`, virglrenderer `482f9d8b`). A
    DWM-composited present (the bench's windowed flip swapchain) runs
    `Signal` and `Flush` asynchronously, then one synchronous
    `SetEventOnCompletion` arm, then waits on a kernel event that fires at
    GPU completion. The host routes that completion through DXMT's shared
    event listener, not a poll. Only the arm passes through
    `npt_ring_relax`, the same 256-yield / 1 ms path as `Map`, so it pays
    the floor only when the host takes longer than ~0.26 ms to answer.
    Scanout (full-screen) presents differ: they pace run-ahead by polling
    the fence in 1 ms high-resolution timer slices, which wake after
    ~1.5 ms. That would be the next thing to shorten if full-screen
    present times show it. `NPT_DEBUG=present_timing` in the app's
    environment splits a present into those four stages; it hasn't been
    run live yet. Live, the 1024x768 present p50 spans 1.15–2.45 ms with
    the stock UMD and UTM's DXMT alone, and neither the patched UMD nor the
    patched DXMT moves it consistently (3 rounds, 2026-10-28).
  - **Windows Fast Startup aborts QEMU.** Booting after a clean shutdown (a
    hybrid resume) hit `Assertion failed: (isv), function hvf_vcpu_exec, file
    hvf.c, line 2295` in 4 of 5 boots once viogpu3d was installed; boots
    after a crash (cold) were fine. With `powercfg /hibernate off` 3 of 3
    shutdown/start cycles booted, and `install-distro.py windows` now runs
    that at first logon. The same assert hit once more at the final power-off
    of a requested shutdown (2026-10-26, Neptune profile), after Windows had
    logged a clean shutdown (events 1074 and 6006, no 41/6008); the daemon
    reported `crashed`, the next boot was clean and the next shutdown stopped
    normally.
  - Uninstalling the active viogpu3d driver (`pnputil /delete-driver
    /uninstall`) hung the guest; restore a pre-driver clone instead. Plain
    `virtio-ramfb` is the same PCI device (1AF4:1050), so viogpu3d also binds
    to it when the Neptune profile is off.
  Frames on the console: the browser stream gets 60 fps bursts
  (16.5 ms median, see [browser-desktop](browser-desktop.md)). Not established: D3D12, real games, long runs, and the
  redistribution rights of the guest tools and DXMT. See
  [decision 0001](decisions/0001-windows-gpu.md).
- Not established: TPM (UTM's swtpm framework exists in the runtime but isn't
  wired), Secure Boot, activation (the generic Pro key doesn't activate),
  Windows Update, resolution changes, audio, the native console or web
  desktop for a Windows guest, MCP screenshots and input (the install script
  uses QMP `screendump` and `send-key`), clipboard and shared folders, and
  Windows 11 release builds (only 26300 was installed). Apple Virtualization
  cannot run Windows.
- **MCP over the guest's OpenSSH Server** (`vm_exec`, file tools, desktop and
  accessibility tools; see
  [Windows machines over SSH](../crates/control/README.md#windows-machines-over-ssh)):
  live on one machine through the daemon socket: exec with exit codes,
  `cmd`, quoted PowerShell, a missing program and a timeout; a 63 KB file
  round trip byte-exact, no overwrite, not-found and bad names; desktop
  state, Start menu apps, launching Notepad, its UIA tree, `set_text`,
  `do`, window minimize/maximize/activate/move. No scheduled task or request
  directory is left afterwards. Calls took 1.2 to 3.8 s, each with its own SSH
  connection; calls now share one connection per machine (OpenSSH
  multiplexing), which is unit-tested but not yet timed on a live guest.
  Non-ASCII text
  (`äöü é 你好 🙂`) passes both ways: exec argv and stdout, a missing
  program's error, a file's name and content, `set_text`, tree and find
  (before the scripts set UTF-8 console encoding, the SSH console's OEM code
  page broke any desktop reply holding such text). The first desktop
  call 30 s after boot timed out once; later ones worked. Not tried: other
  apps' UIA trees beyond Notepad, several desktop calls at once,
  a locked screen or a user without automatic logon.
- The CD loader's "Press any key" prompt appears only on the screen (with
  `virtio-ramfb`, edk2's serial copy shows only `BdsDxe` lines), so the script
  taps Enter for 8 seconds after edk2 starts the first boot entry. With 20
  seconds, a late Enter hit Setup's focused Cancel button and opened "Are you
  sure you want to quit?" (Enter again chose the focused No).

## Full system containers (`--init systemd`)

`run --init systemd` boots a whole distribution: container-init mounts the
root, volumes and network as usual, reports `ip` and `started`, then moves the
container root to `/` (as `switch_root` leaves an initramfs) and executes the
image's `/sbin/init` as PID 1 (a COMMAND replaces it, e.g.
`/lib/systemd/systemd systemd.unit=multi-user.target`). Before that it writes
runtime-only files under the new `/run`: the guest service as
`veynodevm-agent.service` (so `exec` keeps working), a unit that prints the
`exit` marker on poweroff, masks for `systemd-firstboot`,
`systemd-networkd-wait-online` and the console gettys, NetworkManager
`dns=none`, and a modprobe blacklist. An image without `/etc/machine-id` gets
a persistent one on first boot. Verified with `veynodevm/ubuntu-gnome`
(Ubuntu 24.04, systemd 255, GDM autologin into GNOME Shell on Xorg) on Apple
Virtualization (llvmpipe) and on QEMU with `--gpus virgl` (GLX renderer
`virgl`): `exec`, `stop`, `start` and a persistent journal across boots work.

- The init has to be systemd: the guest service only runs as a systemd unit.
  Another init (Alpine's busybox `/sbin/init`) boots, but `exec` and the
  graceful stop are unavailable. An image without the init fails with
  `Failed: init /sbin/init not found in the image`.

- `veynodevm stop` is graceful: on Apple Virtualization the guest service runs
  `/sbin/poweroff` (`systemctl poweroff`); on QEMU the CLI runs
  `systemctl --no-block poweroff` through `exec`, then falls back to the ACPI
  power button (systemd gets SIGRTMIN+4). Without `-t` the stop waits 30 s
  before a hard stop (10 s for ordinary containers). Shutdown logs
  `Failed unmounting /usr/lib/modules` (the read-only bind of the kernel
  modules); it is harmless.
- **Apple Virtualization: `virtio_balloon` and `virtio_rng` are blacklisted.**
  A distribution's udev autoloads drivers container-init never loads. With
  both of these loaded, `ubuntu-gnome` on Apple Virtualization corrupted guest
  memory within 25-100 s of a first boot in 3 of 3 runs (kernel oopses in
  unrelated code, squashfs decompression and ext4 directory errors; the image
  blocks read back intact). With both blacklisted, and in one run each with
  only one of them blacklisted, guests ran clean; the root cause is not
  isolated. The blacklist lives in `/run/modprobe.d/veynodevm.conf` and is
  written for every container, whatever its init: desktop entrypoints run
  `udevadm trigger` too, and an `ubuntu-desktop` run with both modules loaded
  lost Xorg to a guest kernel oops (`docs/computer-use.md`, phase 0). On
  QEMU, container-init loads `virtio_balloon` itself for free page reporting
  (a blacklist stops only udev's autoload); that was run on Alpine
  containers, not yet on a distribution desktop image.
- The image's network stack keeps the address container-init obtained: udev
  renames `eth0` (to `enp0s1` on both backends) and NetworkManager
  shows it as "connected (externally)". DNS comes from `--dns`/runtime.json
  through the bind-mounted `/etc/resolv.conf`, not systemd-resolved.
- The container's environment (`-e`, image `ENV`) applies to `exec`, not to
  systemd services; `-u/--user` and `--entrypoint` are rejected. There is no
  console login (the console is an output-only log); use `exec` or the
  desktop. Inside the guest the overlay's backing mounts (`/run/disk`,
  `/run/lower`) are not visible, so tools that inspect block devices see the
  raw virtio disks only.
- `systemctl reboot` restarts the micro-VM in place on Apple Virtualization
  (container-init runs again; the console log gains a second `started`).
- `ubuntu-gnome` runs GNOME on Xorg (`WaylandEnable=false`) so the session can
  be captured with `xwd` through `exec`; GNOME's Screenshot D-Bus API refuses
  callers other than the shell itself. The image user is `ubuntu`, password
  `ubuntu` (sudo, lock screen). The guest used about 0.7 GiB after login;
  `--init systemd --desktop` defaults to 4 GiB of memory (2 GiB without
  `--desktop`). In `xwd` captures the window shadows of client-side decorated
  windows (GTK 4 apps) are black; whether the native window shows the same
  was not checked (no host screen capture in the test environment).
  `exec` kills what it starts when it returns, so start desktop apps from the
  desktop itself.
- No interactive sessions: systemd replaces container-init before the session
  server starts, so `exec -i/-t`, `attach` and the web terminal refuse these
  containers, and `run --init systemd` rejects `-i/-t`. Use plain `exec`.

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
  - **OpenGL through Zink on Venus** (`ubuntu-desktop` selects it for the
    XFCE session when the Vulkan driver is `venus`; elsewhere set
    `MESA_LOADER_DRIVER_OVERRIDE=zink`): renderer `zink Vulkan 1.3(Virtio-GPU
    Venus (Apple M3 Pro) (MOLTENVK))`, OpenGL 3.1 core/compatibility, GLSL
    1.40, OpenGL ES 3.1 (Mesa 25.2.8). Live evidence, 2026-10-26:
    `scripts/linux-gl-triangle.c` (core context, instanced quads through a
    uniform block, FBO and window readback) passes with 0 mismatches at ~970
    FPS, and at 3.3 with `MESA_GL_VERSION_OVERRIDE=3.3
    MESA_GLSL_VERSION_OVERRIDE=330`; glxgears ~800–950 FPS (virgl ~1250);
    glmark2 2023.01 runs every default scene at 800x600 (score 331–405 on a
    heavily loaded host, load average ~22); SuperTuxKart 1.4 (Ubuntu's arm64
    build is GLES) renders its full pipeline (dynamic lights, karts, terrain;
    screenshot-checked) at 106–112 FPS average in `--profile-time` races,
    1024x768 window, 4 karts. FPS figures vary several-fold with host load.
    - The version stops at 3.1 because MoltenVK 1.4.2 lacks
      `VK_EXT_depth_clip_enable` (GL 3.2 `ARB_depth_clamp`); 3.3 also needs
      `ARB_vertex_type_2_10_10_10_rev`, which Zink withholds because Metal has
      no USCALED/SSCALED 2_10_10_10 vertex formats; 4.0 needs `shaderFloat64`,
      which Metal does not have. The override works for programs that use
      neither depth clamping nor scaled 2_10_10_10 vertices.
    - Plain Zink cannot draw on MoltenVK 1.4.2. With `robustBufferAccess`
      MoltenVK's bounds checks on Zink's arrayed uniform buffers do not
      compile; Venus hides `VK_EXT_provoking_vertex` (MoltenVK has it), so
      Zink wraps draws in generated geometry shaders that MoltenVK's emulation
      does not compile; debug names such as `sampler` (Gallium's blit
      shaders) reach the MSL as identifiers; and depth images with
      `HOST_TRANSFER` usage cannot be created, so every depth texture fails
      with `GL_OUT_OF_MEMORY` (Zink drops that usage only when built for
      macOS). The image's implicit Vulkan layer
      (`guest/images/zink/veynodevm_zink_layer.c`) hides `robustBufferAccess`,
      `geometryShader` and `VK_EXT_host_image_copy` from Zink and strips
      SPIR-V debug names; applications other than Zink see the device
      unchanged. Side effects: no geometry shaders in GL, and flat-shaded
      varyings take the first vertex instead of GL's last. An image built
      from `guest/images/ubuntu-desktop/Dockerfile` reports the Zink renderer
      in its `desktop:` log lines, and the oracle passes in it; `veynodevm
      exec` sessions need `-e MESA_LOADER_DRIVER_OVERRIDE=zink`.
    - Why Venus does not pass `VK_EXT_provoking_vertex` through is not
      established: the host render server allows it and Mesa's Venus driver
      supports it.
  - **A burst of host shader compiles kills a QEMU container that has guest
    exec.** QEMU's main thread runs virgl, whose ANGLE (Metal) shader compiles
    are synchronous: SuperTuxKart's first race (cold Metal cache), and any
    virgl GL game, makes it compile 130–160 ms shaders back to back for over a
    second. The guest stalls behind it, misses the 1 s serial-exec heartbeat
    budget ([decision 0015](decisions/0015-qemu-serial-execution-v2.md)),
    and the worker fail-stops QEMU (`serial execution incarnation fenced;
    QEMU will be terminated`). Seen with virgl and also with Venus/Zink on a
    fresh container (Xorg's glamor runs on virgl); warm runs survive, and
    eight guest CPU spinners for 25 s do not trigger it. Live evidence,
    2026-10-26: the worker's stdout event, and host logs of QEMU's compiles
    right before each fence. The fence is hard to see: the daemon starts the
    worker with stderr discarded, does not log the event, and `ps` can still
    show the container `Up`. What to change (the heartbeat budget, or virgl
    off the big QEMU lock) is open.
  - **A real Vulkan game:** SuperTuxKart 1.4 `--render-driver=vulkan` on
    Venus renders correctly (screenshot-checked) at 115 FPS average in a
    36 s `--profile-time` race, 4 karts, 1024x768 window, frame cap raised.
  - **Performance against the host** (live, 2026-10-26, M3 Pro, host load
    average ~6 from other sessions; run-to-run noise ~±5%).
    `scripts/vk-offscreen-bench.c` runs the same scene natively on MoltenVK
    and in the guest on Venus. Offscreen, the guest matches the host: 1080p
    16-layer fill 396 vs 397 FPS pipelined (100%), 335 vs 340 waiting each
    frame (99%), 385 vs 385 with a per-frame CPU readback of the frame
    (100%). One-triangle frames, sync per frame, run at 1922 vs 1876 FPS
    (102%), but three frames in flight reach 6525 vs 12046 (54%): submission
    overhead. Presenting to X11 is the cost. At 1280x720, X11 drops the light
    scene from 6338 to 1322 FPS (~0.6 ms per frame) and the heavy scene from
    880 to 480. Swapchain image count, present mode and Venus feedback
    options don't change it; FIFO isn't vsynced (1051 FPS);
    `MESA_VK_WSI_DEBUG=sw` is slower (231 / 273). During it the guest is ~84%
    idle and QEMU's main thread ~22% busy: a serial
    glamor → virtio-gpu → virgl → fence round trip, not a copy. SuperTuxKart
    holds 111–118 FPS on Vulkan and Zink alike, whether at 640x480 or
    1024x768, with 2 or 4 vCPUs (2 or 4 GiB), with a 256 MiB or 1 GiB
    `hostmem`, with any present mode or Venus ring/batching option, and with
    Mesa 25.2.8 or 26.0.8 (Ubuntu 26.04). It uses ~30% of a guest CPU, so the
    limit is per-frame latency in this path. No change won, so the defaults
    stay: 2 vCPUs, `hostmem=256M`, Zink on Venus, Ubuntu 24.04's Mesa. Mesa
    26.0.8 still needs the Zink layer (Zink aborts without it) and still stops
    at GL 3.1. Its X11 present is mixed: heavy +19%, light −43%. The host STK
    baseline was not measured, since that needs an app installed on the host.
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
    the catalog hashes for those four roles were re-pinned to an Apple clang
    21 build (2026-10-25 for 5.0.5, 2026-10-29 for 5.0.8). Vendor roles and
    the 27 framework pins match UTM 5.0.8. The geometry-shader answer in
    `qemu_graphics.rs` is bound to the MoltenVK binary; queried again on
    5.0.8's (`vkGetPhysicalDeviceFeatures`/`Properties` through UTM's Vulkan
    loader, 2026-10-29), it reports `geometryShader` true with all five
    geometry limits 0, as 5.0.5's did, and the pin moved to it.
  - Shared engine (`engine start --gpus venus`): `docker run --device
    veynodevm.dev/gpu=all` and compose `devices: [veynodevm.dev/gpu=all]`
    report the Venus device, and the triangle oracle passes inside a docker
    container; containers without the device see no `/dev/dri`. All engine
    containers share the one GPU.
- `exec` right after `run -d` waits (up to 60 s) for the guest service. On
  QEMU the daemon refuses `exec_start` until the worker reports its serial
  transport `ready`.
- QEMU containers stop through the ACPI power button: the serial agent has no
  shutdown permission, so container-init loads `tiny-power-button`, which
  turns the press into SIGTERM for PID 1; the entrypoint gets SIGTERM and 10 s
  before SIGKILL, as on Apple Virtualization. `stop` took 0.36 s for an idle
  container on both backends (QEMU waited out the 10 s timeout and hard-stopped
  before). This needs the Guest's ACPI, so QEMU containers keep booting
  through EDK2, which costs ~0.5 s of their start (the guest is ready ~260 ms
  after `cont` without it, ~800 ms with it). Dropping EDK2 needs another stop
  path, such as a power line on its own virtio port; the micro-VM kernel has
  no driver for the device-tree power button (`gpio-keys`).
- `veynodevm update --gpus none|virgl|venus` (and the web UI's GPU entry)
  moves an existing Linux container between Apple Virtualization and QEMU/HVF.
  It keeps the writable layer, image, ports, volumes, env and session token
  and rewrites the configuration disk and the daemon's VM configuration; the
  container must be stopped, or `--restart` stops and starts it. UDP ports
  block the move to Apple Virtualization; macOS containers and the shared
  engine can't switch. The memory follows the backend's default (512 MiB vs
  2 GiB) only when it still had the old default, so a QEMU container given
  `-m 2g` explicitly also drops to 512 MiB on Apple Virtualization. Warm checkpoints restore only
  on Apple Virtualization with the MAC they were taken with: after a switch
  to QEMU and back (new MAC) a rollback cold-boots their disk. Live-checked
  with `alpine:3.20` (Apple Virtualization → `--gpus virgl --restart` →
  `none`): `exec` works after each switch, a file in the writable layer and a
  `-v` mount survive both, and the virgl guest has `/dev/dri/renderD128` with
  `[drm] features: +virgl`; not yet checked with a desktop, `--init systemd`
  or warm checkpoints.
- `veynodevm update` (and the web UI's Hardware dialog) changes CPUs,
  memory, disk, GPU, desktop, published ports, DNS, MTU and MAC in one
  re-registration of the container's VM. The disk only grows: `rw.raw` is
  made longer (sparse) and container-init runs `resize2fs` at the next start,
  which needs boot assets rebuilt with `guest/kernel/build.py` (it adds
  `e2fsprogs-extra`); older ones keep the old file system size. Memory is
  checked against the VM budget at start, not when it is changed. macOS
  containers change only CPUs and memory; the shared engine none. Network is
  one NAT adapter only (no bridged mode or second adapter); Apple
  Virtualization containers publish TCP only. A new MAC or GPU means warm
  checkpoints cold-boot their disk. Checked with unit tests and the web
  fixture only; not yet live (the disk growth included).
- `machine update` and the Hardware dialog change a VM only while it is
  stopped: a grown disk still needs its partition and file system extended
  inside the guest (`growpart`, GNOME Disks, Windows Disk Management).
- A VM's web terminal is an SSH login, as `vm_exec` reaches Windows: it
  needs the key `machine ssh-key` stored and a forward to guest port 22
  (`--publish 2222:22`), and the guest's own SSH server. Installed
  distributions have no guest agent, so a VM without them has no terminal.
  Checked live on a Windows 11 clone (PowerShell input, resize, exit) through
  the production web server; `npm run dev` does not serve the terminal
  WebSocket, so use `web/server.mjs` (`veynodevm ui`) to try it.
- `--desktop` without `--gpus` runs on Apple Virtualization with a fixed
  1280x800 2D scanout and llvmpipe rendering; no dynamic resolution.
- `veynodevm desktop` opens the Apple Virtualization window, or for `--gpus`
  containers the SPICE console window (`runtime/build-qemu-console.sh`, then
  `scripts/install-dev-runtime.py --qemu`; without it QEMU desktops have no
  window). The QEMU window starts view-only; keyboard and pointer input begins
  when the window is focused.
- The browser desktop ([docs/browser-desktop.md](browser-desktop.md)) on
  Apple Virtualization serves one viewer per container at a time (remote
  desktop shares one encoder among its viewers; see
  [remote-desktop.md](remote-desktop.md)) and does not
  show the guest cursor (the browser draws its own). Desktop containers created
  before the Apple path existed need a restart to boot the agent that serves
  it. Firefox needs the H.264 option (no HEVC in WebCodecs). macOS
  containers stream from the Guest's Screen Sharing the same way, one viewer
  at a time; Option has no Screen Sharing keysym, and guest resolution changes
  are not verified. Sending Option through the VM's own keyboard did not work
  either (tried on a macOS 26 guest): from an unshown `VZVirtualMachineView`
  the worker posted Option as a `flagsChanged` event, then as a key-down of
  key code 58 with the option flag, and routed the letter after it through
  the same view. The guest received the letter (TextEdit and Terminal showed
  a plain `a`, not `å`) but never the modifier; the unshown window is not the
  key window, and making it one would put a window on the user's screen. The
  experiment is not in the tree.
- Alpine 3.22 packages no Venus ICD; Vulkan GPU workloads need an image with
  Mesa's virtio driver (`veynodevm/ubuntu-gpu`).

## Desktop sound and clipboard

How it works: [browser desktop](browser-desktop.md#sound-and-clipboard).

- Checked live on `alpine-desktop` (both backends) through the relay and in
  the Claude app's Chromium browser pane, and a Guest copy into the Mac's
  pasteboard in Brave; not in Chrome, Safari or Firefox, the
  native console window, the Ubuntu images, Windows or macOS containers.
  See [the evidence](browser-desktop.md#sound-and-clipboard).
- A browser that denies `clipboard-write` (the Claude app's browser pane does)
  leaves every Guest copy waiting behind the clipboard button's dot, and **Get
  from desktop** reports the refusal as a toast.
- `alpine-desktop` has no CJK or emoji fonts: such text crosses the clipboard
  exactly but shows as boxes in the Guest.
- The relay's microphone frames come from the page's audio clock; a sender that
  falls behind real time leaves short silences in the Guest's recording.
- Images built before this change have no PipeWire virtual devices (Apple
  Virtualization: `audio_unavailable`) and no `spice-vdagent` (QEMU: the
  clipboard reports that the Guest runs no clipboard agent). Rebuild the image
  to get them.
- Windows machines get a sound card (HD Audio) but no clipboard: no agent port
  is wired for them, and they have no `spice-vdagent`.
- macOS containers (browser desktop only; [design](browser-desktop.md#macos-containers)):
  - Checked live on macOS 26.6.2 through the relay (clipboard both ways with
    Chinese and emoji; a 440 Hz tone at 48 kHz; mute), not in a browser page;
    see [the evidence](browser-desktop.md#macos-containers).
  - Sound needs the opt-in Guest service started with `--allow-observe
    --allow-audio` and its Screen Recording permission, which nothing grants
    for you (the Tart images grant it only to Screen Sharing); `run` does not
    install that service. Without it the speaker stays silent
    (`audio_unavailable` in the encoder diagnostics).
  - TCC charges the service to whatever started it: launched over SSH it
    counts as `sshd-keygen-wrapper` and gets no audio even with its own grant.
    Start it as a launchd job in the user's GUI domain.
  - In some live runs a tone played with `afplay -t` went silent part-way
    through a session while records kept coming; not reproduced with plain
    `afplay`, cause unknown.
  - Only containers created after this change have the sound card. Recreate
    older ones to get it.
  - No microphone: the viewer's microphone button does nothing. Apple
    Virtualization's only input is the Mac's own microphone, fixed for the
    VM's life, which the viewer could not switch.
  - The clipboard runs `osascript` over SSH with `sudo -n launchctl asuser`:
    it needs the image's password-less `sudo`, and each viewer holds one SSH
    session. A Guest whose pasteboard privacy setting asks before a program
    reads the pasteboard would show that prompt; none appeared on Tahoe.
  - The Apple Virtualization window has no sound or clipboard (Linux desktops
    on Apple Virtualization have none there either).
- The remote WebRTC desktop (`crates/media-local`, `browser/`) carries
  neither; only the local viewers do.
- Text only, at most 200 KiB; images and files are not shared.
- ⌘-to-Ctrl mapping is in the browser only. In the native console, use the
  Guest's own Ctrl shortcuts.
- A browser that fires no `paste` event on the desktop canvas (Safari, until
  checked) pastes the Guest's own clipboard on ⌘V after 150 ms. The page's
  text then goes through the clipboard panel's box and **Send to desktop**.
- Firefox and Safari may refuse to write a Guest copy to the page's clipboard
  outside the ⌘C gesture. A copy made from the Guest's own menu then waits
  behind the clipboard button's dot for **Get from desktop**.
