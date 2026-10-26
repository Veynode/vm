# Local control and authenticated direct-host MCP

This implementation operates real Apple `vm-apple` subprocesses and an opt-in Linux QEMU/HVF worker. A successful
`start` means the backend accepted startup, not that the guest is healthy.
`guest_info` and `exec` require the actual guest integration service on vsock 5000.
There is no simulated product backend.

## Local usage

```sh
cargo build --bins
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

Run the daemon independently of the MCP client and management window. The MCP
adapter defaults to observe only. `--allow-lifecycle` adds start, shutdown,
force-stop, console visibility and local configuration registration.
`--allow-execute` separately adds commands.
Desktop input is separately enabled with `--allow-input` on QEMU persistent capture, Apple Virtualization Linux desktop containers and macOS Guests; accessibility remains unsupported. `vm_screenshot` requires registered QEMU capture, an Apple Virtualization Linux desktop container (`--desktop`) or a macOS Guest observation service; `vm_screen_wait` and `vm_screen_text` work wherever screenshots do. Independent
`--allow-files` enables bounded transfer tools only when Guest files integration
is explicitly configured. Linux uses a dedicated files account; macOS requires
Guest `--allow-files --files-root /absolute/private/directory` and uses the service
UID. Execute permission does not imply files permission. Query `vm_guest_info`
for the running Guest's `capabilities.files` and `file_policy`; the OS name alone
does not establish availability.

For test input only, the daemon accepts `--worker-key-event-log-dir /absolute/private/logs`.
The directory must already exist, belong to the daemon user, have mode 0700, carry
no extended ACL and not be a symlink; the daemon does not create it, checks it again
at every worker start (before changing any start state) and hands the worker only
its canonical path. Every ancestor must be owned by root or the daemon user and not
group/other writable unless sticky, so that no other account can replace the
directory between the check and the worker's open. Logging is off by default and
never enabled for QEMU. Each Apple worker start receives a new
`key-events-<vm id>-<unix seconds>-<first 8 worker-instance characters>.jsonl` path.
The worker exclusively creates the file with mode 0600. Key codes and modifier
flags can reconstruct typed text: use only test input while this is enabled.
Per-VM `list`/`status` includes `key_event_log: {"enabled":true,"path":"..."}`
while that worker runs and `{"enabled":false}` otherwise. The daemon writes one
startup diagnostic naming the path, without key data. This reports the launch
option, not proof that the worker has opened or written the file.

`vm_create` registers an existing direct-boot Linux configuration or installed
macOS configuration. It does not install the OS or allocate a guest disk.
The registry is atomically replaced and fsynced. Directory permissions are 0700,
socket/registry 0600; flock prevents two daemons owning one registry. This is a
same-user Unix socket trust boundary, not isolation from other processes running
as that macOS user. Local CLI/API can reference that user's files.

Each VM has a separate native subprocess. MCP EOF ends that session's async jobs
and file sessions, not its VMs. Closing the native window preserves the VM. Daemon death closes the
native subprocess control pipe; native shutdown policy applies. Restart does not
silently resume VMs. Concurrent active VMs cannot attach the same writable disk device/inode in one
daemon, including hard-link aliases. The daemon stores the identity at start so
replacing a configured path cannot hide an already attached inode. Worker-level
advisory locks are a separate native backend safeguard.

Normal shutdown supports `vm_shutdown {"id":"...","mode":"backend"}` (default)
and `mode:"guest"`. Backend mode requests the framework's virtual power button;
the guest may ignore it. Guest mode sends the dedicated shutdown operation over
vsock to an installed Linux service started with shutdown explicitly enabled.
It uses lifecycle permission, not command-execution permission.
Both report request acceptance only: poll status for `stopped`. Neither mode
silently escalates to force-stop. The validation initramfs is not an installed OS
shutdown acceptance fixture.

## Assigned-memory admission

The daemon reads physical memory through `hw.memsize` on macOS or `sysconf` on
portable Unix hosts. Its default assigned-VM-memory budget is the smaller of 75%
of physical RAM and physical RAM minus 4 GiB. On a 16 GiB host this is 12 GiB:
the proposed 8 + 8 + 6 GiB allocation is rejected, not silently overcommitted.

Before spawning a worker, the daemon sums RAM assignments for all its still-live
workers, including ones starting or shutting down, with checked arithmetic.
`capabilities.resource_admission` reports physical, budget, active assigned and
remaining MiB. An exited worker releases its reservation. This counts configured
guest RAM, **not measured RSS or available RAM**. Native/backend overhead, other
daemons, installation subprocesses and unrelated host workloads are not counted;
this limit does not guarantee freedom from memory pressure or swap.

The same report states every product-spec control feature per backend and Guest
under `backends.<backend>.control_features.<guest>` (Apple macOS and Linux, QEMU
Linux, macOS and Windows), and each VM's status repeats its own cells under
`capabilities.control_features`, narrowed by its registration (`qemu_guest_exec`,
registered QEMU capture, console and graphics runtime). Each of the 23 cells has
a `status` and a `reason`:

- `supported`: implemented, with an `evidence` path to a real-Guest record
  retained in this tree (`archive/docs-evidence/`);
- `implemented_unverified`: code exists, but no real-Guest record is retained
  here (offline tests at most);
- `requires_guest_permission`: code exists, only the refusal without the Guest
  grant is recorded;
- `external_blocked`: waits on something outside this tree (Windows media);
- `unsupported`: not implemented, or refused for this pair.

Named full-state snapshots are `implemented_unverified` for Apple Linux and
`unsupported` elsewhere. Memory reclaim is `unsupported` on both backends; the
reasons state the 2026-10-12 real-Guest measurement. The desktop window,
accessibility and application tools of Linux desktop containers and macOS
containers are `implemented_unverified`. `product_capabilities` is the older
seven-entry report.

The explicit `snapshot` cell means live snapshot/save/restore, not an offline
disk clone. `balloon` repeats `memory_reclaim`; an attached balloon device alone
does not establish working host memory reclaim. `clipboard` is unsupported:
no clipboard channel is implemented. The existing Windows cells retain their
registration-refusal reasons. QEMU macOS cells now explicitly report unsupported;
this does not enable macOS registration on QEMU.

`application_launch_list` is unsupported: no `vm_app_*` tool exists. On Linux
only, `vm_exec_start` is the documented route for explicit command argv, subject
to `command_execution` requirements; it does not provide an application-list API.
`accessibility_tree` is unsupported on every pair. macOS tree queries are
unimplemented: `vm_windows` always reports `accessibility_tree:false`. Input
separately requires a Guest Accessibility grant and returns
`accessibility_permission_required` on denial. Granting permission does not
enable a tree query. `window_list` repeats `window_metadata`: macOS on Apple
Virtualization uses the Guest observation service, with existing Screen Recording
permission and `--allow-observe`; Linux and Windows return `unsupported`.

`gpu_3d` uses `supported` only for narrow samples with real-Guest evidence:
macOS Metal Forward+ in Godot, or Linux QEMU virgl/OpenGL 2.1 in SolveSpace.
macOS has no accelerated OpenGL, and large Metal compute/Cycles remains unavailable
(decisions 0017 and 0023, detailed under `known_limits` below). Apple Linux,
QEMU macOS and Windows are unsupported. A QEMU VM without a registered runtime
reports unsupported. An opt-in Venus VM additionally needs its registered Venus
runtime and cites the separate raster/vkmark record. These cells describe
evidenced samples, not current per-VM Guest verification, general application
compatibility or performance; `graphics_profile.guest_verified` is unchanged.

`screenshot_scaled_jpeg` describes the explicit `screenshot` request with
`format:"jpeg"`, `max_width`, `max_height` and `quality`. It requires a Linux
Guest on QEMU. It is supported when a console broker and a helper that understands
the form are registered; a helper that does not understand the form returns its
full-size PNG, which is passed through unchanged (the caller must check `mime_type`
and dimensions). Per-VM status marks it unsupported without the broker; Apple
and Windows entries are unsupported. Plain screenshot requests are unchanged.

Known backend limits of macOS Guests are listed under
`backends.apple-virtualization.known_limits.macos`, and each macOS VM's status
repeats them under `capabilities.known_limits`. Linux and Windows Guests have no
such key. Each entry has `code` (stable), `description`, `effect`,
`classification`, `measured_on`, `decision` and `evidence`. Two are stated, both
measured on a macOS 26.6.2 Guest on one host and not on other versions:

- `macos_guest_metal_large_compute_pipelines`: large Metal compute pipelines
  (the Blender Cycles shading kernels) are not created on the Guest's Metal
  device; Cycles runs on the Guest's CPUs (decision 0023). The control plane has
  no render request path, so it raises no error of its own and does no fallback.
- `macos_guest_opengl_not_accelerated`: the Guest has no accelerated OpenGL
  (CGL) renderer; OpenGL applications get Apple Software Renderer (decision 0017).

After a successful macOS `guest_info` reply reports `graphics.status:"unavailable"`
and `reason:"no_metal_device"`, `status` and `list` expose
`runtime.guest_metal_device_alert`. This is the last observed loss for that worker,
not a current-health guarantee: the daemon does not poll the Guest. Only a later
valid schema1 Metal public-device report with the current service's device
identity and query timestamp clears the alert. An older agent without `graphics`
or a malformed/unrecognized successful reply reports `unknown` in the separate
`runtime.guest_metal_device_observation` and retains the last known loss. A failed
query changes neither last-successful observation nor loss alert. Worker state
events retain both records, and a new worker starts without them. The
Guest-authored reply is unchanged. Non-macOS VMs have no such fields.

The alert includes `operator_guidance`: save Guest work, request normal Guest
shutdown, wait for the stopped state, start the Guest and query `vm_guest_info`
again. Normal Guest restarts recovered the device in the retained decision 0023
observations; the loss trigger and live agent reporting during a loss remain
unverified. The daemon does not automatically restart the VM or provide a CPU
fallback. A recovered device report proves device presence only; application
rendering, large compute pipelines and performance still require separate tests.

For an explicit local experiment, pass `--memory-budget-mib MIB` to `daemon`.
The value must be positive and no greater than physical host memory; it may
intentionally reduce the default host reserve. Remote MCP cannot alter it.
The override applies to that daemon invocation and is not persisted in VM configs.

## MCP sessions

Each MCP connection gets an internal random owner token, its session id. The
adapter injects it and it cannot be supplied in MCP tool arguments. There is no
control lease: any caller with input permission and a recent matching screenshot
can send desktop input, and any caller with execute permission can run commands.
When the session ends, the adapter sends `session_end {"owner":...}`, which
cancels that session's unfinished async jobs and file sessions and replies
`{"closed":true,"job_cancellation":{...},"file_cancellation":{...}}`.

## Tool groups

Permissions decide which tools a session may call; tool groups only narrow which
of them it lists, so an agent's context carries the tools its task needs. A group
never adds a tool its permissions withhold. Without `--tools` a session lists
every permitted tool except the opt-in `cua` group.

```sh
./target/debug/vm-control mcp --state-dir /absolute/private/state --allow-input --tools core,screen,input
veynodevm mcp --input --tools core,screen,input
```

| Group | Tools | Permission |
|---|---|---|
| `core` | `vm_list`, `vm_status`, `vm_capabilities`, `vm_guest_info`, `container_*`, `image_*`; with lifecycle also `vm_start`, `vm_shutdown`, `vm_force_stop`, `vm_create`, the console window, `container_run`/`stop`/`rm`, `image_pull` | observe, lifecycle |
| `exec` | `vm_exec`, `vm_exec_start`/`poll`/`cancel`, `container_exec` | execute |
| `files` | `vm_file_*` | files |
| `screen` | `vm_screenshot`, `vm_screen_wait`, `vm_screen_text` | observe |
| `input` | `vm_input_*` | input |
| `desktop` | `vm_desktop_state`, `vm_desktop_apps`, `vm_windows`, `vm_accessibility_tree`/`find`; with input also `vm_window_action`, `vm_app_launch`, `vm_accessibility_action` | observe, input |
| `snapshots` | `vm_snapshot_list`/`status`; with lifecycle also create, restore, delete, reconcile, resume_source | observe, lifecycle |
| `cua` | `cua_*`: cua-driver in Linux desktops, Windows machines and macOS containers ([below](#cua-driver-tools)); hidden unless named, `all` or `vm_tools_enable` | observe, input, browser, clipboard, execute |

`all` names every group. The mapping lives in
[`src/tool_groups.rs`](src/tool_groups.rs), apart from the tool definitions: a new
tool needs a line there or a matching prefix, and the `every_tool_has_a_group`
test fails until it has one. An unmapped tool stays listed.

While a permitted group is hidden, the session also lists `vm_tools_enable`. Its
`groups` argument offers exactly the hidden groups that hold permitted tools, so
an observe-only session is never offered `input`, `exec` or `files`. A call lists
those groups for the rest of the session, replies with the added tool names, then
sends `notifications/tools/list_changed`; `initialize` reports
`tools.listChanged: true` in this case only. Calling a permitted tool of a hidden
group fails with an error that names `vm_tools_enable`.

Size of `tools/list` (compact JSON; tokens estimated at 4 bytes each, not a
tokenizer count):

| Session | Tools | Bytes | ≈ tokens |
|---|---|---|---|
| observe, before this change | 18 | 9,526 | 2,380 |
| observe | 18 | 8,866 | 2,220 |
| observe, `--tools core` | 9 | 2,682 | 670 |
| observe, `--tools core,screen` | 12 | 4,974 | 1,240 |
| lifecycle | 33 | 17,100 | 4,280 |
| execute | 23 | 11,116 | 2,780 |
| files | 24 | 11,037 | 2,760 |
| input | 26 | 18,590 | 4,650 |
| all permissions, before this change | 52 | 32,273 | 8,070 |
| all permissions | 52 | 31,245 | 7,810 |
| all permissions, `--tools core,exec,files` | 30 | 12,838 | 3,210 |
| all permissions, `--tools core,screen,input,desktop` | 35 | 24,094 | 6,020 |

Per group with every permission: core 8.5 KB, input 8.1 KB (the coordinate
schemas and key enum), desktop 6.8 KB, snapshots 4.0 KB, screen 3.1 KB, exec
3.0 KB, files 2.9 KB, each counting `vm_tools_enable`. Tightened descriptions
cut description text from 12,937 to 11,909 bytes (8%) and the `initialize`
instructions from 2,559 to 1,797 bytes (30%), keeping every permission,
confirmation, timeout and does-not-prove statement.

## cua-driver tools

[cua-driver](../../docs/computer-use.md) runs invisibly in the Guest: on Linux
images that ship it as a stdio MCP child of the guest agent (AT-SPI and CDP), on
Windows machines as a hidden logon task the daemon installs on first use, in macOS
containers embedded over SSH. It is reachable two ways, through one bridge
([`src/cua_bridge.rs`](src/cua_bridge.rs)) in the daemon:

- **The `cua` group** on the main MCP: about 24 everyday driver tools as
  `cua_<tool>` (`cua_get_window_state`, `cua_click`, `cua_verify_state`,
  `cua_run_steps`, ...) with short schemas and a `vm` argument (a `vm_id` from
  `container_list`). Hidden by default: `--tools cua` (or `all`) lists it, or
  `vm_tools_enable` adds it mid-session. Arguments beyond the listed ones pass
  through to the driver, which validates them.
- **A per-VM endpoint**: `vm-control mcp --vm ID` (`veynodevm mcp --vm ID`) on
  stdio, `/vm/{id}/mcp` over HTTPS. It serves the driver's own tool names and
  schemas (its `tools/list`, cached per VM and worker), filtered by scope, plus
  that VM's checkpoint and snapshot tools with the VM argument removed
  (`container_checkpoint`, `container_checkpoint_list` and `container_rollback`
  for containers, `vm_snapshot_*`). An agent or skill written for cua-driver
  works unchanged. Every call is bound to that VM; the endpoint has no
  resources, groups or other VM tools. If the driver cannot answer (VM stopped,
  image without cua-driver) it lists only the VM-level tools.

Scopes are checked by the bridge before the driver sees a call (the driver's own
`bounded` profile is a second fence). Unknown tools are refused, so a newer driver
adds nothing until it is mapped:

| Scope | Flag | Driver tools |
|---|---|---|
| observe | always | `list_apps`, `list_windows`, `get_window_state`, `get_accessibility_tree`, `get_desktop_state`, `zoom`, `verify_state`, `get_browser_state`, and reads of the driver's own state and sessions |
| input | `--allow-input` | click, type, key, scroll and drag tools, `set_value`, `invoke_menu`, `set_window_frame`, `bring_to_front`, `run_steps` (each step's tool is checked too), agent cursor settings |
| browser | `--allow-browser` | `browser_*` |
| clipboard | `--allow-clipboard` | `clipboard_read`, `clipboard_write` |
| execute | `--allow-execute` | `run_script`, `launch_app`, `kill_app`, `page`, recordings and replays |
| never | — | `install_extension`, `check_for_update`, `install_ffmpeg`, `set_config` |

Bridge policies (phase 0 findings), applied while no viewer is open on the VM
(a person watching would lose the window to foreground delivery):
- `type_text` with non-ASCII text and no `delivery_mode` goes `foreground`,
  also inside `run_steps`: the background AT-SPI insert truncates multibyte text
  (upstream #4754). Not `set_value`, which replaces the whole value.
- `browser_click` with neither `delivery_mode` nor `input_route` goes
  `foreground`: Chromium's trusted background route is refused on Linux.
- `list_apps` keeps running apps with a window or desktop entry and installed
  apps; the driver otherwise lists every Guest process, kernel threads included.

A policy that changed a call is named in the result's
`_meta["veynode/bridge_policy"]`. Otherwise the driver's result passes through
unchanged: `effect:"unverifiable"`, `unknown` and refusals are never reported as
success. Bridge refusals use the driver's error shape: `isError`, and
`structuredContent.code` `scope_not_granted`, `tool_not_exposed`,
`deadline_exceeds_limit` or `request_too_large`. A failure the guest agent reports (on QEMU a JSON-RPC
error with `data.veynode_code`: `deadline_exceeded`, `driver_exited`,
`unsupported`, ...) keeps that code; on Apple Virtualization it is the call's error.

Deadlines are per call: 30 s by default; `verify_state`, `wait_for` and
`run_script` their `timeout_ms` plus 10 s (never under 30 s); `type_text` and `set_value` 100 ms
more per character; `run_steps` per step 2 s plus its pause, target timeout
(3 s), `wait_for` (5 s) and `expect` timeouts (2 s each), plus 5 s for `observe`
and a 10 s margin. A call that would need more than 10 minutes is refused.
Requests are at most 1 MiB less 4 KiB, replies 1 MiB.

Transport: on Apple Virtualization the worker's `guest_request` sends the guest
agent's `driver_rpc` method over vsock 5000 (`{"frame","deadline_ms"}`, reply
`{"frame"}`; this method alone gets a 1 MiB request and its own deadline up to
10 minutes, others keep 64 KiB and 35 s). On QEMU the worker adds a second
virtio-serial port `veynode.driver` (`<runtime>/driver.sock`) to Linux Guests and
keeps one connection on it for newline-delimited JSON-RPC frames, with its own ids
so replies may arrive in any order, a deadline per call and no fence: a stall
there only delays driver replies, never the serial-v2 exec channel.

Windows machines and macOS containers use a persistent link per VM and worker
([`src/driver_link.rs`](src/driver_link.rs)): one stdio MCP connection that the
daemon owns (handshake, request ids, start_session replayed after a reconnect),
reconnected on demand with at most 5 starts a minute and closed with the worker.
- **Windows** ([`src/windows_driver.rs`](src/windows_driver.rs),
  [`guest/windows/cua.ps1`](../../guest/windows/cua.ps1)): the first connect checks
  `C:\Program Files\VeynodeVM\cua\cua-driver.exe` against the pin, copies it
  over SCP when it differs, writes the telemetry opt-out to the desktop user's
  profile and registers `\VeynodeVM\cua-driver`, a hidden logon task with the
  desktop user's elevated interactive token (`conhost --headless`, no overlay)
  that serves `\\.\pipe\cua-driver`. The link is the driver's own
  `mcp --socket` proxy over one SSH session. A user must be logged on to the
  console.
- **macOS containers**: `veynodevm __driver VM` copies the pinned darwin binary to
  `~/.veynodevm/cua-driver-<sha>` when missing and runs it with
  `CUA_DRIVER_EMBEDDED=1` over SSH.

The pinned binaries come from `runtime/fetch-cua-driver.sh` (registered as
`cua_driver` by `scripts/install-dev-runtime.py`); a missing or altered one is
refused before anything is copied.

A Guest without a driver (no cua-driver in the image, one that will not start)
still answers `cua_get_window_state` on the main MCP: the host reads the screen's
text (`vm-vision`) and returns its lines as rows with tokens `ocr<N>:<row>`,
marked `trusted:false`, `source:"host_ocr"`. `cua_verify_state` on such a token
is `unknown` with reason `untrusted_source`; an action on one is refused with
`next_call` `vm_input_click` at the row's center in the frame it was read from
(`vm_input_batch` takes the token as a click ref).

Over HTTPS a call is still bounded by the transport's 40 s exchange and 45 s
request limit, so long `run_steps` batches need stdio.

## Bounded asynchronous commands

Execute-enabled MCP connections expose `vm_exec_start`, `vm_exec_poll` and
`vm_exec_cancel`. Start takes the same argv as synchronous `vm_exec`. The asynchronous host timeout range is 1–120000 ms; synchronous
`vm_exec` retains its 1–30000 ms range. Both default to 5000 ms. A start above
30000 ms first requires a successful explicit `vm_guest_info` query for the same
running QEMU Linux worker, opted into exec-v2. Its reply must advertise the
no-reconnect, rootless cgroup execution contract and an integer
`max_async_timeout_ms` covering the request. Unknown, older, malformed and
reconnecting Guest policies retain the 30000 ms host limit; Apple/vsock remains
at that limit because its Guest service can restart within the same VM process.
The observed cap belongs to one worker and cannot carry into its replacement.
An unsupported longer start is rejected before reserving a job or sending
`exec_start`; the host does not retry through legacy execution.
A fresh info query clears its prior cap until the reply is validated. Its text result retains the Guest
protocol wrapper:

```json
{"id":"native-request-id","result":{"job_id":"32-lowercase-hex-characters","state":"running"}}
```

Poll returns `result.state` (`running` or `finished`) and, when finished,
`result.result` with exit code, stdout/stderr, truncation flags, timeout and
cancellation flags; internal failures instead appear as `result.error`. A poll
reply does not mean the command succeeded: inspect the terminal result. Cancel
kills the command's process group in the Guest and returns
`result.cancel_requested`; it is not rollback. Poll again to establish the
actual terminal state, including completion races.

The command runs under a Guest-side timeout; the Guest kills its process group
when the timeout expires, when it is cancelled, or when the MCP session that
started it ends. Jobs are keyed by VM id and job id: `exec_poll` and
`exec_cancel` take `{id, job_id}` and fail with "job is unknown or expired on
this VM" for unknown jobs. Synchronous `vm_exec` internally uses
`exec_start`/`exec_poll`/`exec_cancel` and preserves its existing terminal result
wrapper; it requires these Guest capabilities and never silently falls back to
legacy uncancellable execution. Host job metadata is capped at 256 jobs
including in-flight starts, contains no command bodies/output, and expires
330 seconds after registration. At capacity, the daemon evicts the oldest
record known to have finished before rejecting a new start. Completed results
can therefore expire earlier; active jobs and in-flight start reservations are
never evicted for capacity. Cancellation acknowledgements alone do not mark a
job finished. A restarted VM clears its job records. Guest-side retention and
execution limits may reject or expire jobs sooner.

MCP EOF, read/write failures, HTTPS DELETE and session expiry all end the
session through `session_end`, even while a tool response is pending. A
disconnected direct Unix-socket synchronous caller also requests cancellation of
its own job. An individual HTTP connection closing does not end its
authenticated MCP session; use session DELETE or wait for its configured TTL.
Cleanup is best effort; SIGKILL of the MCP adapter cannot run it, so its Guest
commands run until their own timeout (at most 30 seconds for synchronous
commands, 120 seconds for an admitted asynchronous job). Async
completion/output is not recoverable by a newly created connection. No job is
automatically resumed. Prefer explicit cancel and terminal polling when
confirmed termination is required. On QEMU with `qemu_guest_exec`, shutdown
stops new commands and gives running Guest execution a 60-second quiesce
deadline.

## Pushed events and resources

MCP clients that handle notifications need not poll. Both transports offer the
same events; clients without them keep using `vm_status`, `vm_screen_wait` and
`vm_exec_poll`, which are unchanged. `initialize` advertises
`resources: {subscribe: true, listChanged: false}` next to `tools`.

- **Progress.** A `tools/call` with `_meta.progressToken` receives
  `notifications/progress` about once a second (`progress` is elapsed
  milliseconds, no `total`) until its result; none follows the result.
  `vm_exec_poll` takes an optional `wait_ms` (0–30000): the adapter waits for
  the job to leave `running` instead of the client polling, and returns the
  same reply `vm_exec_poll` would.
- **Resources.** `veynode://vm/{id}/status` (the `vm_status` JSON),
  `/screen` (the screenshot as a blob plus its metadata), `/log/serial` and
  `/log/worker` (text) and, with execute permission, `/job/{job_id}` (the
  `vm_exec_poll` reply). `resources/list` lists the first four per VM;
  `resources/templates/list` has all five.
