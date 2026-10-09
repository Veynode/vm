# One-shot Guest service crash fixture

Test-only code, not a product Guest service or distributable image. No production
Go source, command permissions, GPU configuration or original disk is changed.
The diagnostic intentionally allows the fixture's UID 1000 to arm one precisely
pinned service SIGKILL through a precreated file. Do not install it elsewhere.

The watcher requires Linux ARM64 root and a single matching
`veynode.guest_crash_once` boot nonce. Before its 120-second wait it consumes and
fsyncs a root-owned, exclusive one-shot stamp. `/run/veynode-agent.pid` must name a
positive PID; a pidfd pins it before verification. UID, complete argv, executable
inode/device/hash and `/proc` start time must match the frozen original service.
They are rechecked immediately before `pidfd_send_signal(SIGKILL)`. Only that
service PID is signalled, never its process group, a guessed process or QEMU.
The fixed arm inode is UID 1000 / 0600 under a root-owned 0711 parent; the writer
can overwrite its content but cannot replace the inode. State is root 0700.

The kernel must provide ARM64 Linux pidfd and ppoll syscalls; unsupported kernels
refuse. The watcher neither enables service restart nor claims that SIGKILL
performs cgroup cleanup. The actual acceptance must prove host fail-stop of the
original VM and preserve cleanup-unconfirmed results.

Build from repository root (no GPU or VM involved):

```sh
mkdir -p .local/guest-crash-watcher-new
(cd scripts/fixtures/guest-crash && go test -race ./... && go vet ./...)
(cd scripts/fixtures/guest-crash && \
  CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath \
  -o ../../../.local/guest-crash-watcher-new/watcher .)
python3 scripts/test-qemu-guest-crash-preparation.py
```

The initial checked build is `.local/guest-crash-watcher-v1/watcher`, SHA256
`b42ab5c23843b8023857a07e822552e88bc45c38b7833bb24e79861da4df2edf`.
Four Go host tests with race checking, vet, ARM64 static crossbuild and three
Python tests passed. None invokes a real pidfd signal or starts a Guest.

Preparation below imports a **stopped** maintained execution-v2 disk under
`vm-image`'s lock into a new read-only base and a distinct writable APFS clone.
It hashes source before/after and both copies. Supply the independently measured
full source SHA; never guess it from the earlier unmodified desktop base.

```sh
SOURCE="$PWD/.local/installed-qemu-exec-v2-fixture-v4/images/clones/exec-v2/disk.raw"
SOURCE_SHA=REPLACE_WITH_MEASURED_FULL_SHA256_OF_STOPPED_SOURCE
FIXTURE="$PWD/.local/qemu-guest-crash-fixture-v1"
python3 scripts/prepare-qemu-guest-crash.py \
  --source-stopped --source "$SOURCE" --source-sha256 "$SOURCE_SHA" \
  --config .local/installed-qemu-exec-v2-fixture-v4/installed.json \
  --directory "$FIXTURE" --watcher .local/guest-crash-watcher-v1/watcher \
  --watcher-sha256 b42ab5c23843b8023857a07e822552e88bc45c38b7833bb24e79861da4df2edf
```

Preparation does not boot or mount any disk. After inspecting `preparation.json`,
the operator may launch its dedicated trusted controller using the frozen
QEMU/SPICE/runtime registration, then run:

```sh
python3 scripts/test-qemu-guest-crash-maintenance.py \
  --fixture "$FIXTURE" --binary .local/installed-qemu-exec-bin-v5/vm-control \
  --state "$PWD/.local/control-guest-crash-v1" \
  --output "$PWD/.local/evidence/guest-crash-maintenance-v1"
```

The RAM updater requires its explicit maintenance boot flag, rootfs/tmpfs root,
and the supplied descriptor-locked `/dev/vda`. It verifies the old agent,
OpenRC/cgroup script, graphics module/boot script and ACPI handler before and
after adding the watcher. It refuses an existing watcher directory. The
maintenance config omits `qemu_guest_exec`; the installed config enables it and
contains the one-shot nonce. Never boot the installed fixture before the
acceptance observer is ready: the watcher deliberately consumes its one chance
even on timeout/refusal. Failed fixtures remain evidence and must not be retried
by deleting their stamp.

Successful RAM maintenance proves only the isolated update and worker exit;
actual installed service crash, VM termination and cleanup failure reporting
remain separate acceptance steps.

After successful maintenance, use the same dedicated daemon, keeping this
installed VM ID unregistered until the harness runs:

```sh
python3 scripts/test-qemu-guest-crash.py \
  --fixture "$FIXTURE" --binary .local/installed-qemu-exec-bin-v5/vm-control \
  --worker .local/installed-qemu-exec-bin-v5/vm-qemu \
  --state "$PWD/.local/control-guest-crash-v1" \
  --output "$PWD/.local/evidence/guest-crash-installed-v1"
```

It checks installed ext4/15 graphics cases, the real watcher-ready record, disk
descriptor identity, and the worker→guardian→QEMU process chain. It obtains an
actual long-command job ID, verifies a `setsid`/fork descendant's ticks are
increasing, then sends an ordinary UID1000 execution request to write the arm
file. The arm request's ACK is not used as kill evidence: service death may
interrupt it. Three ordered serial witness records must match the same frozen
service PID/start-time/inode/hash. The original host process incarnations must
all disappear within a 15-second observation bound, new execution must be denied,
and the original MCP session must close with explicit error exit **1**. A client
timeout or forced client kill does not count as that response.

This crash intentionally has no normal ACPI shutdown and no empty-cgroup ACK.
The safety result is host termination of the original VM, not a claim that the
killed service itself cleaned its cgroup. The imported immutable base is hashed
again; the crashed writable clone is retained as evidence. The script neither
restarts that VM nor deletes its once stamp. The operator separately retires the
dedicated controller.

`python3 scripts/test-qemu-guest-crash-harness.py` runs three host-only evidence
parser tests (identity replacement, missing/duplicate/reordered/refusal records,
partial serial records and valid proof). These are not actual crash acceptance.
