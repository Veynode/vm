# VeynodeVM for Veynode core

Status: **API 1** (2026-10-28). Core's `veynodevm` plugin ([0027](decisions/0027-veynode-core-mac-engine.md),
[0028](decisions/0028-core-split-and-api-1.md)) drives machines only through the daemon's local API, which
[api.md](api.md) describes. Core's fake daemon (`packages/plugins/veynodevm/tests/helpers/fake-daemon.ts`) is the
reference for the shapes it uses, and [crates/api/tests/vectors](../crates/api/tests/vectors/wire.json) holds the
examples both sides test against. This page says what core installs, how it starts the daemon, and what has been
verified.

## What core installs

Core's installer unpacks the archive that `scripts/package-release.py --qemu-runtime --archive` writes: a
`VeynodeVM/` directory with `bin/` (`veynodevm`, `vm-control` and the daemon's siblings), `libexec/` (host helpers and
`libexec/qemu/`, the QEMU/GPU runtime), `VeynodeVM.app`, `share/` (`runtime.json`, `boot/`, `guest/`), `LICENSES/`,
`install.py` and `MANIFEST.json`. It is the same layout as the standalone product, and both share one home and one
daemon. Core runs `bin/veynodevm daemon start` from the unpacked directory and nothing else: no Python, no
registration. The daemon finds its pieces through `share/runtime.json`, relative to itself, and a `daemon start` from
a newer version's directory moves the LaunchAgent onto it, restarting the daemon, which stops the machines it runs.

The runtime is UTM 5.0.8's QEMU and the frameworks it loads (virglrenderer, libepoxy, ANGLE, GLib and the rest),
the SPICE server with VeynodeVM's reconnect fix, QEMU's firmware and the native console app, re-signed with the
release's identity; `LICENSES/NOTICE` lists each component's version, source and licence. Venus and Neptune are not
in it (their registration pins the unsigned bytes), so `hello.runtime.gpu` reports `virgl` only, and a Windows
machine gets the basic display. Its proper source build is [0026](decisions/0026-gpu-runtime-source-build.md).

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
- Windows `unattended` with `gpu` `neptune` installs UTM's viogpu3d driver at the first logon. The daemon downloads
  UTM's guest tools 0.1.274 (Neptune wire format 2, pinned size and SHA-256; 0.1.273 with format 1 until the runtime moved to UTM 5.0.8) into `<home>/downloads` and attaches
  them; nothing is shipped.
- `machine.display` on QEMU sends the pointer shape (2) after input 5 and copied text (3) after input 6, and lists
  inputs 17 and 18 (the viewer's size, sent to the Guest as its monitor) in its capabilities (11).
- `machine.display` endpoints and epochs are per client (`hello.client`): another client's viewer, such as the web
  UI's, with a larger epoch no longer cuts core's. Sound and microphone endpoints stay one per machine.
- Shares of a Linux cloud-image machine on QEMU mount 9p with `cache=mmap`, so files on them take writable
  `MAP_SHARED` maps (Chromium's disk cache, SQLite's WAL). The seed is written at create, so a machine created
  before this keeps its `/etc/fstab` until that line gains `cache=mmap` and the share is mounted again.

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

After the merge, through the branch's SPICE helper and the owner's running daemon: `machine.display` on a Debian 13
machine on QEMU/HVF with virgl sends video (53 frames in 10 s at 1280x800, the console upright). Its console
switches to a scanout with `y0top` set, which the helper used to refuse, leaving the endpoint without video.

The release archive (`package-release.py --qemu-runtime --archive`, signed with an Apple Development identity,
hardened runtime) from an isolated unpack and `VEYNODEVM_HOME`: `daemon start` with no registration, and again from a
second directory, which rewrote the LaunchAgent; `hello.runtime` `qemu` and `gpu.virgl` true; a Debian 13 machine on
QEMU/HVF created, started, ran `machine.exec` (exit 0), reported `gpu` virgl, stopped and was removed, with the QEMU
worker on the archive's SPICE server. A Linux machine on Apple Virtualization started, but `machine.exec` timed out:
see [Local Network privacy](known-issues.md#networking).

Not verified:

- `machine.microphone`: the daemon's frames reached the console broker, but the guest recorded silence.
- `veynodevm daemon start` for core's own home and programs directory.
- The Windows install since it moved into the daemon, and its Neptune driver step (the driver itself was verified
  installed by hand, see [known issues](known-issues.md#windows-11-arm64-machines)).
- The pointer shape, copied text and monitor size on `machine.display`: unit tests, and the SPICE helper compiles
  against UTM's headers; no live run. The pointer's byte order (spice-gtk's ARGB32, swapped to RGBA) is unchecked.
- Any other live run after the merge: the host had under 20 GiB free.
- macOS, skipped by decision.

Unit and fixture tests cover the SSH exec and copy commands, the lease lookup, the qcow2 converter, the cloud-init
seed, the Windows answer file, the screen-stream translation and the sound conversion.