- **Subscriptions.** `resources/subscribe` makes the session send
  `notifications/resources/updated` for that URI, with the event in
  `_meta["veynode.dev/event"]`:

| Resource | Events |
| --- | --- |
| status | `started`; `state` (worker-reported state changed, with `state`); `guest_reset`; `stopped` or `crashed` with `exit_code`, once per worker; `lost` when the session read too late |
| screen | `changed` (at most one a second), then `settled` once still for 750 ms |
| log | `appended` with the new `size` |
| job | `finished` with `exit_code`, `timed_out`, `cancelled` and truncation flags, or `expired`; then the subscription ends |

The Guest returns a command's output only with its exit, so a job reports no
partial output; read the job resource or poll for stdout and stderr. Screen
watching repeats a 5-second `screen_wait`, so a change in the short gap between
two waits can be missed. A session holds at most 32 subscriptions, at most 2 of
them screens.

The daemon side is two non-blocking methods, so no adapter holds one of the
daemon's 32 connections while it waits: `events {after}` reads a sequence-numbered
log of the last 512 lifecycle events (`vm`, `event`, `state`, `reason`,
`exit_code`, `unix_ms`; no host path or pid), and `log_read {id, stream,
max_bytes}` returns at most the last 64 KiB of the VM's `serial_log` or its QEMU
worker's stderr log. The log file must be owner-private and is opened without
following symlinks. Terminal escapes and control characters are removed, runs
of 32 or more token-like characters (hex, base64) with a digit are replaced by
`<redacted>`, and in the worker log every absolute path is replaced by `<path>`.
Serial output is Guest output and keeps Guest paths. The adapter checks for new
events, log growth and finished jobs every 500 ms (logs every second).

