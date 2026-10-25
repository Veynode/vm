# Apple virtualization worker

Build on Apple Silicon macOS 14 or newer using `scripts/build-native.sh`.
The result is ad-hoc signed with `com.apple.security.virtualization` and linked
at `.local/bin/vm-apple`. An ad-hoc signed development bundle is also built at
`.local/VeynodeVM.app` (bundle identifier `com.veynode.vmworker.dev`) so native
window automation can identify the worker. A distributable signed application
is not produced.

One worker owns one real `VZVirtualMachine`. The control service retains its
stdin pipe; an optional AppKit window displays `VZVirtualMachineView` directly.
Closing that window does not stop the VM. Closing the owning stdin pipe forcibly
stops it (or cancels installation), so a service crash cannot leave an orphan.
Service restart recovery remains a product requirement; this behavior is not
VM persistence across a service failure.

## CLI and protocol

- `vm-apple probe`: host capabilities, explicit unsupported/unverified fields.
- `vm-apple latest-restore`: Apple's supported restore URL and CPU/memory minimum.
- `vm-apple run --config FILE`: start an existing Linux or macOS machine.
- `vm-apple install --config FILE`: install macOS using a local IPSW into a new
  identity and caller-provided writable raw disk. Refuses existing identity.

Config requires `id`, `guest` (`linux` or `macos`), `cpus`, and `memory_mib`.
Linux also requires an **uncompressed ARM64 Image** `kernel`; optional `initrd`
and `command_line` (default `console=hvc0`). EFI zboot containers must first be
extracted; the direct kernel loader cannot load them. macOS requires `disk` and
`machine_identity_dir`; installation also requires `restore_image`.

Optional config:

- `window`: false by default.
- `width`/`height`: 2560×1440 default, bounded to 640–7680 × 480–4320.
- `serial_log`: append serial output to this file, enables `serial_write`.
- `disk`: existing raw writable disk image.
- `shared_dir`: existing directory exported **read-only**, virtiofs tag `veynode`.
  On a Guest supporting virtiofs: `mount -t virtiofs veynode /mnt/veynode`.
  Never silently exports the host home directory.

stdin/stdout are newline-delimited JSON. Stdout is protocol-only. Events include
`{"event":"state","state":"running","vm_id":"..."}`; responses retain the
request `id`. Requests are bounded to 1 MiB; oversize lines are discarded through
the next newline. Methods:

- `status`, `shutdown` (requests cooperative Guest shutdown), `stop` (force).
- `show_window`, `close_window`.
- `serial_write` with base64 `data` (up to 64 KiB decoded).
- `guest_request` with `request: {id, method, ...}`. Connects to virtio-vsock port
  5000, sends one JSON line, reads one response. Request ≤64 KiB, response ≤1 MiB,
  35 second total deadline including connection, at most 16 requests in flight.
  Returns the checked Guest envelope inside the native `result`; callers must
  propagate a nested Guest `error`. Response IDs must match and exactly one of
  `result`/`error` must exist. This channel needs a Guest integration service.

`stop` is rejected during macOS installation because the Apple installer
specifies undefined behavior when the VM is stopped or paused during install.
Unsupported methods return an explicit error. Local ownership and permissions
belong to the control service; this worker must not be exposed to the internet.

## Verification

`python3 native/tests/lifecycle.py --config .local/run/linux-smoke.json --window`
requires the real Linux direct-kernel fixture prepared by the repository script.
It boots the kernel, checks native window close continuity, executes a real
serial shell command, and verifies owner-EOF cleanup. Evidence is saved in
`archive/docs-evidence/apple-linux-lifecycle.txt`. It is a boot/console test, not a
Linux desktop or 3D acceptance test.

`native/tools/build-probes.sh` builds standalone diagnostics in `.local/bin`:

- `metal-probe`: run **inside a macOS Guest** for Guest Metal evidence. Reports
  actual Metal device/families, validates a compute kernel with raw timings, and
  renders an offscreen triangle with verified pixel readback.
  Compute correctness and timings are not game/CAD graphics performance.
- `h264-probe`: requires VideoToolbox hardware H.264 encoding, verifies the
  encoder reports hardware use, encodes 120 synthetic 1080p frames with bounded
  pending work, and checks output count/status. Host results prove only host
  encoding availability; capture, remote transport, browser decoding, dynamic
  adaptation, and end-to-end encryption remain separate work.

Existing host probe outputs are in `archive/docs-evidence/apple-host-*.json`. No result
establishes parity with or superiority to Parallels.

For a verified local macOS IPSW:

```sh
python3 scripts/prepare-macos.py --restore-image /absolute/path/Restore.ipsw
python3 native/tests/install-macos.py --config .local/vms/macos-smoke/config.json
```

The preparer makes a private 64 GiB sparse raw disk, 4 CPU/4 GiB config, and copies
built diagnostics into the read-only Guest share. The installer stores progress,
raw events, errors, and final status beside the config. Successful preparation is
not installation evidence. Interrupted installation is recorded as failure;
no automatic overwrite/reinstall is attempted. A fresh fixture can be created
with `--name another-name` after inspecting the failed state. Initial macOS
Setup Assistant remains interactive after installation.

## Writable disk ownership

The worker opens each writable disk and verifies a regular file, then holds an
exclusive advisory lock on a private key derived from the opened `(device,inode)`
under `~/Library/Application Support/Veynode/locks/`. This directory is shared by
all checkouts for the current user; directories and files are checked for safe
ownership/permissions and opened without following final-component symlinks.
Hard links converge on the same key. The disk descriptor and lock remain open
for the worker lifetime, and the path's inode is rechecked before attachment.

A direct `flock` on the raw image was tested and **prevents VZ startup**, because
Virtualization.framework's separate VM process acquires its own image lock.
The inode key avoids that conflict. Independent old VZ workers were also tested:
a second writable attachment through a hard link fails while the first runs.
Our advisory lock is a cooperating-worker policy; other hypervisors need not
honor it. A privileged or same-user malicious host can bypass application
policies and is outside this isolation guarantee.

`python3 native/tests/disk-lock.py --config .local/run/linux-smoke.json` verifies
real boot, concurrent hard-link rejection without harming the first VM, and
lock release/reuse after worker exit. Evidence: `apple-disk-lock-tests.txt`.
