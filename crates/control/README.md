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
`--allow-execute` separately adds commands and control leases.
Desktop input is separately enabled with `--allow-input` on QEMU persistent capture; accessibility remains unsupported. `vm_screenshot` requires registered QEMU capture or an opted-in macOS Guest observation service with existing Guest Screen Recording permission; Apple Linux returns unsupported. Independent
`--allow-files` enables bounded transfer tools only when Guest files integration
is explicitly configured. Linux uses a dedicated files account; macOS requires
Guest `--allow-files --files-root /absolute/private/directory` and uses the service
UID. Execute permission does not imply files permission. Query `vm_guest_info`
for the running Guest's `capabilities.files` and `file_policy`; the OS name alone
does not establish availability.

`vm_create` registers an existing direct-boot Linux configuration or installed
macOS configuration. It does not install the OS or allocate a guest disk.
The registry is atomically replaced and fsynced. Directory permissions are 0700,
socket/registry 0600; flock prevents two daemons owning one registry. This is a
same-user Unix socket trust boundary, not isolation from other processes running
as that macOS user. Local CLI/API can reference that user's files.

Each VM has a separate native subprocess. MCP EOF releases its control leases,
not its VMs. Closing the native window preserves the VM. Daemon death closes the
native subprocess control pipe; native shutdown policy applies. Restart does not
silently resume VMs. Concurrent active VMs cannot attach the same writable disk device/inode in one
daemon, including hard-link aliases. The daemon stores the identity at start so
replacing a configured path cannot hide an already attached inode. Worker-level
advisory locks are a separate native backend safeguard.

Normal shutdown supports `vm_shutdown {"id":"...","mode":"backend"}` (default)
and `mode:"guest"`. Backend mode requests the framework's virtual power button;
the guest may ignore it. Guest mode sends the dedicated shutdown operation over
vsock to an installed Linux service started with shutdown explicitly enabled.
It uses lifecycle permission, not command-execution permission or a control lease.
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

For an explicit local experiment, pass `--memory-budget-mib MIB` to `daemon`.
The value must be positive and no greater than physical host memory; it may
intentionally reduce the default host reserve. Remote MCP cannot alter it.
The override applies to that daemon invocation and is not persisted in VM configs.

## Control ownership

Before `vm_exec`, call `vm_acquire_control` with the VM id. Each MCP connection gets
an internal random owner token. Leases last 60 seconds; repeat acquire to renew.
`vm_release_control` releases it; clean MCP EOF also releases it. Abruptly killed
MCP clients lose access at expiry. Owner tokens and epoch are injected by the
adapter and cannot be supplied in MCP tool arguments.

The local human can execute:

```sh
./target/debug/vm-control call --state-dir /absolute/private/state human_takeover '{"id":"linux-agent"}'
./target/debug/vm-control call --state-dir /absolute/private/state human_release '{"id":"linux-agent"}'
```

Takeover increments the server epoch, rejects subsequent commands by the old
holder and blocks new acquisitions until human release. It cannot retract a
command's completed side effects. Both synchronous commands and explicit async
jobs use registered Guest jobs with best-effort process-group cancellation as
described below.
There is no native UI takeover button yet.

## Bounded asynchronous commands

Execute-enabled MCP connections expose `vm_exec_start`, `vm_exec_poll` and
`vm_exec_cancel`. Start takes the same argv and 1–30000 ms timeout as synchronous
`vm_exec`, and requires current VM control. Its text result retains the Guest
protocol wrapper:

```json
{"id":"native-request-id","result":{"job_id":"32-lowercase-hex-characters","state":"running"}}
```

Poll returns `result.state` (`running` or `finished`) and, when finished,
`result.result` with exit code, stdout/stderr, truncation flags, timeout and
cancellation flags; internal failures instead appear as `result.error`. A poll
reply does not mean the command succeeded: inspect the terminal result. Cancel
returns `result.cancel_requested`; it is not rollback or proof of termination.
Poll again to establish the actual terminal state, including completion races.