Over HTTPS a principal reads and subscribes only to resources of its
allowlisted VMs, `resources/list` is filtered to them, the status resource gets
the same public projection as `vm_status`, and the endpoint forwards only
`resources/updated` notifications for allowlisted VMs (progress reaches only the
request that asked for it). Each session queues at most 256 notifications or
1 MiB for its GET stream; a client that reads too slowly, or has no stream open,
loses the oldest ones and next receives `notifications/veynode/events_dropped`
with the count. One GET stream per session is expected; its 15-second
keepalives count as session activity, the 8-hour absolute limit still applies,
and the stream ends with the session. Screen subscriptions are capped at 8
across all HTTPS sessions.

## APFS base images and writable clones

`vm-image` imports a stopped raw disk into a private read-only base and creates
independently writable raw clones suitable for `VZDiskImageStorageDeviceAttachment`:

```sh
cargo build --bin vm-image
./target/debug/vm-image import --store "$PWD/.local/images-store" --id linux-base --source /absolute/stopped-disk.raw --source-stopped
./target/debug/vm-image clone --store "$PWD/.local/images-store" --base linux-base --id linux-agent-copy
```

Use the returned `disk_path` in a locally registered VM configuration. The clone
operation uses fd-based macOS `fclonefileat`, so source pathname replacement cannot
redirect cloning after the source has been opened. It requires APFS and a source
on the same volume; unsupported/cross-volume cloning fails. **There is no implicit
full-copy fallback.** Bases are 0400, writable clones 0600, manifests 0400 and store
directories 0700. Manifests record logical size, initial SHA256, base identity and
clone mechanism. A writable clone's initial SHA256 describes its creation state;
it is expected to diverge after guest writes.

Before cloning a base, the CLI validates its read-only metadata and SHA256.
Hashing reads the complete logical disk, including sparse zero regions; use a
release build for large images. The sha2 dependency enables its asm feature,
allowing its runtime-checked ARM hardware implementation on supported CPUs.
Known-vector and source/base/clone hash equality tests verify identical digest
semantics; no full-image hashing is skipped to make cloning appear fast.
Directories and files are staged privately, fsynced and published with exclusive
rename; existing ids, symlinks and nonregular source files are rejected. The
original source and base are never rewritten. A failed operation cleans its own
staging directory when possible; a process/power crash may leave a private
`.staging-*` directory, which is not adopted as a valid image. Check final manifests
before retrying after an ambiguous filesystem I/O error.

`--source-stopped` asserts that the caller shut down the source guest cleanly. It
does not bypass lock checks: import takes shared locks on the source inode and the
native worker's global sidecar. Tests confirmed VZ's live exclusive disk lock
rejects the shared import lock. Locking is advisory and cannot make an image
consistent if an unrelated writer ignores locks. This feature is **not live
snapshots, application-consistent backup, VM save/restore or thin-provisioning
capacity reservation**.

Before cloning, free space must meet `--min-free-mib` (64 MiB default). This is a
metadata headroom check, not a reservation for the disk's entire logical size.
APFS initially shares blocks; later writes allocate independent blocks and may
fail with ENOSPC. Logical size and SHA256 are not disk-usage or performance claims.
There is currently no image deletion/garbage collection or automatic base update.

```sh
cargo test --test images
python3 scripts/test-control-images.py --config .local/run/linux-agent.json
```

These tests exercise actual APFS cloning and a real VZ worker with a disposable
blank raw disk, byte independence, active-disk rejection, overwrite protection,
symlink rejection, corrupt-base refusal and configurable space-headroom refusal.
They do not certify an installed guest booting from the clone; that needs a
separate cleanly stopped installed-guest fixture.

## Agent checkpoints, rollback and forks

Agents that are about to try something risky, or want to try several
approaches at once, use the container tools (lifecycle permission; the list is
observe):

| Tool | Does | Confirm |
|---|---|---|
| `container_checkpoint` | APFS clone of the container's writable layer, plus RAM/CPU/device state on Apple Virtualization (`memory`, default true) | — |
| `container_checkpoint_list` | name, time, whether it was running and synced | — |
| `container_rollback` | hard stop, swap in a clone of the checkpoint, restore its machine state or boot again (`memory: false` forces a boot) | container |
| `container_checkpoint_delete` | delete one checkpoint | name |
| `container_fork` | `count` (1–16) new containers from one checkpoint or the live layer | — |
| `container_fork_rm` | stop and delete a fork group, or every fork of a source | selector |

The tools run `veynodevm checkpoint|rollback|fork` (`crates/cli/src/checkpoint.rs`).
A Linux container's only mutable state is `rw.raw`: the image root, boot assets
and configuration disk are read-only. A checkpoint is `clonefile` of that layer
into `<container>/checkpoints/NAME/`, so it costs no data blocks until the
container writes. A running container runs `sync` first; the clone is one
atomic APFS operation, so the copy is crash-consistent (ext4 replays its
journal). A disk-only checkpoint does not capture memory or processes; rolling
back to it cold-boots the micro-VM and the entrypoint runs again. Warm
checkpoints (below) also capture memory on Apple Virtualization.

A fork gets a clone of the chosen layer (the live layer is synced and cloned
once, so all forks of one call start from the same state), a new container
id, hostname and session token in its own configuration disk, a random locally
administered MAC (`mac_address`; QEMU sets it on its virtio-net NIC) and a fresh free
loopback host port for each port the source published. The guest agent records
the owning container id on the layer (`/run/disk/.veynodevm-container-id`);
booting a layer owned by another id removes `/etc/machine-id` and
`/var/lib/dbus/machine-id` from the overlay upper directory, and systemd
containers get a new one (`guest/veynode-agent/layer.go`). With `start`
(default) the forks are booted and the call returns when every entrypoint has
started. Before creating anything, `count × memory_mib` is checked against
`capabilities.resource_admission.remaining_budget_mib`; a later per-start
admission failure removes the forks the call created. Results name a `group`
for `container_fork_rm`, and each fork carries `fork_of` in `container_list`.

Measured 2026-10-26 on an M3 Pro (macOS 27), live micro-VMs in a disposable
`VEYNODEVM_HOME`, alpine with a 16 GiB sparse layer, 512 MiB (Apple) or 2 GiB
(QEMU) RAM; three runs each:

| Step | Apple Virtualization | QEMU/HVF `--gpus virgl` |
|---|---|---|
| checkpoint of a running container (incl. guest `sync`) | 57–82 ms | 105–111 ms |
| rollback: hard stop / swap / boot to entrypoint | 32–127 / 0 / 768–1975 ms | 3 / 0–1 / 1249–1490 ms |
| fork ×3 from a checkpoint, all entrypoints started | 1086 ms (35–52 ms per fork to create) | 2460 ms (24–25 ms per fork) |
| fork ×3 from the live layer | 775 ms | 3147 ms |
| disk | logical 16 GiB per clone; free space moved by 0–11 MiB per checkpoint or fork group (noisy: other sessions share the volume) | same |

