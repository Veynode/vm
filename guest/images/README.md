# Official images

Built with the shared engine and imported for micro-VMs:

```bash
veynodevm engine start
veynodevm build -t veynodevm/alpine-desktop -f guest/images/alpine-desktop/Dockerfile guest/images
veynodevm build -t veynodevm/ubuntu-desktop -f guest/images/ubuntu-desktop/Dockerfile guest/images
veynodevm build -t veynodevm/ubuntu-gpu guest/images/ubuntu-gpu
veynodevm build -t veynodevm/ubuntu-gnome guest/images/ubuntu-gnome
```

The desktop images share `desktop/veynodevm-desktop` (udev, Xorg on the
virtio-gpu scanout, XFCE), so their build context is `guest/images`. Xorg runs
as root; the XFCE session runs as `VEYNODEVM_DESKTOP_USER` when the image has
that user (`ubuntu-desktop` sets `veynode`, in groups `input video render`),
and as root otherwise (`alpine-desktop`). The Guest's computer-use helper,
cua-driver, refuses a root session.

| Image | Contents | Run |
|---|---|---|
| `alpine-desktop` | Xorg (modesetting on virtio-gpu), XFCE, Mesa virgl + llvmpipe | `veynodevm run -d --desktop [--gpus virgl] veynodevm/alpine-desktop`, then `veynodevm desktop <name>` |
| `ubuntu-desktop` | Xorg + XFCE as user `veynode` (uid 1000), Venus (Vulkan), OpenGL through Zink on Venus or virgl, vulkan-tools, AT-SPI, the pinned cua-driver (`runtime/cua-driver.json`) | `veynodevm run -d --desktop --gpus venus veynodevm/ubuntu-desktop` |
| `ubuntu-gpu` | Mesa virgl (GL) and Venus (Vulkan virtio ICD), vulkan-tools, mesa-utils | `veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu` |
| `ubuntu-gnome` | Full Ubuntu 24.04 system: systemd as PID 1, GDM autologin (user `ubuntu`, password `ubuntu`) into GNOME Shell on Xorg, NetworkManager, Mesa | `veynodevm run -d --init systemd --desktop [--gpus virgl] --name gnome veynodevm/ubuntu-gnome`, then `veynodevm desktop gnome` |

Without `--gpus`, a desktop runs on Apple Virtualization with a 2D virtio-gpu
scanout (software rendering, llvmpipe). `--gpus virgl|venus` selects QEMU/HVF
with the registered UTM-derived runtime, where the guest renders through
virglrenderer (GL) or Venus → MoltenVK → Metal (Vulkan, experimental).
Alpine 3.22 does not package the Venus ICD, so Vulkan GPU work uses
`ubuntu-gpu` or `ubuntu-desktop`. Venus renders offscreen and presents to X11
(DRI3); importing GBM/KMS buffers into Venus does not work on macOS hosts
(see docs/known-issues.md).

On Venus, `ubuntu-desktop` runs OpenGL through Mesa's Zink (OpenGL 3.1 and
GLES 3.1 on Vulkan) instead of virgl: its entrypoint sets
`MESA_LOADER_DRIVER_OVERRIDE=zink` for the XFCE session when the Vulkan driver
is `venus`. `veynodevm exec` does not inherit the session environment; pass
`-e MESA_LOADER_DRIVER_OVERRIDE=zink` there, or `=virtio_gpu` for virgl. The
image's implicit Vulkan layer (`zink/veynodevm_zink_layer.c`) works around
MoltenVK bugs that otherwise break every Zink draw; it changes nothing for
other Vulkan applications.

`ubuntu-gnome` boots its own init (`--init systemd`, see "Full system
containers" in docs/known-issues.md). Verified on Apple Virtualization
(GNOME on llvmpipe) and on QEMU with `--gpus virgl` (GLX renderer `virgl`,
OpenGL 2.1; the shell renders through Mesa gallium on virtio-gpu). The
squashfs root is about 440 MiB.
