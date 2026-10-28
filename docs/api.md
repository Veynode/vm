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
{"result":{"api":1,"daemon":"0.4.0","methods":["hello","machine.ls","…"],"home":"/Users/a/Library/Application Support/VeynodeVM","runtime":{"appleVirtualization":true,"qemu":false}}}
```

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
  state: "stopped" | "starting" | "running" | "paused" | "stopping" | "failed";
  source: { disk?: string; from?: string; iso?: string; driversIso?: string };
  cpus: number; memoryMiB: number; diskGiB: number;
  gpu: "virgl" | "venus" | "neptune" | null; macAddress: string | null;
  shares: { hostPath: string; tag: string; readOnly: boolean; guestPath: string | null }[];
  ports: { host: number; guest: number }[];
  createdAt: number; diskUsedBytes: number; icon?: string | null;
  idlePauseS: number; // 0: no idle pause
};
```

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `machine.ls` | call | `{}` | `Machine[]` sorted by name |
| `machine.inspect` | call | `{name}` | `Machine` |
| `machine.create` | stream | `{name, os, cpus, memoryMiB, diskGiB?, shares?, ports?, gpu?, backend?, diskBus?, attach?}` and one source: `{disk}` (a raw Linux disk, cloned), `{from}` (an OCI reference; macOS), `{iso, unattended?, driversIso?}` | `Machine`; `progress` events |
| `machine.start` | stream | `{name, wait?}` | `Machine`; with `wait`, ends once the guest answers |
| `machine.stop` | call | `{name, force?}` | `{name}` |
| `machine.pause` / `machine.unpause` | call | `{name}` | `{name}` |
| `machine.rm` | call | `{name, force?}` | `{name}` |
| `machine.update` | call | `{name, cpus?, memoryMiB?, diskGiB?, ports?, gpu?, newMac?, idlePauseS?}` (stopped, except `idlePauseS` alone; disks only grow) | `{name}` |
| `machine.rename` | call | `{name, newName}` (stopped) | `Machine` |
| `machine.clone` | call | `{name, newName}` (stopped; APFS clone, new identity) | `Machine` |
| `machine.eject` | call | `{name}` | `{name}` |
| `machine.icon` | call | `{name, icon: string \| null}` | `{name}` |
| `machine.ssh_key` | call | `{name, user, key}` | `{name}` |
| `machine.display` | call | `{name, width, height}` | `{name, width, height}` |
| `machine.endpoints` | call | `{name}` | `{address: string\|null, hostAddress: string\|null, ports: {host, guest, address}[]}` |
| `machine.exec` | stream | `{name, argv, user?, env?: ["K=V"], cwd?, stdin: bool, tty?: bool}` | `{exitCode}`; output on `stdout`/`stderr`, input on `stdin`, signals on `control` |

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

### Containers

A container is a micro-VM booted from an OCI image (a Tart image boots a macOS VM). Rows and records keep the shapes
`veynodevm --json` prints (snake_case fields), which the web UI and the MCP `container_*` tools read; arguments are
camelCase like the rest of the API.

| Method | Kind | Arguments | Result |
|---|---|---|---|
| `container.run` | stream | `{image, command?, entrypoint?, name?, detach?, autoRemove?, ports?, volumes?, env?, workdir?, user?, cpus?, memoryMiB?, diskGiB?, gpus?, desktop?, desktopSize?: [w, h], dns?, mtu?, init?, quiet?, tty?, stdin?, idlePauseS?, idleHibernateS?}` (the `veynodevm run` flags; `volumes` host paths are absolute) | the row; `progress` events. The client attaches afterwards when not `detach` |
| `container.ls` | call | `{all?}` | rows |
| `container.inspect` | call | `{name}` | `{container, state, status, ip, exit_code, gpu, errors, vm, directory, disk_bytes, …}` |
| `container.start` | call | `{name}` | row |
| `container.stop` | call | `{name, timeoutS?}` | row |
| `container.pause` / `container.unpause` | call | `{name}` | row (`state` `paused`, `status` `Up (Paused)`) |
| `container.sleep` | call | `{name}` | row; hibernated if it can be, else paused; its next use wakes it |
| `container.keepAwake` | call | `{name, seconds}` (0 ends it early, at most 86400) | row; no idle pause or hibernation until then |
| `container.rm` | call | `{name, force?}` | `{id, name}` |
| `container.rename` | call | `{name, newName}` | row |
| `container.update` | call | `{name, restart?, changes: {cpus?, memoryMiB?, diskGiB?, gpus?, desktop?, desktopSize?, ports?, dns?, mtu?, newMac?, idlePauseS?, idleHibernateS?}}` | `{id, name, backend, gpus, cpus, memory_mib, disk_bytes, ports, mac_address, …}` |
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

