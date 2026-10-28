# VeynodeVM for Veynode core

Status: **API 1** (2026-10-28). Core's `veynodevm` plugin ([0027](decisions/0027-veynode-core-mac-engine.md),
[0028](decisions/0028-core-split-and-api-1.md)) drives machines only through the daemon's local API, which
[api.md](api.md) describes. Core's fake daemon (`packages/plugins/veynodevm/tests/helpers/fake-daemon.ts`) is the
reference for the shapes it uses, and [crates/api/tests/vectors](../crates/api/tests/vectors/wire.json) holds the
examples both sides test against. This page says what core installs, how it starts the daemon, and what has been
verified.

## What core installs

Core's installer unpacks the release directory that `scripts/package-release.py` assembles (`bin/`, `libexec/`,
`VeynodeVM.app`, `share/boot/`, `share/guest/`, `web/`, `install.py`, `MANIFEST.json`) and runs its `install.py`,
which registers it in the home's `runtime.json`. It is the same archive as the standalone product, and both share
one home and one daemon. The QEMU/HVF and GPU runtime is not in it: its redistribution follows
[0026](decisions/0026-gpu-runtime-source-build.md), and until then a home registers UTM's runtime itself.

## Starting the daemon

```
veynodevm daemon start    # a launchd user agent: dev.veynode.veynodevm, or one per VEYNODEVM_HOME
veynodevm daemon status
```

The socket is `<home>/state/control.sock`. A connection opens with `hello`:

```
{"method":"hello","arguments":{"api":1,"client":"veynode-core/1"}}
{"result":{"api":1,"daemon":"0.1.0","methods":[…],"home":"…","runtime":{"appleVirtualization":true,"qemu":true,
 "gpu":{"virgl":true,"venus":false,"neptune":true}}}}
```

- `methods` is core's capability check: a method that is missing is not offered by this daemon.
- `runtime` says which backends start here. `gpu` is present only with a QEMU runtime; `gpu.virgl` lets a Linux
  machine run on QEMU/HVF, and `gpu.neptune` decides whether a Windows machine gets the Neptune GPU or the basic
  display.
- An `api` the daemon does not speak gets a `version` error and the connection closes. Additions (methods, fields)
  keep `api` 1; a removal or a change of meaning raises it.

## Methods

| Core needs | Method |
| --- | --- |
| list, inspect | `machine.ls`, `machine.inspect` |
| create (Linux cloud image, macOS Tart image, Windows ISO with the unattended install) | `machine.create` (stream) |
| lifecycle | `machine.start` (stream), `machine.stop`, `machine.rm`, `machine.update`, `machine.clone` |
| run commands | `machine.exec` (stream: stdin, stdout, stderr, signals, exit status) |
| files | `files.copy` (stream) |
| ports and addresses | `machine.endpoints` |
| screen | `machine.display`: a loopback endpoint in core's screen-stream protocol |
| sound | `machine.sound`, `machine.microphone`: loopback endpoints of raw s16le 48 kHz PCM, QEMU machines only |

Arguments, results and refusals are in [api.md](api.md#machines). Errors carry a `code` (`invalid`, `not_found`,
`state`, `unsupported`, `version`, `failed`) that core can act on without reading the message.

What changed for core when this merged onto the split:

- `machine.display` is the screen endpoint `{name, epoch}`, as core uses it. The standalone product's resolution
  call, which had the same name, is `machine.resolution`.
- `machine.create` with `disk` takes qcow2 as well as raw, and `source.digest` says which image a machine came from.
- `ports` work on Apple Virtualization machines too: the daemon publishes them on 127.0.0.1.
- `machine.exec` and `files.copy` on a Linux cloud-image machine run over SSH, with no 30 s or 64 KiB limit.
- Windows `unattended` needs `driversIso` and no longer `attach`.

## Verified, and what is not

Before the merge, on the `machine-contract` branch, through the API, macOS 27, disposable homes:

- Linux on Apple Virtualization (Ubuntu 24.04 arm64 `server-cloudimg`): create, start, exec (exit codes, `root`,
  environment, working directory, 20 MB of stdin, concurrent execs, TERM, INT and KILL, hang-up when the client
  goes), `files.copy` of files and directories both ways, shares both ways, endpoints, the port publisher, update
  with a disk grow seen in the guest, clone with a new hostname, rm with and without force.
- Debian 13 genericcloud arm64 (qcow2) on Apple Virtualization: the converted disk matches Debian's raw image, and
  the guest's ext4 stays clean with the worker's cached, fsync'd disk.
- Linux on QEMU/HVF: `machine.sound` (a 440 Hz tone arrived at 440 Hz), the `machine.display` endpoint, its token
  and capabilities message, `gpu` none and virgl through `machine.update`.
- `hello.runtime.gpu` on a host with UTM's runtime: `neptune` true.

Not verified:

- `machine.microphone`: the daemon's frames reached the console broker, but the guest recorded silence.
- `veynodevm daemon start` for core's home (loading a launchd agent was not run here).
- The Windows install since it moved into the daemon, and the Neptune GPU with UTM's guest driver.
- macOS, skipped by decision.

Unit and fixture tests cover the SSH exec and copy commands, the lease lookup, the qcow2 converter, the cloud-init
seed, the Windows answer file, the screen-stream translation and the sound conversion.
