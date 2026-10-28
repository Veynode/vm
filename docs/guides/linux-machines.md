# Full Linux distributions (UEFI machines)

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

Verified: Ubuntu Desktop 24.04.5 arm64 and Fedora Workstation 44 aarch64 install unattended and boot from disk into GNOME with the virgl renderer; see [known issues](../known-issues.md#full-linux-distributions-uefi-machines) for what is verified and the limits.