Jobs bind to VM id, this MCP connection's random owner and the lease epoch at
start. Other connections cannot poll or cancel them even if they know a job id or
later obtain VM control. This connection may still poll/cancel its previously
authorized jobs after its lease is revoked; that grants no authority to start new
commands. The adapter remembers original job epochs, so later lease renewal does
not accidentally change job ownership. Synchronous `vm_exec` internally uses
`exec_start`/`exec_poll`/`exec_cancel` and preserves its existing terminal result
wrapper; it requires these Guest capabilities and never silently falls back to
legacy uncancellable execution. Host ownership metadata is capped at 256
jobs including in-flight starts, contains no command bodies/output, and expires
330 seconds after registration. At capacity, the daemon and adapter evict the
oldest registered record known to have finished before rejecting a new start.
Completed results can therefore expire earlier; active jobs and in-flight start
reservations are never evicted for capacity. Cancellation acknowledgements alone
do not mark a job finished. A restarted VM clears its job ownership records.
Guest-side retention and execution limits may reject or expire jobs sooner.

Local `human_takeover` first revokes the execution epoch, then requests cancellation
of known unfinished jobs. The response separately reports completed cancellation
requests, failures and unconfirmed requests after a 12-second reporting deadline.
A start response arriving after takeover is registered and immediately receives a
best-effort cancellation request; its response includes `control_revoked` and the
cancellation outcome. These safeguards cannot undo completed side effects or
guarantee termination if the Guest integration transport has failed. Guest jobs remain bounded by their original
timeout. Transport hangup monitoring runs even while synchronous execution is
waiting, and a scope guard runs on normal MCP EOF and read/write failures: the daemon first
revokes all that session owner's leases, then requests cancellation of unfinished
owned jobs. Explicit control release does the same for that VM. HTTPS DELETE and
expiry close the stdio session even while its tool response is pending and use
this cleanup. A disconnected direct Unix-socket synchronous caller also requests
cancellation of its own job. An individual HTTP connection closing does not end
its authenticated MCP session; use session DELETE or wait for its configured TTL.
Failures remain best effort;
SIGKILL of the MCP adapter cannot run a guard, so its lease may last until the
60-second TTL and its guest command until its original maximum 30-second timeout.
Async completion/output is not recoverable by a newly created connection. No job
is automatically resumed. Prefer explicit cancel and terminal polling when
confirmed termination is required.

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

## HTTPS endpoint

`vm-mcp-http` exposes `/mcp` using rustls with TLS terminating on the user's Mac.
It defaults to `127.0.0.1:8443`; wider binding requires an explicit bind address.
No public deployment or firewall changes are made by this repository.

The transport supports MCP version `2025-06-18`, JSON POST responses, notification
202 responses and DELETE. GET returns 405 because SSE delivery is not implemented.
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
      "vm_ids": ["linux-agent"]
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
Tokens are VM/session-bound. No desktop control lease or execute permission is
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
  --qemu-library "$PWD/.local/windows-gpu/UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu" \
  --qemu-resources "$PWD/.local/windows-gpu/UTM.app/Contents/Resources/qemu" \
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

Guest serial logging is disabled by default. Explicit `serial_log` enables an
owner-private append-only diagnostic file, bounded to 8MiB. Backend stderr is
retained in a private per-run file, also bounded to 8MiB. Disabled/full/failed log
outputs are drained. Screenshot PNG data is not persistently stored by the
controller. The explicit test script saves diagnostics and screenshots.

