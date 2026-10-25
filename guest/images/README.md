# Official images

Built with the shared engine and imported for micro-VMs:

```bash
veynodevm engine start
veynodevm build -t veynodevm/alpine-desktop -f guest/images/alpine-desktop/Dockerfile guest/images
veynodevm build -t veynodevm/ubuntu-desktop -f guest/images/ubuntu-desktop/Dockerfile guest/images
veynodevm build -t veynodevm/ubuntu-gpu guest/images/ubuntu-gpu
```

The desktop images share `desktop/veynodevm-desktop` (udev, Xorg on the
virtio-gpu scanout, XFCE), so their build context is `guest/images`.

| Image | Contents | Run |
|---|---|---|
| `alpine-desktop` | Xorg (modesetting on virtio-gpu), XFCE, Mesa virgl + llvmpipe | `veynodevm run -d --desktop [--gpus virgl] veynodevm/alpine-desktop`, then `veynodevm desktop <name>` |
| `ubuntu-desktop` | Xorg + XFCE, Mesa virgl (GL) and Venus (Vulkan), vulkan-tools | `veynodevm run -d --desktop --gpus venus veynodevm/ubuntu-desktop` |
| `ubuntu-gpu` | Mesa virgl (GL) and Venus (Vulkan virtio ICD), vulkan-tools, mesa-utils | `veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu` |

Without `--gpus`, a desktop runs on Apple Virtualization with a 2D virtio-gpu
scanout (software rendering, llvmpipe). `--gpus virgl|venus` selects QEMU/HVF
with the registered UTM-derived runtime, where the guest renders through
virglrenderer (GL) or Venus → MoltenVK → Metal (Vulkan, experimental).
Alpine 3.22 does not package the Venus ICD, so Vulkan GPU work uses
`ubuntu-gpu` or `ubuntu-desktop`. Venus renders offscreen and presents to X11
(DRI3); importing GBM/KMS buffers into Venus does not work on macOS hosts
(see docs/known-issues.md).
