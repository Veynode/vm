# VeynodeVM for Veynode core

Status: **API 1** (2026-10-28). Core's `veynodevm` plugin ([0027](decisions/0027-veynode-core-mac-engine.md)) drives
machines only through the daemon's local API: [api.md](api.md) describes it, and
[crates/api/v1.schema.json](../crates/api/v1.schema.json) holds the shape of every request and result. This page
says what core ships, how it starts the daemon, and what has been verified.

## Layers

- **Core layer**: the daemon (`vm-control`), its workers and helpers, and API 1. This is all that core's archive
  holds, and all that the plugin calls.
- **Wrapper layer**: the `veynodevm` CLI, the MCP servers, the web UI and the browser desktop relay. They are
  clients of the daemon and are not in core's archive. `veynodevm --json machine …` gives the same results as the
  API methods it calls, but it is not the contract.

## Starting the daemon

Core runs the daemon of its own home, beside any daemon the owner runs:

```
vm-control --home H service install   # launchd agent dev.veynode.veynodevm.<hash of H>, started now and at login
vm-control --home H service status    # {"home","label","plist","installed","loaded","running","socket","log"}
```

The socket is `H/state/control.sock` ([api.md](api.md#the-daemon-service)). A connection opens with `hello`:

```
{"method":"hello","arguments":{"api":1,"client":"veynode-core/0.1.54"}}
{"result":{"api":1,"daemon":"0.1.0","methods":[…],"runtime":{"appleVirtualization":true,"qemu":true,
 "gpu":{"virgl":true,"venus":false,"neptune":true,"reason":{…}}}}}
```

- `methods` is core's capability check: a method that is missing is not offered by this daemon.
- `runtime` says which backends start here. `gpu.neptune` decides whether a Windows machine gets the Neptune GPU or
  the basic display.
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
| macOS permissions | `machine.grant` |
| screen | `machine.display`: a loopback endpoint in core's screen-stream protocol |
| sound | `machine.sound`, `machine.microphone`: loopback endpoints of raw s16le 48 kHz PCM, QEMU machines only |

Each method's arguments, results and refusals are in [api.md](api.md) and the schema. Errors carry a `code`
(`invalid`, `not_found`, `state`, `unsupported`, `version`, `failed`) that core can act on without reading the
message.

The screen and sound endpoints share these rules:

- The viewer sends the token and `\n` within 5 s, or the endpoint closes the connection.
- The newest viewer replaces the one before it.
- The same `epoch` answers the same endpoint. A new `epoch` starts a new one, with a new port and token, and cuts
  every viewer of the old one.
- An endpoint ends when its machine stops.

Apple Virtualization machines, macOS included, never offer sound. Core's own in-guest program carries it there.

## The archive

`scripts/package-core.sh [--debug] [--boot DIR] [--output FILE.tar.gz]` builds `veynodevm.app` and packs it as a
tar.gz under `.local/dist/`. It uploads nothing. The bundle contains:

- `Contents/MacOS`: `vm-control` (built without its MCP servers), `vm-qemu`, `vm-console`, `vm-console-stream`,
  `vz-desktop-stream`, `vm-vision` and `vm-macos-desktop`.
- `Contents/Library/vm-apple.app`: the Apple Virtualization worker, with the virtualization entitlement.
- `Contents/Resources`: `guest/veynode-agent`, optionally `boot/`, and `runtime.json`. Paths in `runtime.json` are
  relative to `Resources`. A home's own `runtime.json` wins field by field, and that is where the QEMU/HVF runtime
  is registered.

It has no CLI, no MCP, no web UI, gateway, remote-proto or session-auth, and no Python: the Windows unattended
install runs in the daemon. Signatures are ad hoc, and a release signs again with the Developer ID and notarizes.

## Verified, and what is not

Through the API, on 2026-10-28, macOS 27, disposable homes:

- Linux on Apple Virtualization (Ubuntu 24.04 arm64 `server-cloudimg`, sha256 `50e24e1f…`): create, start, exec
  (exit codes, `root`, environment, working directory, 20 MB of stdin by digest, concurrent execs, TERM, INT and KILL
  for the login user and root, hang-up when the client goes), `files.copy` of files and directories both ways,
  shares both ways, endpoints, the port publisher (also after a port update), update with a disk grow seen in the
  guest, clone with a new hostname and machine-id, rm with and without force.
- Linux on QEMU/HVF (the same image):
  - `machine.sound`: a 440 Hz tone played in the guest arrived as 48 kHz stereo at 440 Hz.
  - `machine.display`: the endpoint, its token, the capabilities message and the same-epoch answer. No video:
    this host's QEMU console delivered no frames to any client (`screenshot` failed the same way).
  - `gpu` none and virgl through `machine.update`.
- `hello.runtime.gpu` on a host with UTM's runtime: `neptune` true.

Earlier, through the CLI before the daemon took these over: `display` on Apple Virtualization (H.264 1920×1080, a
switch to HEVC, a new epoch cutting the old viewer, a bad token refused, input reaching the console), the archive
unpacked outside the checkout, and Windows 11 ARM64 on QEMU/HVF (the unattended install in about 20 minutes, exec,
cp, stop, update, start, endpoints).

Not verified:

- `machine.microphone`: the daemon's frames reached the console broker, but the guest recorded silence.
- `machine.display` video through the daemon, which needs a QEMU console that delivers frames, or a Linux machine
  with the guest agent on Apple Virtualization.
- `service install` (loading a launchd agent was not run here). `--home`, the per-home label and the socket were
  checked on a daemon started by hand.
- The Windows install since it moved into the daemon, and the Neptune GPU with UTM's guest driver.
- macOS, skipped by decision. These need a live run:
  - `machine.create` with `from` (`ghcr.io/cirruslabs/macos-tahoe-vanilla`) and its first boot (key, Screen
    Sharing, sudoers).
  - `machine.exec` and `files.copy` over SSH, and `root`.
  - `machine.grant` for Screen Recording and Accessibility.
  - Shares at `/Volumes/TAG`, endpoints, display, stop and clone.

Unit and fixture tests cover the API framing and refusals (`crates/control/tests/api.rs` runs a real daemon), the
exec wrapper, the lease lookup, the qcow2 converter, the cloud-init seed, the Windows answer file, the screen-stream
translation, the sound conversion and the launchd plist.
