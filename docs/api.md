# Local API 1

The daemon's interface for every client: the `veynodevm` CLI, the MCP server, the web UI and Veynode core's plugin
([decision 0028](decisions/0028-core-split-and-api-1.md)). The Rust implementation of both sides is
[crates/api](../crates/api); `crates/api/tests/vectors` holds byte-exact examples every implementation tests against.

## Connection

- The socket is `<home>/state/control.sock`, where `<home>` is `$VEYNODEVM_HOME` or
  `~/Library/Application Support/VeynodeVM`. It is mode 0600 and the daemon refuses a peer whose effective uid is not
  its own.
- One connection carries one call. The client opens it, says `hello`, sends one request, reads the answer and closes.
  Closing the connection cancels the call.
- Every message before a stream is one line of UTF-8 JSON ending in `\n`, at most 1 MiB (12 MiB for a `screenshot` or
  `driver_rpc` answer).

### hello

```json
{"method":"hello","arguments":{"api":1,"client":"veynodevm-cli/0.4.0"}}
```

```json
{"result":{"api":1,"daemon":"0.4.0","methods":["hello","machine.ls","…"],"home":"/Users/a/Library/Application Support/VeynodeVM","runtime":{"appleVirtualization":true,"qemu":true,"gpu":{"virgl":true,"venus":false,"neptune":true,"reason":{"venus":"…"}}}}}
```

`runtime.gpu` is present when a QEMU/HVF runtime is registered: the `gpu` values a QEMU machine can start with here,
and in `reason` why one cannot. The daemon checks the runtime's files once and answers from that check while their
sizes and modification times stay the same; `machine.start` checks again.

`methods` lists every method this daemon serves. A daemon that does not serve the asked `api` answers
`{"error":"…","code":"version"}` and closes. A daemon older than API 1 answers `hello` itself with an error and no
code; the bundled clients (`veynodevm-api`, `web/daemon.mjs`) report both as `version` and say to restart the
daemon, and name the daemon's version when it answers `unknown method` for a method they know. A first line that is not `hello` is a legacy request: the daemon answers
it once without a `code` and closes. Only VM-layer methods are served that way.

### Requests and answers

```json
{"method":"machine.inspect","arguments":{"name":"dev"}}
```

The answer is `{"result":…}` or `{"error":"<message>","code":"<code>"}`:

| Code | Meaning |
|---|---|
| `invalid` | The arguments are wrong: a missing field, a bad name, a size out of range |
| `not_found` | The machine, container, image or job does not exist |
| `state` | It exists but cannot do this now: running, stopped, busy, or the name is in use |
| `unsupported` | This daemon, backend or guest cannot do it |
| `version` | The client and daemon do not share an API version |
| `failed` | Anything else; the message says what |

A `failed` message that says the disk is full is reported by clients as such.

### Streams

A streaming method answers `{"result":{"stream":true}}`, after which the connection carries frames in both directions
until the daemon sends `end`:

```
u8 channel | u32 big-endian length | payload (at most 1 MiB)
```

| Channel | Direction | Payload |
|---|---|---|
| 0 `stdin` | client → daemon | bytes; an empty frame ends the input |
| 1 `stdout` | daemon → client | bytes |
| 2 `stderr` | daemon → client | bytes |
| 3 `event` | daemon → client | JSON, such as `{"progress":"Cloning the disk"}` |
| 4 `end` | daemon → client | JSON `{"result":…}` or `{"error","code"}`; the daemon closes after it |
| 5 `control` | client → daemon | JSON: `{"signal":"TERM"\|"INT"\|"HUP"\|"KILL"}`, `{"resize":{"rows","cols"}}`, or a method's own messages |

## Methods

Sizes are in MiB or GiB as their names say. Names are `[A-Za-z0-9_-]{1,64}` for machines and `[A-Za-z0-9_.-]{1,64}`
for containers; any method that takes a `name` also takes the record's `id`.

### Daemon

| Method | Arguments | Result |
|---|---|---|
| `hello` | `{api, client}` | see above |
| `daemon.status` | `{}` | `{pid, daemon, api, home, runtime, vms}` |

### Machines

A machine is a persistent VM: Linux from a disk image or an installer ISO, macOS from an OCI image, Windows 11 ARM64
from an ISO (QEMU only).