Each fork reported its own hostname and MAC from inside the Guest, served its
own HTTP response on its remapped host port, kept the checkpoint's files and
dropped the source's `/etc/machine-id`; writes in one fork were not visible in
another or in the source. A 16 × 2 GiB fork was refused by the memory check
before creating anything. Unit tests cover names, config derivation, port
remapping, fork selection, tool scopes and confirmations, and the agent's
layer adoption; they do not boot a Guest.

### Warm checkpoints and rollback (Apple Virtualization)

On Apple Virtualization a checkpoint of a running container also saves its
machine state (`veynodevm checkpoint create` without `--disk-only`, macOS 14+).
The daemon's `machine_state_save` worker method (`container_state.rs`,
`native/worker/ContainerState.swift`) pauses the VM, writes
`checkpoints/NAME/machine.vzstate` with `saveMachineStateTo`, `fsync`s and
`fclonefileat`s the writable layer from the worker's held descriptor while
still paused, and resumes: disk and memory come from one paused interval. Both
files are mode 0400 in a 0700 staging directory, and the paths must stay
inside the container directory. The guest console log is copied alongside.

Restore needs the same configuration, so containers created since this change
get a persistent `vz-machine-id.bin` (`machine_identifier_file`, a
`VZGenericMachineIdentifier` created 0600 on first start) and a fixed random
`mac_address`. Extra disks, the configuration disk, virtiofs shares and a new
serial log path do not block restore. Rollback (`container_rollback`, or
`veynodevm rollback` without `--cold`) hard-stops the VM, swaps in a clone of
the checkpoint's layer and starts a fresh worker with
`restore_machine_state` (`--restore-machine-state PATH`), which restores and
resumes it, emitting `machine_state_restored`. The CLI then sends
`clock_sync` (the guest agent steps `CLOCK_REALTIME` to the host time; the
monotonic clock continues from the checkpoint). Processes, tmpfs contents and
the published ports carry on from the checkpoint. If the restore fails, the
reply carries `warm_error`, and rollback re-clones the layer and cold-boots;
`path` says `warm` or `cold`. An agent older than `clock_sync` leaves the
clock behind and reports the error in `restore.clock_sync`. Deleting a
checkpoint deletes its state file with it.

Forks always cold-boot (`path: "cold"`). VZ restores a state file only into a
VM with the source's MAC address and machine identifier: a new MAC or a new
identifier failed restore with "invalid argument" in live tests, and changing
the MAC inside the restored guest leaves it without a DHCP lease, because
vmnet filters by the configured MAC. A warm fork would therefore be a second
VM on the network with the source's MAC. QEMU containers keep disk-only
checkpoints: virgl and Venus install a migration blocker, so the reply names
`warm_unavailable`. So do containers created before machine identifiers.

Measured 2026-10-26 on an M3 Pro (macOS 27), live alpine micro-VMs in a
disposable `VEYNODEVM_HOME`, worker built from this tree, three runs each:

| Step | 512 MiB guest | 2 GiB guest, 1 GiB tmpfs filled |
|---|---|---|
| warm checkpoint (VM paused) | 228–258 ms (paused 211–252 ms) | 1290–1342 ms |
| state file | 34 MiB | 1088 MiB |
| warm rollback, total (restore to running) | 409–566 ms (363–513 ms) | 1578–1768 ms |
| cold rollback, total (boot to entrypoint) | 864–916 ms (830–881 ms) | 697–791 ms |
| fork ×3 from a warm checkpoint (cold) | 1041 ms | 858 ms |
| checkpoint of a stopped container (disk only) | 10 ms | — |

The state file grows with the memory the guest has touched, and saving and
restoring it costs about 1.2 s per GiB here: a small, idle guest rolls back
about twice as fast warm, a guest with a gigabyte in use rolls back faster
cold, but warm keeps its processes. After each warm rollback a counter
process resumed from its checkpoint value, HTTP through the published port
answered, and the clock was stepped by the time spent since the checkpoint
(2.3–9.5 s). A `-v` virtiofs share stayed mounted. Through MCP, warm rollback
took 335 ms and `memory: false` 715 ms. Unit tests cover path containment,
eligibility, fork identifiers and the agent's clock request validation; they
do not boot a guest.

Live full-state snapshots of whole VMs remain the separate `vm_snapshot_*`
tools: they save RAM/CPU/device state for single-disk Apple Linux VMs with a
persistent `machine_identity_dir` (`implemented_unverified`) and keep named,
transactional snapshots in the VM's state. QEMU's paused-migration helper
(`qemu_snapshot.rs`) is not wired in and refuses virgl/Venus because QEMU
installs a migration blocker for accelerated virtio-gpu.

## HTTPS endpoint

`vm-mcp-http` exposes `/mcp` using rustls with TLS terminating on the user's Mac.
It defaults to `127.0.0.1:8443`; wider binding requires an explicit bind address.
No public deployment or firewall changes are made by this repository.

