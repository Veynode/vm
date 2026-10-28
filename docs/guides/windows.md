# Windows 11 ARM64 machines

Windows runs as a QEMU/HVF machine installed from your own Windows 11 ARM64 ISO (Apple Virtualization has no Windows support). `--os windows` puts the ISO and seed images on USB CD-ROMs (Setup reads them before any driver loads), the disk on virtio-blk (`--disk-bus nvme` for the inbox NVMe driver instead), and adds virtio-net, virtio-serial, balloon, RNG, virtio keyboard/tablet and pvpanic; their drivers come from the virtio-win ISO. The display is UTM's `virtio-ramfb` (Basic Display, no GPU acceleration); `--gpus neptune` is an experimental Triton profile with hardware D3D11 through DXMT on Metal, pixel-checked and benchmarked, which needs UTM's guest tools 0.1.273 in the guest. With Neptune the guest defaults to 1280x1024; `veynodevm machine display NAME 1920x1080` sets the mode (the driver lists up to 2048x1152; it persists across boots) through that SSH login. Guest reboots restart QEMU, because edk2 hangs after an in-process reset. `install-distro.py windows` also sets up Windows' OpenSSH Server with a generated key, forwarded to `127.0.0.1:2222` (`machine create --publish HOST:GUEST`), and records that login (`veynodevm machine ssh-key`) so the MCP tools `vm_exec`, file transfer, `vm_desktop_state`, window actions, app launch and the UI Automation accessibility tree work over it, without installing anything in the guest. There is no TPM, no Secure Boot key enrolment and no guest agent, so Setup's Windows 11 checks are bypassed by the answer file.

```sh
scripts/install-distro.py windows --name win11 --iso ~/Downloads/Windows11_Client_arm64_en-us.iso \
    --drivers-iso virtio-win.iso --evidence .local/evidence/windows-installed.png
# or by hand: veynodevm machine create win11 --os windows --iso WIN.iso --attach answer.iso --monitor
```

The script writes an `Autounattend.xml` seed (Windows 11 Pro with the generic, non-activating key unless `--edition`/`--product-key` say otherwise; local administrator `veynode` with automatic logon), taps Enter for the CD's "Press any key" prompt, loads the virtio storage and network drivers in Setup and the rest offline, waits for the first logon to power off, ejects the media and boots the disk. See [known issues](../known-issues.md#windows-11-arm64-machines) for what is verified.

The first logon also tunes the guest the way development and test VMs usually are (`--no-guest-tuning` keeps Windows' defaults); each step is an idempotent command in `FirstLogonCommands`, listed in `GUEST_TUNING` in the script:

- the High performance power plan (`powercfg /setactive SCHEME_MIN`), before the display and sleep timeouts are turned off (always) and hibernation with it (always, for Fast Startup);
- SysMain (prefetching), Windows Search (indexing) and DiagTrack (telemetry) disabled and stopped;
- Delivery Optimization limited to HTTP downloads, no peer-to-peer (policy `DODownloadMode` 0);
- for the automatic-logon user, visual effects set to "Adjust for best performance" (`VisualFXSetting` 2 with its effects mask and no minimize animation; font smoothing stays on, so screenshots stay legible) and transparency off; they take effect from the next logon;
- TRIM enabled (`fsutil behavior set DisableDeleteNotify 0`, Windows' default), so blocks the guest frees are discarded through virtio-blk.

Windows Defender is left as it is.

With or without the tuning, the first logon also compiles the .NET Framework assemblies Windows queued for ARM64 (`ngen.exe executeQueuedItems`, ~30 s), which Windows otherwise does only when it next idles long enough. Without those native images every Windows PowerShell start JIT-compiles for ~1 s, and every exec, desktop and file call starts one: a `machine exec` of `cmd /c exit 0` took ~1.3 s, and ~0.3 s with them. On a machine installed before this, run that command once as an administrator (`C:\Windows\Microsoft.NET\FrameworkArm64\v4.0.30319\ngen.exe executeQueuedItems`).

The daemon reaches the guest over that SSH login. Calls to one machine share a single SSH connection (OpenSSH multiplexing): the first leaves a connection master in the background, with its socket in `<home>/state/ssh-mux` (0700), and later calls open a session on it instead of connecting and authenticating again. The master exits 60 s after its last session ends, or when the machine's QEMU process exits. If that socket path would exceed the 104-byte Unix socket limit (a very long home directory), every call connects on its own, as before. The web terminal always connects on its own.