### Pause and idle pause

`pause {id}` and `resume {id}` stop and restart a running VM's vCPUs (Apple `VZVirtualMachine.pause`, QEMU
`stop`/`cont`); the Guest's memory stays in use. `start` takes `idle_pause_s` (0, or 60 to 604800), and
`idle_policy {id, idle_pause_s}` changes it for a running VM: after that long without activity the daemon pauses the
VM itself. Activity is any VM method other than `status`, `list`, `events`, `log_read`, `capabilities`,
`control_lease` and the lifecycle methods; an open console window; Guest jobs still running; and `wake {id}`, the
heartbeat long-lived streams send every 20 s. The next such method resumes a VM the daemon paused for idleness
before it runs, and an Apple Linux Guest's clock is stepped to the host's (`clock_sync`). A VM paused with `pause`
refuses other methods (`state`) until `resume`; `shutdown` resumes it first. `status` reports
`idle: {idle_pause_s, idle_s, paused_by: "idle" | "request" | null}` while the VM runs, and `events` carries
`idle_paused` and `resumed`. The `container.*`, `machine.*`, `terminal.open`, `desktop.stream` and `files.copy`
streams hold their VM awake while open, and so does each connection through an Apple container's published ports.

`idle_hibernate_s` (on `start` and `idle_policy`, same bounds; Apple Linux VMs with `machine_identifier_file` and
`mac_address` only) hibernates the VM after that long without activity, whether it runs or the daemon paused it for
idleness: the worker saves the machine state to `hibernate.vzstate` in the VM's disk directory and exits, which
frees the Guest's memory. `status` then reports `hibernated` with `hibernation: {path, bytes}`, and `idle` carries
`idle_hibernate_s`. `start`, `resume`, `wake` and every method that needs the Guest restore it from that file (the
worker holds the request until the VM runs; the clock is stepped); the file is removed once the restored VM runs,
or when its restore fails, and the next start boots afresh. `shutdown`, `force_stop` and `delete` discard it, as
does a start with `restore_machine_state` or a snapshot restore. While the worker saves, methods that need the Guest
wait (up to 300 s) and then restore it. `events` carries `hibernating`, `hibernate_failed`, `restoring`,
`restored` and `hibernation_discarded`. A container's `state` is then `hibernated` and its `status` `Up (Hibernated)`.

Before it pauses or hibernates a running Apple Linux VM, the daemon asks the guest agent's `activity` (one-minute
load from `/proc/loadavg`): at 0.5 or more the Guest is busy, which counts as activity, so work started in the
background is not frozen. A Guest that does not answer counts as quiet. Two methods let a client say what the monitor
cannot see: `sleep {id}` does now what the idle monitor would (hibernates a VM that can be, pauses any other the way
the monitor does, so its next use wakes it; refused while Guest jobs or a snapshot run), and `keep_awake {id,
seconds}` (0 to 86400; 0 ends it) keeps it from both until then and wakes it if it sleeps; `idle.awake_for_s` shows
what is left. MCP has them as `vm_sleep` and `vm_keep_awake` (lifecycle tools).