```ts
type Machine = {
  name: string; id: string; os: "linux" | "macos" | "windows";
  backend: "apple-virtualization" | "qemu-hvf";
  state: "stopped" | "starting" | "running" | "stopping" | "failed";
  source: { disk?: string; digest?: string; from?: string; iso?: string; driversIso?: string };
  cpus: number; memoryMiB: number; diskGiB: number;
  gpu: "virgl" | "venus" | "neptune" | null; macAddress: string | null;
  shares: { hostPath: string; tag: string; readOnly: boolean; guestPath: string | null }[];
  ports: { host: number; guest: number }[];
  createdAt: number; diskUsedBytes: number; icon?: string | null;
};
```

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `machine.ls` | call | `{}` | `Machine[]` sorted by name |
| `machine.inspect` | call | `{name}` | `Machine` |
| `machine.create` | stream | `{name, os, cpus, memoryMiB, diskGiB?, shares?, ports?, gpu?, backend?, diskBus?, attach?}` and one source: `{disk}` (a raw or qcow2 Linux disk; see below), `{from}` (an OCI reference; macOS), `{iso, unattended?, driversIso?}` | `Machine`; `progress` events |
| `machine.start` | stream | `{name, wait?}` | `Machine`; with `wait`, ends once the guest answers |
| `machine.stop` | call | `{name, force?}` | `{name}` |
| `machine.rm` | call | `{name, force?}` | `{name}` |
| `machine.update` | call | `{name, cpus?, memoryMiB?, diskGiB?, ports?, gpu?, newMac?}` (stopped; disks only grow) | `{name}` |
| `machine.rename` | call | `{name, newName}` (stopped) | `Machine` |
| `machine.clone` | call | `{name, newName}` (stopped; APFS clone, new identity) | `Machine` |
| `machine.eject` | call | `{name}` | `{name}` |
| `machine.icon` | call | `{name, icon: string \| null}` | `{name}` |
| `machine.ssh_key` | call | `{name, user, key}` | `{name}` |
| `machine.resolution` | call | `{name, width, height}` (a running Windows machine, through its login) | `{name, width, height}` |
| `machine.display` | call | `{name, epoch}` (running) | `{format:"stream", address, port, token, epoch}`; see [the screen endpoint](#the-screen-endpoint) |
| `machine.sound`, `machine.microphone` | call | `{name, epoch}` (a running QEMU machine) | `{format:"pcm", rate:48000, channels, address, port, token, epoch}`; see [the sound endpoints](#the-sound-endpoints) |
| `machine.endpoints` | call | `{name}` | `{address: string\|null, hostAddress: string\|null, ports: {host, guest, address}[]}` |
| `machine.exec` | stream | `{name, argv, user?, env?: ["K=V"], cwd?, stdin: bool, tty?: bool}` | `{exitCode}`; output on `stdout`/`stderr`, input on `stdin`, signals on `control` |

- **Linux disk images.** `disk` is an upstream cloud image (Ubuntu 24.04 or Debian 13 arm64, raw or qcow2). The
  daemon converts it to raw once per digest under `<home>/machine-images`, APFS-clones it per machine and grows the
  disk and its GPT to `diskGiB`; `source.digest` is the image's sha256. A NoCloud seed (`seed.iso`) gives the guest
  the login `veynode` with the home's key, passwordless `sudo`, the hostname, the shares at `/mnt/TAG` and the
  guest agent. `machine.clone` writes a new seed, so the clone gets a new hostname and machine-id.
- **Ports.** On QEMU, `ports` are user-mode forwards on 127.0.0.1. On Apple Virtualization the daemon publishes them
  from 127.0.0.1 to the guest's lease address while the machine runs. `machine.endpoints` lists both with
  `address` 127.0.0.1.
- **Windows.** `{iso, unattended: true, driversIso}` installs Windows 11 ARM64 without a person: the daemon writes the
  answer file seed, presses the ISO's key prompt, waits for the install (up to 3 hours) and ejects the media. The
  machine has the login `veynode` with its own key and OpenSSH, and is left stopped.
- **exec on Linux disk images** runs over SSH as `veynode` (the lease address on Apple Virtualization, a private
  loopback forward on QEMU), with no time or output limit. `user` `root` runs it through `sudo -n`; `cwd`, `env`,
  stdin and signals pass through, and the command gets a hang-up when the client goes. `machine.start` with `wait`
  ends once SSH answers. `files.copy` uses `scp` the same way, for files and directories. Other Linux machines use
  the guest agent.

### Files

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `files.copy` | stream | `{name, direction: "in"\|"out", hostPath, guestPath}` for a machine | `{exitCode}` |

A macOS machine (`machine.create {from, user?}`, a Tart image, pulled when missing) is a macOS container's Guest kept
as a machine: an APFS clone of the image disk (grown to `diskGiB`), the image's identity and login (`user`, default
`admin`; the password is `VEYNODEVM_MACOS_PASSWORD`, else Tart's). Its first `machine.exec` or `files.copy` finishes
the first boot over SSH (key, hostname, disk, then the Accessibility and Automation grant) with `progress` events;
commands run as the login, shares mount at `/Volumes/My Shared Files/TAG`, and `machine.clone` keeps the identity
with a new MAC address and hostname. Apple allows two macOS VMs running at once.

### The screen endpoint

`machine.display` starts a loopback TCP listener in the daemon that speaks core's screen-stream protocol (the
header of core's `apps/macos-runtime/native/screen-stream.swift`):

- The viewer sends `token` and a newline within 5 s, then input messages: kind u8 and a fixed-size payload.
- The endpoint sends kind u8, length u32 BE, payload: capabilities (11) first, then video (1: width and height u16
  BE, flags u8 with 1 = key frame, Annex B H.264 or HEVC) and codec changes (12).
- The newest viewer replaces the one before it.
- The same `epoch` answers the same endpoint. A new one replaces it, with a new port and token, and cuts its viewers.
  The endpoint ends when the machine stops.

Each viewer opens `desktop.stream` on the daemon's own socket: QEMU's SPICE console, or on Apple Virtualization the
Linux guest agent or macOS Screen Sharing.

### The sound endpoints

`machine.sound` and `machine.microphone` start loopback TCP listeners with the screen endpoint's rules for the
token, the newest viewer and `epoch`. After the token and a newline:

- A sound viewer reads raw s16le 48 kHz stereo PCM (`channels` 2) for as long as it stays connected. The daemon
  converts the Guest's rate if it differs. A viewer that stops reading for 2 s is cut.
- A microphone viewer writes raw s16le 48 kHz mono PCM (`channels` 1). The daemon sends it to the Guest in 10 ms
  frames.

Both ride one connection per machine to the VM's SPICE console broker, which plays the Guest's sound to the client
that asked last. Apple Virtualization machines, macOS included, answer `unsupported`; core's own in-guest program
carries sound there.

### Containers

A container is a micro-VM booted from an OCI image (a Tart image boots a macOS VM). Rows and records keep the shapes
`veynodevm --json` prints (snake_case fields), which the web UI and the MCP `container_*` tools read; arguments are
camelCase like the rest of the API.

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `container.run` | stream | `{image, command?, entrypoint?, name?, detach?, autoRemove?, ports?, volumes?, env?, workdir?, user?, cpus?, memoryMiB?, diskGiB?, gpus?, desktop?, desktopSize?: [w, h], dns?, mtu?, init?, quiet?, tty?, stdin?}` (the `veynodevm run` flags; `volumes` host paths are absolute) | the row; `progress` events. The client attaches afterwards when not `detach` |
| `container.ls` | call | `{all?}` | rows |
| `container.inspect` | call | `{name}` | `{container, state, status, ip, exit_code, gpu, errors, vm, directory, disk_bytes, …}` |
| `container.start` | call | `{name}` | row |
| `container.stop` | call | `{name, timeoutS?}` | row |
| `container.rm` | call | `{name, force?}` | `{id, name}` |
| `container.rename` | call | `{name, newName}` | row |
| `container.update` | call | `{name, restart?, changes: {cpus?, memoryMiB?, diskGiB?, gpus?, desktop?, desktopSize?, ports?, dns?, mtu?, newMac?}}` | `{id, name, backend, gpus, cpus, memory_mib, disk_bytes, ports, mac_address, …}` |
| `container.icon` | call | `{name, icon?: string \| null}`; without `icon` it only answers | `{id, icon}` |
| `container.logs` | stream | `{name, follow?}` | `{exitCode}`; the output on `stdout`/`stderr` |
| `container.exec` | stream | `{name, argv, env?, user?, workdir?, timeoutMs?, stdin?, tty?, rows?, cols?}`: Linux with `stdin` or `tty`, a guest session (an empty `argv` is a shell) with input on `stdin` and `{"resize":{rows, cols}}` on `control`; Linux otherwise, a bounded guest job (default 30 s) whose output comes at its end; macOS, SSH with `stdin` streamed | `{exitCode, timedOut}` |
| `container.attach` | stream | `{name, rows?, cols?}`: the entrypoint's terminal (`-t`), its stdin and console (`-i`), or its console; a macOS container's background COMMAND output | `{exitCode, errors, left}` once the entrypoint ended, `{left: true}` when the client went first |
| `container.cp` | stream | `{name, direction, hostPath, guestPath}` (macOS) | `{exitCode}` |
| `container.desktop` | call | `{vm, method, arguments}`: a macOS container's desktop or accessibility method | the method's result |
| `checkpoint.create` | call | `{name, checkpoint, memory?}` | the checkpoint report |
| `checkpoint.ls` | call | `{name}` | checkpoints |
| `checkpoint.rm` | call | `{name, checkpoint}` | `{container, removed}` |
| `checkpoint.rollback` | call | `{name, checkpoint, start?, memory?}` | the rollback report |
| `checkpoint.fork` | call | `{name, count?, checkpoint?, start?}` | `{group, forks}` |
| `checkpoint.unfork` | call | `{selector}` (a fork group or a source container) | `{removed}` |

The client keeps only its own terminal (raw mode, Ctrl-P Ctrl-Q, Ctrl-C); the guest session is the daemon's.

### Images

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `image.ls` | call | `{}` | image records |
| `image.inspect` | call | `{reference}` | the record |
| `image.pull` | stream | `{reference}` | the record; `progress` events |
| `image.rm` | call | `{reference}` | `{reference}` |
| `image.load` | stream | `{path, rootfs?}` (a `docker save` archive on the host) | records, with `rootfs` paths when asked |
| `image.gc` | call | `{}` | `{removedBytes}` |

### Shared engine

The engine VM is the daemon's; the docker context and the docker CLI (`run --shared`, `build`) are the client's.

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `engine.status` | call | `{}` | `{state, status, endpoint, port, context, gpu, backend, certs, env: {DOCKER_HOST, DOCKER_TLS_VERIFY, DOCKER_CERT_PATH}}` |
| `engine.start` | stream | `{shares?, gpus?, cpus?, memoryMiB?, diskGiB?, port?, image?, dns?, mtu?}` (applied when the engine is created) | status, once dind's endpoint is up |
| `engine.stop` | call | `{}` | `{}` |
| `engine.rm` | call | `{force?}` | `{}` |

### Desktop and terminal

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `desktop.open` / `desktop.close` | call | `{name}` | `{name}` |
| `desktop.stream` | stream | `{name, codec?: "hevc"\|"h264", tracks?}` | `{exitCode}`; records `u32 BE length, u8 kind, payload` on `stdout` (kind 1 a JSON event, 2 and 10 video access units, 20 audio; see `services/desktop_stream.rs`); the page's messages (`{"events"}`, `{"keyframe"}`, `{"encoder"}`, `{"audio"}`, `{"mic"}`, `{"clipboard…"}`) on `control`. The client leaving stops it and releases held keys |
| `terminal.open` | stream | `{name, rows, cols, argv?}`; `name` is a container or a machine | `{exitCode}`; the same records on `stdout` (kind 1 `ready`/`exit`/`error`, kind 2 output), `{"input"}` and `{"resize"}` on `control` |

### MCP

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `mcp.serve` | stream | `{lifecycle?, execute?, files?, input?, browser?, clipboard?: bool, vm?, tools?: [group]}`, the flags of `vm-control mcp` | `{}` once the session ends; its newline-delimited JSON-RPC on `stdin` and `stdout`. The end of `stdin` ends the session after its answers; the client leaving cancels its jobs and file sessions (`session_end`) |

`veynodevm mcp` relays a stdio client to `mcp.serve`. `vm-mcp-http` (`crates/mcp`) runs one stdio server per HTTPS
session, `vm-control mcp` by default.

### VM layer

The daemon's VM-level methods keep their names and arguments: `list`, `create`, `status`, `start`, `shutdown`,
`force_stop`, `delete`, `capabilities`, `events`, `exec`, `exec_start`, `exec_poll`, `exec_cancel`, the `file_*`,
`screenshot`, `input_*`, `desktop_*`, `accessibility_*`, `snapshot_*`, `driver_rpc` and window methods. The MCP
`vm_*` tools map to them one to one. They answer with codes like every other method once the client says `hello`.
