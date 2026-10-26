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
- Named volumes are not implemented; use host bind mounts (`-v /host:/path`).
  Image `VOLUME`s become anonymous directories on the container's disk.
- Apple Virtualization publishing (`-p`) is TCP only; UDP needs QEMU.
- Hard links whose target is hidden by a later layer are skipped during
  flattening (reported as a warning).

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
  `neptune-dxmt-experimental` profile matches UTM 5.0.5's default Triton
  arguments (`Configuration/UTMQemuConfiguration+Arguments.swift`):
  `virtio-ramfb-gl,hostmem=8G,blob=on,venus=on,neptune=on`, `-accel
  hvf,ipa-granule-size=4096`, SPICE `gl=es` with ANGLE on Metal,
  `VK_DRIVER_FILES` at UTM's MoltenVK ICD, and `NPT_BACKEND=dxmt` with the
  `NPT_*_LIBRARY_PATH` variables at `dxmt-native.framework`. The render
  server is the reviewed Venus registration's `virgl_render_server` (built
  from virglrenderer `482f9d8b`, the commit UTM 5.0.5 ships, with Neptune);
  UTM's own `QEMURenderServer` is App-Sandbox-signed and traps in
  `_libsecinit_appsandbox` outside UTM's QEMUHelper, so it is not used.
  Findings from `win11v` (M3 Pro, macOS 27, 4 vCPUs, 8 GiB), 2026-10-26:
  - **Driver version must match the host.** Guest tools 0.1.274 (the file
    UTM's "latest" link points to) speak Neptune wire format 2; the UTM 5.0.5
    runtime speaks format 1 (`neptune: virtgpu: wire format mismatch
    host=0x00000001 guest=0x00000002`). Use **0.1.273**
    (`.local/iso/utm-guest-tools-0.1.273.iso`, SHA-256 `7d2c0343…c2f04601`
    from GitHub's asset digest, viogpu3d DriverVer 08/30/2026).
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
  Not established: D3D12, real games, frames on the console (the swapchain
  paces at 60 Hz, but nobody looked at the window), long runs, and the
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
  directory is left afterwards. Calls take 1.2 to 3.8 s. Non-ASCII text
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
  `systemctl --no-block poweroff` through `exec`, because the micro-VM kernel
  has no driver for the virt power button. Without `-t` the stop waits 30 s
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
  isolated. The blacklist lives in `/run/modprobe.d/veynodevm.conf`.
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
  QEMU the daemon refuses `exec_start` until the worker reports its serial
  transport `ready`.
- QEMU containers (other than `--init systemd`) stop with a hard stop: container-init does not handle the
  virt power button and the serial agent has no shutdown permission, so the
  entrypoint gets no SIGTERM.
- `--desktop` without `--gpus` runs on Apple Virtualization with a fixed
  1280x800 2D scanout and llvmpipe rendering; no dynamic resolution.
- `veynodevm desktop` opens the Apple Virtualization window, or for `--gpus`
  containers the SPICE console window (`runtime/build-qemu-console.sh`, then
  `scripts/install-dev-runtime.py --qemu`; without it QEMU desktops have no
  window). The QEMU window starts view-only; keyboard and pointer input begins
  when the window is focused.
- The browser desktop ([docs/browser-desktop.md](browser-desktop.md)) on
  Apple Virtualization serves one viewer per container at a time and does not
  show the guest cursor (the browser draws its own). Desktop containers created
  before the Apple path existed need a restart to boot the agent that serves
  it. Firefox needs the H.264 option (no HEVC in WebCodecs).
- Alpine 3.22 packages no Venus ICD; Vulkan GPU workloads need an image with
  Mesa's virtio driver (`veynodevm/ubuntu-gpu`).
