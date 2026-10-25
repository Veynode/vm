# Official images

Built with the shared engine and imported for micro-VMs:

```bash
veynodevm engine start
veynodevm build -t veynodevm/alpine-desktop guest/images/alpine-desktop
veynodevm build -t veynodevm/ubuntu-gpu guest/images/ubuntu-gpu
```

| Image | Contents | Run |
|---|---|---|
| `alpine-desktop` | Xorg (modesetting on virtio-gpu), XFCE, Mesa virgl + llvmpipe | `veynodevm run -d --desktop [--gpus virgl] veynodevm/alpine-desktop`, then `veynodevm desktop <name>` |
| `ubuntu-gpu` | Mesa virgl (GL) and Venus (Vulkan virtio ICD), vulkan-tools, mesa-utils | `veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu` |

Without `--gpus`, a desktop runs on Apple Virtualization with a 2D virtio-gpu
scanout (software rendering, llvmpipe). `--gpus virgl|venus` selects QEMU/HVF
with the registered UTM-derived runtime, where the guest renders through
virglrenderer (GL) or Venus → MoltenVK → Metal (Vulkan, experimental).
Alpine 3.22 does not package the Venus ICD, so Vulkan GPU work uses
`ubuntu-gpu`. Venus renders offscreen; presenting through KMS/WSI does not
work yet (see docs/known-issues.md).