`vm_screenshot` is observation-only and returns MCP image content plus UUID,
timestamps, physical Guest pixel dimensions and unknown Guest UI scale. Internal
SPICE scanout IDs are removed. One asynchronous capture per VM is allowed; another
returns busy. Screenshot responses are <=3MiB, PNG <=2MiB, worker deadline 15s, control
wait bound 17s. Shutdown/stop can be processed while capture is pending. Capture
currently starts a short-lived SPICE consumer: a completely static desktop that
sends no new draw can fail explicitly. Use the persistent console broker below to
keep the capture connection from VM startup. Input uses separate current control authorization plus a recent frame observation token; a screenshot alone grants no input permission.
The larger response bound applies only to an outstanding screenshot request;
ordinary replies and all incoming requests remain <=1MiB. Persistent native/broker
records remain <=4MiB. The full-resolution lossless PNG is never resized to fit;
images above 2MiB fail explicitly, so not every 1440p/4K scene is guaranteed to fit.
Screenshot JSON avoids optional slash escaping to keep worst-case base64 within
its response budget. The macOS Guest JPEG producer and 700KiB validation remain
unchanged. Fixed native error diagnostics report the stage and encoded byte count,
never pixels or file paths.

Reproduce the actual RAM-only test (requires the pinned media and local runtimes):

```sh
cargo build -p vm-control --bins
python3 scripts/test-control-qemu.py --output .local/evidence/control-qemu-new.json
```

The 1536MiB QEMU fixture and 512MiB VZ isolation fixture never attach a disk. The
stock graphics stack in this test retains known 2D/text rendering defects. The
controller test is not pixel correctness, Windows GPU, native-window, performance,
release packaging or licensing acceptance. See `docs/evidence/control-qemu.md`.


### Persistent console registration

Register `--qemu-console-broker "$PWD/target/debug/vm-console"` and
`--qemu-console-helper "$PWD/.local/bin/spice-console"` together to enable the
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
the controller pass. Internet operation is not validated. See `docs/evidence/control-qemu-persistent.md`.

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

### Frame-bound physical desktop input (QEMU only)

Start the MCP adapter with **`--allow-input`** or give a HTTPS principal the
independent **`input`** scope alongside `observe` and an explicit VM allowlist.
`execute`, `files` and `lifecycle` do not imply input permission. Acquire/renew the
existing 60-second control lease, take `vm_screenshot`, then copy its `frame_id`,
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
  Unicode text injection. Presses are followed by releases.

Input requires the persistent console broker and a screenshot observation younger
than 5 seconds. Each worker keeps at most 16 observation metadata records; restarting
the VM discards them. Static source capture timestamps may be old: expiry uses the
host's new screenshot observation time without falsifying the original capture time.
Every step rechecks the daemon's VM/client/epoch lease and queries the native
capture owner's current generation/dimensions through `frame_status`; unavailable,
changed, foreign and expired frames fail closed. The currently validated mapping is
single-display Linux Xorg/libinput, physical pixels and default calibration.

Capture validation and QMP injection occur in separate processes and cannot be
atomic; a resize or takeover can race an **already dispatched** step. Subsequent
steps are rejected after revocation. Failed/interrupted operations release only
buttons/keys belonging to their opaque operation token. Concurrent operations are
rejected while one is active. A worker safety deadline of 4 seconds is checked
between bounded RPCs; failed QMP cleanup is reported and, on safety-deadline retry
failure, terminates the worker/VM rather than declaring release success. This does
not undo clicks already delivered. QMP acknowledgements mean dispatch, not proof
that an application accepted input. Guest evdev/Xorg acceptance is tested separately.

The local trusted-user Unix daemon can be used for `human_takeover`/`human_release`;
these are not remote Agent tools. Actual stdio disconnect and human takeover during
drag release the button and prevent completing the drag. Internet browser input,
accessibility, Unicode/layout handling, Retina calibration, multiple displays,
macOS input and Windows input are still unsupported.

Reproduce the actual disposable Linux input acceptance (requires existing pinned
QEMU/Alpine prerequisites and the console broker/native helper):

```sh
cargo build -p vm-control --bins
python3 scripts/test-control-input.py --output .local/evidence/control-input-new.json
```

### macOS Guest screenshots

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

macOS browser input remains disabled. Guest service/console identity must still
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
properties and a paused/prelaunch state, after existing disk/witness FD checks.
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
execution witness/guardian supervision, input authority and normal/forced cleanup
are retained. New production-path runtime acceptance remains separate from
component tests and earlier standalone renderer lifecycle tests.