The transport supports MCP version `2025-06-18`, JSON POST responses, notification
202 responses, DELETE, and server-sent events (see [Pushed events and
resources](#pushed-events-and-resources)): a POST carrying `_meta.progressToken`
is answered as an SSE stream of its progress notifications followed by its
response, and GET (Accept `text/event-stream`) opens the session's notification
stream. There is no resumption (`Last-Event-ID` is ignored).
It uses provisioned bearer tokens, **not OAuth discovery or dynamic registration**;
clients must support explicit authorization headers. This is not a claim of
complete MCP OAuth authorization-profile support.

Example private JSON configuration (replace placeholders):

```json
{
  "bind": "127.0.0.1:8443",
  "tls_cert": "/absolute/private/fullchain.pem",
  "tls_key": "/absolute/private/key.pem",
  "allowed_origins": ["https://your-client.example"],
  "session_idle_seconds": 900,
  "principals": [
    {
      "name": "trusted-agent-device",
      "token_sha256": "64-lowercase-hex-characters-of-SHA256-of-a-random-bearer-token",
      "scopes": ["observe", "execute"],
      "vm_ids": ["linux-agent"],
      "tools": ["core", "exec"]
    }
  ]
}
```

Config and TLS key must be regular, owner-owned files with no group/other access
(0600). Tokens should contain at least 256 random bits. Only their SHA256 digest
belongs in this host config; provision raw tokens to the authorized client through
a trusted private channel, never a URL/query parameter. TLS clients must verify
hostname and certificate chain. Revocation currently means removing the principal
or changing its digest and restarting the HTTPS service; no hot reload is promised.
No token, operation body or guest output is written to service logs.

```sh
./target/debug/vm-mcp-http --config /absolute/private/https.json --state-dir /absolute/private/state
```

Every remote principal has an explicit VM-id allowlist (omission grants none).
Scopes are `observe` (required), `lifecycle`, `execute`, `files`, `input`,
`browser` and `clipboard`; the last two grant only the matching
[cua-driver tools](#cua-driver-tools), whose `vm` argument must be allowlisted.
`/vm/{id}/mcp` opens a per-VM cua-driver session: the path's VM must be in the
allowlist, the session's child serves only that VM, and the session answers on
that path only.
Optional `tools` lists the [tool groups](#tool-groups) its sessions see (omission
lists every permitted tool); it grants nothing beyond `scopes`. `vm_tools_enable`
needs no VM id. JSON POST replies carry no server notifications, so this
transport answers `initialize` with `tools.listChanged: false` and drops the
`list_changed` after `vm_tools_enable`; call `tools/list` again after it.
Remote `vm_create` is blocked even with lifecycle scope: host disk paths, shared
directories, restore images and kernel command lines must be configured locally.
List/status/start responses hide host paths, kernel arguments and process IDs.
Observe still grants guest information and authorized VM resource metadata. Execute
explicitly grants command access inside the allowlisted guest; guest-visible
shared files remain within that command-access boundary. Independent `files` scope grants the bounded file-transfer tools; `execute` and
`lifecycle` do not. Each token is bound to this MCP session owner and VM. These
file tools access only the explicitly configured Linux or macOS Guest files
directory, not arbitrary Guest paths. macOS uses the service UID; its private
directory is not isolated from other programs running under that same UID.

The endpoint checks allowed browser Origin, MIME types, Accept and negotiated
version. Sessions are random, principal-bound, limited to 32 total / 4 per principal,
expire after configured idle time (max 3600 seconds) or 8 hours absolute, and close
on DELETE. Requests are capped at 1 MiB, 64 concurrently before reading the body,
and 45 seconds. Same-session concurrent operations return 409 instead of queueing
stale operations. TLS connection-level volumetric denial-of-service protection,
OAuth discovery, per-request consent UI and unattended credential rotation remain
release work.

Direct verified HTTPS protects sensitive MCP traffic between the client and the
Mac. It says nothing about remote desktop encryption, browser code authenticity,
WebRTC signaling binding, hosted TLS termination or cloud relay E2EE: those
separate product paths are not implemented here. Do not terminate this TLS in a
hosted reverse proxy and describe the resulting connection as end-to-end encrypted.

## Reproducible checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
python3 scripts/test-control.py --config .local/run/linux-agent.json --require-guest
python3 scripts/test-control-resources.py --config .local/run/linux-agent.json
python3 scripts/test-control-http.py --config .local/run/linux-agent.json --node-sdk
```

The HTTPS test creates a temporary certificate and explicitly trusts it in test
clients; it also asserts that an untrusted client rejects it. It never disables
certificate or hostname checks. `--node-sdk` requires the locked official SDK in
`tests/mcp-client` (`npm ci --prefix tests/mcp-client`). Optional independent Python
SDK verification:

```sh
python3 -m venv .local/mcp-test-venv
.local/mcp-test-venv/bin/pip install mcp==2.3.0
.local/mcp-test-venv/bin/python scripts/test-control-http.py --config .local/run/linux-agent.json --sdk --node-sdk
```

The tests use disposable fixture VMs, perform actual ARM guest commands and stop
their VMs on cleanup. Evidence is under `.local/evidence/`. HTTPS failures save a
separate `.failed.json` without replacing the last passing result. These checks
are integration evidence, not performance, graphics, internet reachability or
public-release certification.

## Product licensing

The product source license has not been selected. Package metadata intentionally
does not declare a license grant; a licensing decision and dependency license
review remain required before public distribution.

## Scoped file transfer

Local stdio adds file tools only with `--allow-files`; verified HTTPS principals
must explicitly include `files` alongside `observe`. Neither execute nor lifecycle
implies files. The native worker transports these requests to an explicitly enabled
Linux or macOS Guest files service. Disabled or unavailable Guest integration
returns an error; the controller does not infer availability from the OS. QEMU
Guest integration remains unsupported. There is no host file read or
command-execution fallback.

The six tools are `vm_file_upload_begin`, `vm_file_upload_write`,
`vm_file_upload_commit`, `vm_file_download_begin`, `vm_file_download_read`, and
`vm_file_cancel`. Consult their MCP input schemas. Every call includes the VM id;
the adapter injects its random session owner, which the client cannot override.
Tokens are VM/session-bound. No execute permission is
needed for files. Chunk size is32KiB, maximum transfer8MiB, with sequential offsets,
SHA256, no-overwrite publication and bounded lifetime. Closing the MCP session
requests file cleanup; Guest deadlines cover unconfirmed cleanup or abrupt host
failure. Sensitive file bodies are never logged.

This is single-filename transfer into an explicit Guest directory. Linux uses a
separate files account; the ordinary execute account cannot use those uploaded
files. macOS requires a fixed owner-private directory and a dedicated worker
running under the Guest service UID. That is **not isolation from other programs
with the same UID**. The files permission does not authorize command execution,
and MCP macOS command execution remains unsupported. Neither backend grants
directory traversal, arbitrary Guest file access, overwrite or directory creation.
The controller routing tests use pipe fixtures and do not establish actual macOS
Guest transfer acceptance. See [Linux Guest boundary](../../guest/veynode-agent/README.md).

Windows machines (QEMU/HVF, `guest: windows`) have no guest agent; the daemon
moves files over the Guest's OpenSSH Server instead (see
[Windows machines over SSH](#windows-machines-over-ssh)). Writes and reads
are staged on the host (`<state>/windows-files`, 0600) and cross SSH once, at
`vm_file_upload_commit` and `vm_file_download_begin`; the other calls never
touch the Guest. The directory is `%USERPROFILE%\veynodevm-files` of the SSH
user, which has no separate account: anything that user runs can read it.
Names are single Windows file names (no device names, separators, wildcards
or trailing dots); publication checks SHA256 in the Guest and never
overwrites.


## Opt-in QEMU/HVF console backend

Apple Virtualization remains the default. A VM may specify `"backend":"qemu-hvf"`
only after the local daemon operator registers a trusted runtime. Runtime executable,
framework and helper paths never come from a VM configuration or remote MCP call.
A local diagnostic registration using the already built SPICE reconnect fix is:

```sh
./target/debug/vm-control daemon --state-dir /tmp/veynode-control \
  --backend "$PWD/.local/bin/vm-apple" \
  --qemu-worker "$PWD/target/debug/vm-qemu" \
  --qemu-launcher "$PWD/.local/spice-reconnect-build/runtime/probe-loader" \
  --qemu-library "$PWD/.local/runtime/utm/UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu" \
  --qemu-resources "$PWD/.local/runtime/utm/UTM.app/Contents/Resources/qemu" \
  --qemu-spice-library "$PWD/.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib" \
  --qemu-screenshot-helper "$PWD/.local/bin/spice-screenshot"
```

This revision accepts Linux ARM RAM direct boot (`kernel`, optional `initrd` and
`command_line`) and runs EDK2 from the trusted resources directory so supported
Guests receive ACPI. It explicitly rejects writable disks, host shares, macOS,
Windows, native windows, display-size overrides and Guest integration on this
backend. Those capabilities are unfinished; no compatibility fallback is hidden.
The existing Apple disk and integration paths remain available. QEMU uses HVF,
virtio-gpu-gl, virtio keyboard/tablet, and private Unix SPICE/QMP sockets. Physical
input devices support the separately scoped, frame-bound physical MCP input path described below.

`start` returns before Guest readiness. Status distinguishes process failure,
QMP running/paused/unavailable, and `guest_ready:null`. QMP is sampled at most once
per second, suspended during capture. A SPICE socket or QMP running status alone
proves no graphics API or Guest health. The `shutdown` result says only that the
power-button request was sent; callers must poll. The real test fixture installs
ACPI button/evdev modules and runs BusyBox acpid, `/sbin/poweroff` without `-f`, and
PID1 shutdown actions. A Guest without these facilities may keep running.

Every worker receives a private process group. On worker failure the daemon uses
`waitid(WNOWAIT)` to retain the worker PID until its group is killed, then reaps it;
cleanup never kills an already reaped, possibly reused PGID. A daemon monitor
checks failures independently of client calls. EOF/SIGTERM tells the worker to
quit QEMU with bounded cleanup. Force-stop and supervisor-loss termination may
lose Guest data; they are not normal Guest shutdown. Automatic VM restart is not
implemented. A SIGKILLed worker can leave its private runtime directory and
socket names; it does not leave an executing QEMU child in the verified path.

Daemon stderr logging uses a non-blocking 64-line queue. Existing successful and
error returns from the daemon's serve function allow up to 250 ms to drain it.
On SIGTERM, queued lines not yet written may be lost; at most the 64-line queue.

Guest serial logging is disabled by default. Explicit `serial_log` enables an
owner-private append-only diagnostic file, bounded to 8MiB. Backend stderr is
retained in a private per-run file, also bounded to 8MiB. Disabled/full/failed log
outputs are drained. Screenshot PNG data is not persistently stored by the
controller. The explicit test script saves diagnostics and screenshots.

`vm_screenshot` is observation-only and returns MCP image content plus UUID,
timestamps, physical Guest pixel dimensions and unknown Guest UI scale. Internal
SPICE scanout IDs are removed. One asynchronous capture per VM is allowed; another
returns busy. Screenshot responses are <=12MiB, PNG <=8MiB (decision 0024; 3MiB and 2MiB before 2026-10-11), worker deadline 15s, control
wait bound 17s. Shutdown/stop can be processed while capture is pending. Capture
currently starts a short-lived SPICE consumer: a completely static desktop that
sends no new draw can fail explicitly. Use the persistent console broker below to
keep the capture connection from VM startup. Input needs separate input permission plus a recent frame observation token; a screenshot alone grants no input permission.
The larger response bound applies only to an outstanding screenshot request;
ordinary replies and all incoming requests remain <=1MiB. Persistent native/broker
records remain <=4MiB. The full-resolution lossless PNG is never resized to fit;
images above 2MiB fail explicitly, so not every 1440p/4K scene is guaranteed to fit.
Screenshot JSON avoids optional slash escaping to keep worst-case base64 within
its response budget. The macOS Guest JPEG producer and 700KiB validation remain
unchanged. Fixed native error diagnostics report the stage and encoded byte count,
never pixels or file paths.

The MCP `vm_screenshot` also takes `max_width`, `max_height` (16–8192), `region`
(`{x, y, width, height}` in screenshot pixels), `format` (`jpeg`, `png`) and
`quality` (1–100, default 80) on every backend. The full capture still crosses
the daemon; the stdio adapter crops, scales down (never up, one factor for both
axes) and re-encodes it with `vm-vision image` (ImageIO) and refuses a result
above 700 KiB. Scaling defaults to JPEG, a crop alone keeps the capture's format.
`width`, `height`, `frame_id` and `source_generation` stay those of the full
frame, for input; `image_width`, `image_height`, `image_scale` (image pixels per
source pixel) and `image_region` describe the image, and the source digest moves
to `source_sha256`. A point in the image maps back as
`x = image_region.x + image_x / image_scale` (y likewise). For a whole-screen
image the reply adds `image_point_pixel_scale` (`1 / image_scale`, when at most
8): pass it as `point_pixel_scale` with `"coordinate_space":"points"` and click
image coordinates directly. Clients such as Claude downscale images over about
1568 px themselves, so without `max_width` the pixels a model sees are not the
ones `width`/`height` describe; `"max_width":1568` makes them agree. The QEMU
broker's own scaled-JPEG form (decision 0025) is not used by this path.

Reproduce the actual RAM-only test (requires the pinned media and local runtimes):

```sh
cargo build -p vm-control --bins
python3 scripts/test-control-qemu.py --output .local/evidence/control-qemu-new.json
```

The 1536MiB QEMU fixture and 512MiB VZ isolation fixture never attach a disk. The
stock graphics stack in this test retains known 2D/text rendering defects. The
controller test is not pixel correctness, Windows GPU, native-window, performance,
release packaging or licensing acceptance. See `archive/docs-evidence/control-qemu.md`.


### Persistent console registration

Register `--qemu-console-broker "$PWD/target/debug/vm-console"` and
`--qemu-console-helper "$PWD/.local/bin/VeynodeQEMUConsole.app/Contents/MacOS/spice-console"`
(built by `runtime/build-qemu-console.sh`; the headless `.local/bin/spice-console`
also works but has no native window) together to enable the
persistent capture owner. These are trusted daemon options. The worker starts
QEMU paused (`-S`), waits up to12s for the broker's verified capture connection,
then sends QMP `cont`. Connection readiness does not assert a Guest frame exists.
Broker registration takes priority over the optional short-lived screenshot
helper. Broker failure never silently switches back to a cold SPICE consumer.

A screenshot uses its own private Unix connection, matching request identity and
strictly accepting a bounded JSON record. It cannot start/stop encoding or acquire
an encoding epoch. The control side enforces an absolute15s read deadline with
nonblocking I/O and poll, retains actual source capture timestamps and sequence,
and creates a new frame UUID for the observation result. Snapshot requests that
observe the same static frame can have different UUIDs and identical source
sequence/timestamps. Input binding is still not implemented.

On worker stop, the broker receives SIGTERM and up to3s to reap its native capture
child before QEMU quit. Unexpected broker exit leaves the Guest running but
screenshots fail explicitly; restoring capture currently requires VM restart.
The real boot-from-paused integration exposed and fixed native intermediate scanout
transitions, a server ESTALE publication race, and a Darwin zombie-only process-group
cleanup edge. Static MCP screenshots across new clients and the browser-adapter
lifetime now pass. Separate actual Chromium154 reports now verify HEVC and H264 decoding and
confirmed disconnect from this static source; those reports are independent of
the controller pass. Internet operation is not validated. See `archive/docs-evidence/control-qemu-persistent.md`.

The explicit static integration command is:

```sh
python3 scripts/test-control-qemu.py --persistent-console --static-desktop \
  --output .local/evidence/control-qemu-persistent-new.json
```

For the registered persistent path, replace the optional
`--qemu-screenshot-helper` flag in the command above with both console flags.
`--extra-console-consumer /absolute/executable` is an explicit test-only hook: the
script receives the private console socket and output prefix, is limited to180s,
and is followed by another MCP screenshot and lifecycle checks. A zero exit status
from the hook does not certify browser decoding; save separate browser evidence.

### Frame-bound physical desktop input

Start the MCP adapter with **`--allow-input`** or give a HTTPS principal the
independent **`input`** scope alongside `observe` and an explicit VM allowlist.
`execute`, `files` and `lifecycle` do not imply input permission. Take
`vm_screenshot`, then copy its `frame_id`,
`source_generation`, `width` and `height` into the input call alongside `id`:

```json
{"id":"linux-console","frame_id":"<screenshot UUID>","source_generation":12,"width":1280,"height":800,"x":200,"y":200}
```

- `vm_input_click`: `x`, `y`, optional `button` (`left`, `middle`, `right`).
- `vm_input_drag`: start `x`,`y`, destination `end_x`,`end_y`, optional button;
  20 bounded intermediate moves, no user-controlled indefinite hold.
- `vm_input_scroll`: `x`,`y`, `direction` (`up`,`down`,`left`,`right`), 1–20 discrete `ticks`.
- `vm_input_key`: 1–8 explicit `keys` in the advertised QEMU qcode enum. These are
  physical key positions/chords interpreted by the Guest keyboard layout, **not**
  Unicode text injection (see `vm_input_text`). Presses are followed by releases, 20 ms later by default;
  optional `hold_ms` (0–900) holds them longer, for games that steer or accelerate
  only while a key is down. Longer holds are repeated calls.
- `vm_input_text`: `text`, 1–256 Unicode scalar values and at most 1024 UTF-8 bytes;
  U+000A (Return) and U+0009 (Tab) are the only control characters, noncharacters
  are refused, nothing is normalized. Supported only for a Linux Guest on QEMU
  registered with `qemu_guest_exec`, whose Guest service runs on the execution
  transport with `--allow-text-input` in an X11 session (XTEST, at least 8 spare
  keycodes; no Wayland). Every other VM is refused with `unsupported: Unicode text
  input requires ...` before any console check; a Guest without the service is
  refused with `unsupported` after one `guest_info` query. The text does not
  travel as key codes: the daemon splits it into chunks of 16 scalar values and
  the worker sends each as a `text_input` request (hex of the UTF-8 bytes)
  under the same operation and recent-frame fences as `input_step`;
  the Guest binds temporary keycodes, reads the mapping back and sends XTEST key
  events (`guest/veynode-agent/README.md`). The first failure ends the call and
  states how many characters were delivered and how many are unconfirmed;
  nothing is retried or dropped silently. A chunk may take up to 4 s when the
  Guest has to wait before rebinding a keycode. The daemon, worker and Guest
  service do not log or persist the text, and replies carry counts only; the
  caller's own process arguments and MCP client logs are outside this code.
  Success means the Guest X server acknowledged the key events, not that the
  application accepted them.

Click, drag and scroll accept optional `coordinate_space`: `"source_pixels"`
(the default, preserving existing integer pixel requests) or `"points"`.
Points require a caller-supplied finite `point_pixel_scale` greater than 0 and
at most 8; supplying a scale with source pixels, omitting the scale for points,
or using an unknown space is a typed error (`invalid_point_pixel_scale` or
`invalid_coordinate_space`). Point coordinates may be fractional and must be
finite and nonnegative (`invalid_point_coordinate` otherwise). Before the existing
frame bounds, generation, expiry and identity checks, the control plane multiplies
each position by the scale and rounds to the nearest pixel, with ties away from
zero. Converted pixels outside the frame are refused without clamping. Both drag
endpoints and scroll positions are converted; scroll `ticks` remain discrete
wheel-button press/release pairs in the wire's own units and are never scaled.
The conversion is shared by QEMU Linux and opted-in macOS Guest input.

For example, `"coordinate_space":"points","point_pixel_scale":2.0,"x":200,"y":150`
on a 2560×1440 frame converts to source pixel (400,300), normalized (5126,6838).
Keep `width` and `height` in source pixels. The scale is caller knowledge (for
example from `vm_windows` or the display); it is never inferred from or added to
the screenshot binding. macOS screenshots already report `point_pixel_scale`
and `guest_ui_scale`, but input does not depend on those metadata fields.
Points are local to the captured display's upper-left origin: `vm_windows`
reports global desktop points, so callers must first subtract the selected
display's global origin. This option supplies no display selection, rotation
or letterbox transform.

On QEMU, input requires the persistent console broker and a screenshot observation younger
than 5 seconds, or older when the console has drawn nothing since: the worker
records the capture's `source_sequence` with the frame and accepts an aged frame
whose sequence still equals the current `frame_status.source_sequence` (any draw,
a blinking caret included, ends that). Freshness is checked when an operation
begins; its later steps keep the generation and size checks but not the age, so a
click's release is not refused because the press redrew the button. Each worker keeps at most 16 observation metadata records; restarting
the VM discards them. Static source capture timestamps may be old: expiry uses the
host's new screenshot observation time without falsifying the original capture time.
Every step queries the native capture owner's current generation/dimensions through `frame_status`; unavailable,
changed, foreign and expired frames fail closed. The currently validated mapping is
single-display Linux Xorg/libinput, physical pixels and default calibration.

Capture validation and QMP injection occur in separate processes and cannot be
atomic; a resize can race an **already dispatched** step. Failed/interrupted operations release only
buttons/keys belonging to their opaque operation token. Concurrent operations are
rejected while one is active. A worker safety deadline of 4 seconds is checked
between bounded RPCs; failed QMP cleanup is reported and, on safety-deadline retry
failure, terminates the worker/VM rather than declaring release success. This does
not undo clicks already delivered. QMP acknowledgements mean dispatch, not proof
that an application accepted input. Guest evdev/Xorg acceptance is tested separately.

Actual stdio disconnect during a drag releases the button and prevents completing
the drag. Internet browser input,
accessibility, keyboard layout translation for `vm_input_key`, Unicode text outside
the `vm_input_text` case above (Apple Virtualization, macOS, Wayland, QEMU without
`qemu_guest_exec`), Retina calibration, multiple displays,
macOS input and Windows input are still unsupported.
The host path of `vm_input_text` (daemon, worker fence, serial transport) is covered
by offline tests and the Rust-to-Go interop test.

#### Act and observe (`observe`)

Through MCP, every input tool and `vm_accessibility_action` take an optional
`observe` object (`crates/control/src/agent_view.rs`). After dispatch the adapter
waits as `vm_screen_wait` does from the frame the input was bound to (for an
accessibility action, the last frame the session saw, else one taken before the
action), with `settle_ms` (default 300) and `timeout_ms` (default 2000; 0 skips
the wait), then takes a fresh screenshot and returns its anchor as `frame`
(`frame_id`, `width`, `height`, `source_generation`) plus `observed` (`changed`,
`settled`, `elapsed_ms`). The next input can use that frame without a separate
screenshot. `return` chooses what else comes back:

| `return` | Reply |
| --- | --- |
| `delta` (default) | accessibility elements added (`+`), changed (`~`) or removed (`-`) since the last tree this session read, as compact lines by ref; the first time, the whole tree. Without a tree (QEMU, Windows Guests) the changed regions between the bound frame's image and the new one (`vm-vision diff`, 16 px cells, at most 16 regions) and the text recognized in them |
| `compact_tree` | the whole compact tree, with the arguments of the session's last `vm_accessibility_tree` (default `interactive_only`) |
| `text` | the screen's text lines, `"text" @x,y` |
| `screenshot` | the image, with `max_width`, `max_height`, `format`, `quality` as for `vm_screenshot` |
| `none` | only the frame |

The action has already happened when an observation fails, so a failure is
reported in `observed.error` and the call is not an error. The input reply drops
its dispatch note; `guest_delivery_confirmed` stays false, and the observation is
the evidence of acceptance.

Every input reply carries `action`, an `ActionResult` in cua-driver's shape
([`src/action_result.rs`](src/action_result.rs)) with `route:"virtual_hid"`, so
host input and driver input read the same. `effect` comes from what the observation
saw and never from the dispatch: `confirmed` only with evidence (a changed region
that contains the click, scroll or drag-end point, or for keys and text any
change), `unverifiable` when the screen changed elsewhere or the change cannot be
placed (QEMU reports no regions), `suspected_noop` when it settled unchanged, and
`unverifiable` without `observe`. Each unconfirmed result names a `next_call`
(`vm_screenshot`).

**Control lease** ([`src/control_lease.rs`](src/control_lease.rs)). A person's
input always wins. While a console window is open on an Apple Virtualization VM
and its person typed, clicked or scrolled in the last 5 s, agent input
(`vm_input_*`, window and accessibility actions, `cua_*` input and browser tools,
on either MCP face) is refused before anything is sent: host tools fail with
`person_in_control: <ms> ms ...`, which the adapter returns as a refused
`ActionResult` with `escalation {target:"lease", reason:"person_in_control"}` and
`next_call` `vm_control_lease`; driver calls get the driver's error shape with code
`person_in_control`. Closing the window hands control back at once. While a
window is open the bridge keeps to background delivery. `vm_control_lease`
(input permission) reports `holder` (`person`, `agent`, `none`), `viewers` and
`agent_may_act_in_ms`; `release` gives the agent's lease back. Changes are
`control` events (`state` `person` or `agent`) on the VM's subscribed status
resource. The worker
reports the window and the person's input (`window_opened`, `window_closed`,
`person_input` at most once a second); the QEMU console and browser desktops do
not report yet, so they never pause the agent.

The adapter also re-anchors frames on Apple Virtualization Linux desktops and
macOS Guests, whose services keep the 5 s window: input bound to a frame this
session saw more than 4 s ago first takes a new screenshot and, when its SHA-256,
geometry and generation equal the old frame's (the screen has not changed),
sends the input with the new `frame_id` and reports `reanchored`
(`from`, `to`). A different digest refuses the input with `stale_frame` before
anything is sent. QEMU frames are left to the worker's sequence check above, and
frames this session never saw are passed through to the backend's own check.

#### Batched input (`vm_input_batch`)

`vm_input_batch` (input permission, MCP only) runs 1–50 `steps` in order and
stops at the first failure. Each step has a `type`: `click` and `scroll` (at `x`,
`y` or at the box center of a `ref` this session read), `drag`, `key` and `text`
take the fields of the single tools; `a11y` takes `ref`, `action`, `action_name`,
`text`, `value` as `vm_accessibility_action`; `wait` waits `settle_ms` for the
screen (as `vm_screen_wait`), or until `until_text` is recognized on screen, or
until an element matching `until_name`/`until_role` exists (polled every 300 ms
up to its `timeout_ms`, default 5000). Every step's shape is checked before the
first is sent. `timeout_ms` (default 30000, at most 120000) bounds the batch.

Coordinates are in the pixels of the frame given (`frame_id`, `width`, `height`,
`source_generation`, required when there are input steps). Every input step is
sent bound to a frame the backend checks as for the single tools: the caller's
frame (re-anchored when old and unchanged, as above), reused while it is younger
than 4 s, then a fresh frame taken before the step. A fresh frame whose size or
generation differs from the batch's fails the step with nothing sent. A redraw
caused by the batch itself does not fail it; express what must have happened
between steps with `wait` steps. The reply lists a short summary per step done
(`click @960,316`, `text 12 chars`, `a11y set_text e12`, `wait found …`) and, on
failure, `failed` (`index`, `type`, `error`) with `isError` set. `observe` runs
once at the end or at the failure and compares with the frame the batch started
from. With `interactive_only` trees the delta leaves out static text such as a
confirmation label; wait for it with `until_name` or `until_text`, or observe
`text`.

Fixture measurements (`agent_view_tests.rs`, scripted daemon and helper, text
tokens counted as bytes / 4, images as width × height / 750 after fitting
1568 px; run with `cargo test -p vm-control --lib agent_view::tests::measure -- --nocapture`):

| Flow | Before: calls, tokens, bytes | After: calls, tokens, bytes |
| --- | --- | --- |
| Linux desktop, act by ref and verify | 3, ~33.9k, 157 KB (JSON tree, action, JSON tree) | 2, ~2.2k, 9.5 KB (compact `interactive_only` tree, action with `observe` delta) |
| QEMU 1920×1080, click and verify | 5, ~5.0k, 1.72 MB (3 screenshots, click, wait) | 1, ~0.1k, 0.6 KB (click with `observe` delta: regions and their text) or 1, ~1.8k, 186 KB (`observe` screenshot at 1568 px) |
| Form, 5 fields and Submit, by ref | 8, ~34.2k, 159 KB | 2, ~2.3k, 9.8 KB (tree, one batch) |
| Form, 5 fields and Submit, QEMU coordinates | 19, ~12.0k, 4.0 MB (a screenshot before each field, as model turns outlast 5 s) | 2, ~1.9k, 186 KB (1568 px screenshot, one batch) |

These are fixture numbers for the MCP surface, not live Guest timings; a real
tokenizer counts JSON refs and numbers more densely than 4 bytes per token, so
the JSON rows understate the earlier cost.

Reproduce the actual disposable Linux input acceptance (requires existing pinned
QEMU/Alpine prerequisites and the console broker/native helper):

```sh
cargo build -p vm-control --bins
python3 scripts/test-control-input.py --output .local/evidence/control-input-new.json
```

### Browser desktop input (`desktop_input`)

The web UI's browser desktop (`docs/browser-desktop.md`) sends input through the
local RPC `desktop_input` with exactly `id` and `events` (1–64 of `move`,
`button`, `wheel`, `key`; `crates/control/src/desktop_input.rs`). On QEMU/HVF
the worker injects through QMP `input-send-event` and is refused while a
frame-bound input operation or the native window input is active. On Apple
Virtualization Linux the daemon only validates the batch and replies `{"inject":"stream"}`; the caller forwards the events over the guest
desktop stream to the agent's uinput devices. It is not an MCP tool.

### Apple Virtualization Linux desktops

Linux desktop containers (`veynodevm run --desktop` without `--gpus`) need no
Guest setup for MCP: the guest agent (`--desktop-stream`, set by container-init)
answers `screenshot`, `screen_wait` and frame-bound input over the host-only
vsock service (`guest/veynode-agent/observe.go`). Screenshots are PNG of the
active DRM scanout (JPEG when a busy screen exceeds 700 KiB), with a fresh
`frame_id` and a `source_generation` that changes with the display geometry; the
hardware cursor plane is not included. The daemon plans input as for QEMU,
translates it to evdev codes (`desktop_input::planned_evdev`) and the agent
checks the frame (≤5 s old, same generation and size as the current scanout)
and the operation before writing to its uinput keyboard and absolute tablet.
Cleanup releases what an operation holds. A wheel release has no evdev event and is not
sent.

### Waiting for screen changes (`vm_screen_wait`)

`vm_screen_wait` (observe) blocks until the screen differs from a baseline, then
optionally until it has been still for `settle_ms` (0–5000), within
`timeout_ms` (1–25000, default 10000). The baseline is the screenshot named by
`frame_id`, so a change that already happened after acting on it counts, or the
screen when the call starts. It returns `changed`, `settled`, `first_change_ms`,
`elapsed_ms` and `resized`:

| Backend | Detection | Regions |
| --- | --- | --- |
| Apple Virtualization Linux desktop | agent, 64x64 tile hashes every 30 ms | changed areas in screenshot pixels; `region` limits the wait to one area |
| QEMU/HVF | console `frame_status.source_sequence` (one step per SPICE draw) every 30 ms | none; `region` refused |
| macOS Guest | screenshot SHA-256 every 250 ms | none; `region` refused |

The daemon remembers the last 16 screenshot anchors per VM for QEMU and macOS
baselines. A blinking cursor or a clock is a change; use `region` (Linux) or a
short `settle_ms` window accordingly. Take a new screenshot before input, or pass
`observe` to the input call itself, which waits the same way and returns a fresh
frame and what changed (see "Act and observe" above).

### Reading screen text (`vm_screen_text`)

`vm_screen_text` (observe) takes a screenshot and runs the host helper
`vm-vision` (`native/tools/vm-vision.swift`, Apple Vision text recognition;
built by `native/tools/build-vm-vision.sh` and registered as `vision` in
`runtime.json`) on it. It works for every backend with screenshots and needs
nothing in the Guest. The reply carries the screenshot's `frame_id`,
`source_generation`, `width` and `height` and `lines` in reading order, each with
`text`, `confidence` and a box plus center point in screenshot pixels, so a
recognized word can be clicked with the input tools directly. `find` adds
`matches`: every case-insensitive occurrence of a string with its own box.
(`vm-vision row --y Y --from X` is a second mode used by the macOS first-boot
grant: the runs of pixels along a row that differ from its background, which
finds the control at the end of a list row. `image` crops, scales and encodes a
screenshot for `vm_screenshot`; `diff --before PATH` returns the changed regions
between two screenshots and `text --crop X,Y,W,H` reads only part of one, for
`observe`.)
`fast` uses the fast recognizer; `languages` takes up to 8 BCP 47 codes (default
automatic). The helper reads the screen in overlapping full-width bands, each
upscaled twice: Vision skips rows of small text on a full desktop and reads
10–12 px panel text reliably only when upscaled. A warm call takes 0.2–0.6 s;
the first on a host loads Vision's models and can take tens of seconds (60 s
limit). Lines beyond the 1 MiB MCP reply are truncated and
`lines_truncated_from` gives the total.

### Windows and applications (`vm_desktop_state`, `vm_window_action`, …)

Apple Virtualization Linux desktop containers answer four more tools from the
guest agent, which talks X11 (EWMH) to the container's display :0, so nothing
is installed in the image. `vm_desktop_state` (observe) returns the screen
size, cursor, keyboard layout (`_XKB_RULES_NAMES`), workspaces, window manager,
the focused application and the managed windows top-most first: id, title,
class, pid and command, type, states (`maximized_vert`, `hidden`,
`fullscreen`, …), workspace (-1 = all), client geometry and window frame in
screenshot pixels. `vm_desktop_apps` (observe) lists visible `.desktop`
entries (system, `/usr/local`, flatpak exports and the session user's own).
With input permission, `vm_window_action` asks the window
manager to `activate`, `close`, `minimize`, `maximize`, `unmaximize`,
`fullscreen`, `unfullscreen` or `move_resize` (`x`, `y` place the client area)
a window and returns it as left 150 ms later (null once closed);
`vm_app_launch` starts an entry by id as the desktop session's user, with its
environment, in a new session (terminal entries in the installed terminal),
then waits up to 5 s for its new window and activates it: window managers keep
a window started without a user action from taking focus, so keys would go
elsewhere.
QEMU and the opted-in macOS Guest
service return `unsupported`; `veynodevm` macOS containers answer them too (see
below).

### Accessibility tree (`vm_accessibility_tree`, `_find`, `_action`)

The same containers expose their AT-SPI accessibility tree. The guest agent
speaks D-Bus itself (`guest/veynode-agent/dbus.go`, `atspi.go`): it finds the
desktop's accessibility bus through `org.a11y.Bus` on the session bus (or the
`AT_SPI_BUS` root window property) and works as the desktop session's user, in
a helper process when that is not root, since the buses admit only their
owner. Reading sets `org.a11y.Status.IsEnabled` for the session, which Qt and
Chromium wait for (reported once as `accessibility_enabled_now`); GTK exposes
its widgets whenever at-spi2 runs, and `vm_app_launch` also starts
applications with `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`. An image without
at-spi2-core returns `accessibility_unavailable`.

`vm_accessibility_tree` (observe) returns elements in tree order with `depth`,
`role`, `name`, `text` (fields without a name; the last 40 lines of a
terminal), `value`, `states` (only the informative ones, plus `disabled`),
`actions`, a `box` with center point in screenshot pixels and a `ref`;
applications carry their `pid`. Only showing elements are walked unless
`include_hidden`; `app` limits it to one application (name substring or pid),
`max_depth` (25) and `max_nodes` (400, at most 2000) bound it, and a walk
stops at 6000 visited elements or 8 s (`truncated` says why). Each tree level
is one pipelined D-Bus round trip, so a desktop with a terminal reads in about
30 ms. `vm_accessibility_find` (observe) searches by `name` (substring of name
or text) and/or exact `role` and returns only matches with their `context`.
`vm_accessibility_action` (input permission) acts on a `ref`: `do`
(the element's `action_name`, else click, activate, press, toggle or jump,
else the first), `focus`, `set_text` or `set_value`, and returns the element
100 ms later.

Through MCP both tree tools default to `"format":"compact"`: a header line
(`count`, `visited`, `truncated`) and one line per element, indented by depth in
the tree (not in `_find` or `interactive_only`):

```text
e12 push button "Save" @1119,598 [focused,is_default]
e40 dialog "Save changes?" @960,512 512x256 [modal]
e7 text ="Meeting notes…" [editable,multi_line,focused]
```

That is `ref role[:subrole] "name" ="text" placeholder "…" value=V @center_x,center_y`
(windows and dialogs add `WxH`) `[states] {actions}` (only when there are
several) `+N` (unexpanded children) `pid=P` and, from `_find`, `in "context"`.
Names are cut at 80 and texts at 160 characters. Refs are short (`e12`): the
adapter maps them to the Guest's refs for the life of the MCP session, gives an
element the same short ref in every read and never reuses a number, and
`vm_accessibility_action` takes either form (an unknown short ref is refused
before anything is sent; its reply is a compact line too). `"format":"json"`
returns the Guest's elements unchanged. `interactive_only` keeps applications,
windows and dialogs plus elements with actions, editable or focused ones and
controls by role, filtering in the adapter: it asks the Guest for four times
`max_nodes` (at most 2000) so `max_nodes` counts what is shown. On the fixture
in `agent_view_tests.rs` (a GNOME-like desktop of 420 elements, 400 read) the
JSON form is 78.9 KB, the compact form 15.7 KB and compact `interactive_only`
8.8 KB.

### Desktop and accessibility tools in macOS containers

`veynodevm` macOS containers answer the same seven tools with the macOS
Accessibility API, which the first boot grants to the Guest's SSH sessions.
The daemon validates the arguments as for Linux, then
runs `veynodevm __desktop VM METHOD ARGUMENTS`, which runs
`native/tools/vm-macos-desktop.swift` (built by
`native/tools/build-vm-macos-desktop.sh`, registered as `macos_desktop` in
`runtime.json`) in the Guest over SSH with the arguments on stdin. The CLI
copies the helper to `~/.veynodevm/desktop-<hash>` on first use and whenever
the host's copy changes. A call takes 0.15 to 0.7 s, most of it SSH.

- `vm_desktop_state`: screen size, `scale`, `work_area` (the screen without
  menu bar and Dock), cursor, keyboard input source, the focused application
  (pid, name, bundle id, window) and every regular application's windows,
  those on screen top-most first: id (the CGWindowID), title, app, bundle id,
  pid, type (AX subrole in words, such as `standard window`), states
  (`focused`, `main`, `minimized`, `fullscreen`, `modal`, `hidden`) and
  geometry including the title bar.
- `vm_desktop_apps`: application bundles in `/Applications`,
  `/System/Applications`, their `Utilities` and `~/Applications`, with bundle
  id, name, path, category and whether it runs.
- `vm_window_action`: AX attributes and buttons. `activate` also unminimizes,
  `maximize` fills the work area, `unmaximize` is unsupported (macOS has no
  such state; use `move_resize`), `move_resize` places the window's origin.
- `vm_app_launch`: `open -b BUNDLE_ID`, then waits up to 5 s for a new window
  and activates it.
- `vm_accessibility_tree` and `_find`: every regular application, the
  frontmost first, with its menu bar (menus show their items once opened with
  `do`). Roles and subroles are the AX names in words (`AXStaticText` is
  `static text`, `AXCloseButton` is `close button`); `role` matches either.
  Elements add `subrole`, `identifier` and `placeholder`; checkboxes, radio
  buttons and menu items report `checked`/`mixed` states, other numeric
  values `value`. A text area's `text` is its last 40 lines. `app` matches
  name, pid or bundle id. The same limits apply (6000 visited, 8 s).
- `vm_accessibility_action`: refs are `pid|child path|AX role|identifier or
  title`. Applications add and remove elements (Calculator's AC becomes C),
  which shifts paths: when the element at the path has another role or key,
  the action uses the application's one element with the ref's role and key,
  or fails as gone if there is none or several; the reply's `ref` is the
  current one. `do` prefers press, confirm, pick, show menu, raise.

### Windows machines over SSH

Windows machines (`machine create --os windows` on QEMU/HVF) answer
`vm_exec`, the six file tools and the seven desktop tools over Windows' own
OpenSSH Server, as the user whose key `install-distro.py windows` set up.
Nothing is installed in the Guest: the daemon sends PowerShell scripts
(`guest/windows/*.ps1`, built into `vm-control`) as text and Windows
PowerShell 5.1 runs them. Requests and replies are UTF-8; the scripts that
use the SSH console set its encoding first, because it defaults to the OEM
code page (`tests/windows_scripts.rs`). The daemon needs a port forward to the Guest's port
22 (`--publish 2222:22`) and the login, which `install-distro.py` records with
`veynodevm machine ssh-key NAME --user USER --key PRIVATE_KEY`
(`machines/NAME/ssh`, 0600). Without either, calls fail and the error names
the missing step; a stopped machine or an SSH failure says so too.

- `vm_exec`: argv runs without a shell (`Process.Start`; the command line is
  built with the Windows quoting rules), so `cmd /c …` and
  `powershell -Command …` work as programs, in the SSH user's home, without
  a window. Up to 30 s and 64 KiB per stream; a timeout kills the process
  tree and returns exit code -1. `vm_exec_start`, `_poll` and `_cancel` are
  unsupported.
- Desktop tools: an SSH session has no desktop, so `guest/windows/session.ps1`
  runs `desktop.ps1` in the logged-on console user's session through a
  one-off scheduled task (interactive token, hidden, `conhost --headless`, so
  no window or console flash), waits for its result, then deletes the task
  and its request directory (`%ProgramData%\VeynodeVM\desktop\<id>`, readable
  only by SYSTEM, administrators and that user). No user logged on is an
  error. `vm_desktop_state` and `vm_window_action` use Win32 (the Alt+Tab
  window list, DWM visible frames, physical pixels); `vm_desktop_apps` lists
  Start menu entries (`Get-StartApps`), `vm_app_launch` opens one through
  `shell:AppsFolder` and activates its new window; the accessibility tools
  use UI Automation (control view) with the same node shape, limits and
  `do`/`focus`/`set_text`/`set_value` actions as Linux and macOS. Refs are
  `hwnd|child path|role|AutomationId or name` and re-resolve like macOS refs.
  `fullscreen` is unsupported (Windows has no such window state).
- `vm_windows` stays macOS-only, as on Linux: `vm_desktop_state` returns
  the windows.

Measured on one Windows 11 26300 ARM64 machine through the daemon
socket: `vm_exec` 1.2 to 1.9 s (SSH and PowerShell start), file commit or
download 1.5 to 1.7 s, desktop tools 2.5 to 3.8 s, of which the UI Automation
walk is 0.07 to 0.19 s and the rest SSH, the task and a second PowerShell
start. A compiled helper would not shorten the measured part, so there is
none. The first desktop call 30 s after boot timed out once (40 s) while
the logon settled; the next calls answered in 2.6 s.


An installed Apple macOS VM can serve `vm_screenshot` through the same host-only
vsock integration, without command execution. Launch the Guest service with
`--allow-observe` as its signed-in user; existing Guest Screen Recording permission
must already cover the responsible application. `vm_guest_info` reports the actual
observation permission/availability. The controller never asks for TCC permission
or substitutes a host screenshot. The adapter remains observe-only unless other
scopes are explicitly enabled.

The response is full-resolution JPEG, up to700KiB, with declared quality ladder
0.80→0.65→0.50→0.35 and actual chosen quality. It includes source/image dimensions,
point-to-pixel scale, Guest display/point bounds, frame UUID and Guest API completion
time. No resize is hidden; unsupported geometry, capture denial, busy/timeout or
all compression levels exceeding the cap return errors. MCP image content uses
actual `image/jpeg`; QEMU retains `image/png`. JSON/request/output limits stay
unchanged. Screenshot support is conditional on Guest service permission, not a
claim that every running macOS VM has automatically granted observation.

### macOS containers through Screen Sharing

Apple macOS configs may set `mac_address` (a locally administered unicast MAC
for the NAT NIC) and `screen_sharing_login` (an absolute path to a private file
holding `user\npassword`, which requires `mac_address`). With both, the worker
answers `vm_guest_info`, `vm_screenshot` and bounded input itself: it finds the
Guest address in the host DHCP leases for that MAC, connects to the Guest's
Screen Sharing on port 5900 with Apple authentication (RFB type 30), reads raw
frames and sends pointer/key events. Responses use the same JPEG ladder, frame
UUIDs, generations and input validation as the Guest service, and
`vm_guest_info` reports transport `screen-sharing-rfb`. No TCC permission is
needed in the Guest; it shows its "screen is being observed" indicator.
`veynodevm` writes these fields for macOS containers.

`veynodevm run` (also with `-d`) finishes a macOS container's first boot
before returning, and part of that is granting the Guest's SSH sessions
Accessibility and Automation of System Events, so `exec` can drive the desktop
with AppleScript, JXA or the AX API. SIP keeps the TCC database read-only even
for root, so the CLI uses the Guest's UI through this same daemon path: an `AXIsProcessTrusted` call over SSH lists
`sshd-keygen-wrapper` in Privacy & Security › Accessibility; the CLI opens that
pane, finds the row with `vm-vision text`, finds the switch at the end of the
row with `vm-vision row`, clicks it, types the login password into the
authorization dialog and checks the result over SSH. Then it sends System
Events an Apple event and clicks Allow on the consent prompt. It needs the
English UI and a password typeable on a US layout; a failure is a warning and
the container still runs. On a fresh Tahoe image it adds 5 to 30 seconds.

See [Guest service](../../native/guest-macos/README.md) for staging/restart and
capture behavior. Registering an existing stopped macOS config does not clone,
format or reinstall its disk. A daemon cannot attach a standalone running worker:
normal Guest shutdown and worker exit are required before handing the same disk
and machine identity to the daemon. Never start two writers to perform acceptance.

The same trusted local owner can manage the private macOS streaming bridge through
`call start_guest_stream_bridge` with `id` and an absolute `socket_path`, and
`call stop_guest_stream_bridge` with only `id`. These local lifecycle operations
are not MCP tools and are unavailable through the HTTPS adapter, even to a principal
holding every scope. They require a running Apple macOS worker. The native worker
validates private socket ownership/modes and fixes the Guest endpoint to vsock5001;
start acknowledges only a listening Unix socket, not successful capture or Guest
connection. The controller waits at most8s for each bridge management response.

### Guest bridge listener provenance (local only)

A new controller supplies per-worker and per-start random256-bit instance nonces to
`start_guest_stream_bridge`. A matching new Apple worker returns `binding` with
version1, VM ID, both instances, worker PID, socket path/device/inode and fixed
Guest port5001. The scope is `host-worker-listener-only`; `input_authorized:false`
is mandatory. Listening does not prove Guest connection, capture identity, or a
frame belonging to the same desktop as a later screenshot.

To revalidate an obtained binding without connecting to or consuming the stream:

```sh
vm-control call --state-dir PRIVATE_STATE guest_stream_bridge_binding \
  '{"id":"VM_ID","worker_instance":"64_LOWERCASE_HEX_FROM_START","bridge_instance":"64_LOWERCASE_HEX_FROM_START"}'
```

The local RPC accepts exactly those three arguments. It is not an MCP tool or a remote HTTP method. The call
requires a live current worker, repeats the native listener check, and compares
VM/worker/bridge identity and owner-only mode0600 socket under mode0700 parent.
Stopping invalidates the binding before waiting for cleanup. Restarting a worker
or bridge invalidates previous nonces even when the same pathname is reused.
A late start reply cannot republish a stopped or superseded attempt.

Old native callers without nonces retain the diagnostic start/stop behavior but
receive no provenance. New controllers fail explicitly with old workers; stop a
failed/unsupported start before retrying, since an unknown worker may have opened
a listener. A failed start never grants provenance. The source binding is valid
only when freshly rechecked; there is no claim of atomicity across a later socket
connect or input RPC. Host user/worker/controller remain trusted.

Linux Apple Virtualization guests use the same bridge for the browser desktop
(`docs/browser-desktop.md`). macOS browser input remains disabled. Guest service/console identity must still
be checked against capture and screenshot metadata before a future media/control
integration can authorize it. Do not use this listener proof as that identity.

### Local experimental Venus profile

The default QEMU Linux profile remains `virgl`, with the original graphics and
HVF arguments. A VM can explicitly select
`"qemu_graphics_profile":"venus-moltenvk-experimental"`. This value is rejected
for other backends/Guests. VM config and MCP cannot supply runtime paths or
renderer environment overrides. The existing stdio MCP lifecycle `vm_create`
accepts only the two profile enum values; `qemu_guest_exec` remains local-only.
Remote HTTPS continues to hide and reject `vm_create` entirely.

The local daemon operator must also pass `--qemu-venus-runtime ABS_JSON`. Its
strict schema 1 contains `profile_revision:"owned-group-v1"`, canonical
`frameworks_dir`, and `loader`, `host_library`, `render_server`, `qemu_library`,
`spice_library`, `icd`, `firmware`, each exactly `{path,sha256}`. The registration
is a private regular file (0600, <=64 KiB); referenced regular files are bounded,
non-symlink canonical paths in locally trusted directories. FIFO/device files,
unknown fields, changed identities and hashes fail closed. The compiled
`qemu_venus_catalog.json` admits only the reviewed matching loader, host virgl
with inherited process-group ownership, standalone server, and pinned runtime
closure. A manifest cannot bless a different binary by declaring its own hash.
The 27 pinned framework files cover the selected transitive `@rpath` closure plus
EGL/GLES/MoltenVK; this is a local integrity catalog, not redistribution approval.

Registration requires Apple Silicon macOS 26+ and a successful public Hypervisor
**configuration object** 4 KiB set/readback; this probe creates no VM. Venus uses
`-machine virt -accel hvf,ipa-granule-size=4096` and a named GPU with `blob=on`,
`hostmem=256M`, `venus=on`. Before `cont`, the worker checks actual QMP GPU
properties and a paused/prelaunch state, after existing disk FD checks.
This does not claim a QMP readback of the running Hypervisor IPA granule. The
pinned QEMU must initialize with the requested option; startup errors stay errors.
The 256 MiB hostmem aperture is not a measured RAM allocation.

Both profiles clear inherited Vulkan/Metal-driver/ANGLE/renderer/`DYLD_*`
injection variables, then restore only explicit registered SPICE and fixed
ANGLE Metal. Venus additionally selects the registered host library/server/ICD;
there is no software-renderer or legacy-runtime fallback. Same-UID trusted
operators can still alter host files; registration rechecks before launch do not
provide protection from arbitrary malicious code running as that operator.

`runtime.graphics_profile` and per-VM capabilities distinguish the configured
profile, successful host admission, and `guest_verified:false`. Actual Guest
Vulkan driver identity and application pixels must be independently measured;
SPICE connectivity, QOM configuration, and host API support do not verify Guest
Vulkan, native desktop scanout, conformance or performance. Existing disk locks,
execution guardian supervision, input authority and normal/forced cleanup
are retained. New production-path runtime acceptance remains separate from
component tests and earlier standalone renderer lifecycle tests.
