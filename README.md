# Veynode VM

Apple Silicon self-hosted VM product in active engineering validation. **Not a public-release build.** The required first release includes macOS ARM, Linux ARM and Windows ARM, native consoles, accelerated graphics, authenticated MCP and an end-to-end encrypted browser console. Several of those requirements are still unimplemented or blocked; see [status](docs/status.md) and [validation](docs/validation.md).

Implemented and exercised: Swift Apple VM worker with native console; persistent Rust daemon and stdio/HTTPS MCP; macOS and Linux Guest integration; control leases; memory admission; cross-worker writable-disk exclusion; rootless installed Linux command jobs with cancellation and human takeover; clean shutdown and offline APFS clone isolation. The opt-in QEMU/HVF controller now runs installed accelerated Linux desktops with bounded command execution and recovery. macOS supports actual screenshot, title-free window observation and bounded file transfer. macOS command execution remains unsupported and Accessibility/input acceptance is pending.

The browser console has real localhost H.264/HEVC hardware-video, paired authentication and QEMU input validation, including local TURN tests. Internet connectivity, complete sensitive-control E2EE and release packaging remain unfinished. Windows ARM installation and Direct3D acceleration remain release gates. No Parallels performance superiority is claimed. See [current evidence and exact limitations](docs/status.md), [macOS observation](docs/evidence/macos-windows-live/README.md), and [installed Linux recovery](docs/evidence/installed-qemu-retention-recovery-live/README.md).

## Build and real validation

Host: Apple Silicon macOS14+, tested M1 Pro16GiB/macOS26.4.1. Install Swift Command Line Tools and normal development dependencies (`brew install rust go qemu squashfs`). Full Xcode is not needed for the VZ worker; selected upstream graphics source builds need it. Do not replace installed applications or disable platform protections.

```sh
./scripts/check.sh
# Downloads pinned Alpine media (~416MiB), builds actual ARM64 guest appliance,
# boots real VMs, tests stdio/HTTPS control and faults; no mocks for acceptance:
./scripts/check.sh --real
# Verify with the official TypeScript MCP client as well:
npm ci --prefix tests/mcp-client
python3 scripts/test-control-http.py --config .local/run/linux-agent.json --node-sdk
```

The initial fixture is a real, diskless Linux kernel/initramfs service appliance, **not an installed graphical desktop**. Optional persistent Alpine installation:

```sh
python3 scripts/provision-linux-qemu.py
# Creates a NEW .local/vms/linux-persistent/disk.raw and refuses overwrite.
# Installation uses QEMU/HVF network; resulting config boots on Apple VZ.
python3 scripts/test-persistent-linux.py --config .local/vms/linux-persistent/config.json
python3 scripts/test-installed-jobs.py --config .local/vms/linux-persistent/config.json
# APFS clone isolation with real installed Guest writes (source must be stopped):
cargo build --release --locked --bin vm-image
python3 scripts/test-installed-clones.py --config .local/vms/linux-persistent/config.json --image-binary target/release/vm-image
```

The alternative `install-linux.py` records a failed VZ-network installation attempt on the current host; it is retained for diagnosis, not the default installer. Media, disks, generated secrets and detailed diagnostic logs stay under gitignored `.local/`. Cross-process disk lock files live in the private `~/Library/Application Support/Veynode/locks/` directory. Scripts do not touch existing Parallels VMs.

## Use the real control path

Start a daemon in its own terminal/process:

```sh
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

Configure your MCP client to launch the last command as a stdio server. Observation is default; execution and lifecycle are separate opt-ins. Acquire VM control before execution. Use vm_exec_start/poll/cancel for cancellable commands; cancellation acknowledgement is not finished state. Installed fixture commands run as the locked non-root veynode account, while separately authorized guest shutdown uses the init system. Read [control usage and HTTPS security boundaries](crates/control/README.md) before exposing any listener; development tests bind localhost and do not disable certificate checks.

[Native backend](native/README.md) · [Product requirements](docs/product-spec.md) · [Architecture](docs/architecture.md) · [Security](docs/security.md) · [Windows GPU gate](docs/decisions/0001-windows-gpu.md) · [Linux graphics evidence](docs/decisions/0002-linux-graphics.md) · [Benchmark contract](benchmarks/README.md)
