# Linux Guest integration service

The production listener is Linux AF_VSOCK port 5000 and accepts only host CID 2.
No TCP listener exists. Launch flags grant independent capabilities:
`--allow-execute` for commands and `--allow-shutdown` for normal init-based
shutdown (also requires root). Without execute permission, all command/job
methods return `forbidden`. The host still must apply per-client permissions.

One bounded JSON request/response is carried per connection. Requests are at
most 64 KiB, have a nonempty string `id` (≤128 bytes) and a string `method`.
Unknown fields and trailing JSON are rejected. Up to 16 simultaneous connections
leave management capacity when all four command slots are executing synchronously.

- `info`: actual OS/architecture and implemented capabilities, including
  `async_exec` when command execution is enabled.
- `exec`: existing synchronous API; `argv` (1–128 arguments, no NUL) and
  `timeout_ms` (1–30000). Results now also contain `cancelled:false`.
- `exec_start`: same arguments; starts a real process and returns
  `{job_id,state:"running"}`. Job IDs are random 128-bit values as 32 hex digits.
- `exec_poll`: takes `job_id`; returns `{job_id,state:"running"}` or
  `{job_id,state:"finished",result:{exit_code,stdout,stderr,stdout_truncated,
  stderr_truncated,timed_out,cancelled,duration_ms}}`. A completed job suffering
  an internal process-wait failure instead returns `state:"finished"` and an
  `error:{code,message}`. It never disguises that failure as successful output.
- `exec_cancel`: takes `job_id`; returns `cancel_requested:true` when a running
  job has been signalled for cancellation, false if already finished. Poll for
  actual completion. A completion/cancellation race may finish naturally before
  cancellation wins; the final `cancelled` field reports the actual outcome.
- `shutdown`: independently authorized `/sbin/poweroff`, without `-f`; returning
  `requested:true` is not evidence of completed VM shutdown. Observe VM state.

Synchronous and asynchronous executions share a limit of four active commands.
Overload returns `busy`. Finished jobs are capped at 32 results and expire five
minutes after completion; expiry is checked on all manager operations, and the
oldest finished results are evicted at capacity. Active jobs are never evicted.
Unknown or expired jobs return `not_found`. Results are in-memory and do not
survive service restart. There is no unbounded output/history cache.

Production Linux execution uses a separate root-owned cgroup v2 subtree per
command. Go's `clone3(CLONE_INTO_CGROUP)` starts the child inside that group before
any user code runs; no post-exec migration window is used. Cancellation, timeout,
and direct-parent completion kill the entire group with `cgroup.kill`, wait up
to one second for `populated 0`, then remove it. Detached `setsid`/double-fork
children remain in the group. Cleanup failure returns `containment_failed`;
already-performed side effects cannot be reversed. The final result distinguishes
`cancelled` from `timed_out`.

This requires a root service, fixed non-root `--exec-user`, usable unified cgroup
v2 and working clone3/cgroup.kill. Startup performs an actual execution probe;
unsupported configurations fail explicitly. `info.execution_containment` reports
`cgroup-v2-clone3-kill`, and `capabilities.descendant_containment` is true only for
this configured mode. The service and separate files worker stay outside command
groups. This is descendant lifetime containment, not a general execution sandbox.

Each stdout/stderr stream is independently bounded to 64 KiB and excess output
is drained while recording truncation. No command bodies or output are logged.
Host RPC callers must understand `finished.error` as a failure and must not
report completion solely from the initial start/cancel acknowledgement.

Tests use real local subprocesses:

```sh
go -C guest/veynode-agent test -race ./...
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go -C guest/veynode-agent build -trimpath -o ../../.local/bin/veynode-guest .
```

Host unit tests prove process handling, bounded state, and protocol logic only;
real Linux VM + MCP acceptance is a separate validation. The separate Swift
macOS Guest service currently does not implement these async job methods.

## Fixed command identity and cleanup

For an installed Linux Guest, create a dedicated account and existing home, then
launch the privileged service with:

```sh
veynode-guest --allow-execute --allow-shutdown --exec-user=veynode
```

The service resolves this account once at startup. UID 0, primary GID 0, missing
accounts, relative/missing homes, and inability to apply credentials fail
startup. A real `/bin/true` credential probe runs before the listener opens.
Every command runs with that fixed UID/GID and **empty supplementary groups**,
with HOME/USER/LOGNAME and working directory set to the selected account. The
request protocol cannot select a different identity. Executable lookup uses the
fixed PATH rather than the privileged service's inherited PATH. The service
keeps its own privilege only for its explicitly enabled shutdown operation and
process management. Execution failure never falls back to root.

The legacy RAM development appliance deliberately launches with
`--legacy-exec-process-groups`. Only that explicit diagnostic option permits
inherited service/root execution without cgroup containment. It reports
`legacy-process-group-not-contained` and false descendant-containment capability;
`setsid` descendants can escape it. Installed provisioners never select this flag.
Omitting `--exec-user` alone no longer enables production command execution.

`info.execution_identity.mode` reports `legacy-root`, `service-user` or
`fixed-user`; its fields include `user`, `uid`, `gid`, `home`, `cwd`, `rootless`
and `supplementary_groups`. `info.service_uid` reports the listener's separate
privilege. Execution-account identity and descendant containment are distinct.

SIGTERM/SIGINT revoke new command admission and request cleanup of synchronous
and asynchronous commands. Any unconfirmed descendant cleanup stops new command
admission, cancels peer jobs and prevents a later service-stop success report. A listener failure also attempts bounded cleanup.
SIGKILL cannot run in-process cleanup: a subsequent service startup exclusively
locks and reaps its own root-protected stale `job-*` cgroups before accepting RPCs.
It refuses unexpected directories or failed cleanup. Until restart, an abruptly
orphaned command may keep running; this is not immediate crash containment and
cannot reverse its intervening effects. Guest reboot/VM poweroff are different
lifecycle events.

The service locks `/run/veynode-exec.lock` and owns mode0700
`/sys/fs/cgroup/veynode-exec`. It mounts cgroup v2 only over an expected unmounted
Guest sysfs cgroup directory; incompatible existing hierarchies are rejected.
The non-root execution account cannot migrate itself out of its group. No host
security setting or host cgroup is changed. No CPU/memory controller limits are
configured yet.

This remains **not a sandbox** for filesystem/network access, privilege gained
through an administrator's existing setuid/sudo policy, kernel exploits or other
processes sharing the execution UID. `rootless` describes initial credentials,
not a promise that every account operation is harmless. MCP session loss is a
separate host cancellation policy, not the Guest service's SIGTERM handler.

Real kernel/installed acceptance and reproduction commands are recorded in
[`docs/evidence/linux-cgroup-2026-10-08`](../../docs/evidence/linux-cgroup-2026-10-08/README.md).

## Explicit Linux file directory

Files are opt-in and independent of command/shutdown permission:

```sh
veynode-guest --allow-execute --exec-user=veynode --allow-shutdown \
  --allow-files --files-user=veynode-files --files-root=/home/veynode-files/transfers
```

The administrator must create the account, existing home and private (0700)
transfer directory first. Use a different non-root UID/GID from the command
account. The listener resolves the account at startup and launches a separate
copy of itself with fixed credentials and no supplementary groups. RPC payloads
cannot change the account, directory or child arguments. The privileged listener
never opens transfer files. This worker is currently Linux-only. `info.file_policy`
reports actual worker UID/GID, configured root, ownership and limits. Published
files are 0600 and owned by the files account; the ordinary execution account
cannot read or change them. Upload-then-execute and arbitrary Guest filesystem
access are not implemented by this boundary.

Current path scope is **one filename directly in the configured directory**.
Absolute paths, separators, `.`/`..`, NUL and reserved `.veynode-` names are
rejected. Go `os.Root` anchors operations to the opened directory, and final
opens use O_NOFOLLOW|O_NONBLOCK. Downloads require an owned, single-link regular
file; symlinks, hardlinks, directories, FIFO/device files and other owners are
rejected. Bind mounts or a malicious Guest root/files-account process are outside
this boundary; this is not a sandbox against the trusted Guest administrator.

The build requires Go1.27.1. The official [os.Root documentation](https://go.dev/src/os/root.go)
describes its traversal protection and mount/device limitations. The published
[GO-2026-4970 advisory](https://pkg.go.dev/vuln/GO-2026-4970) affects older Root
implementations through pre1.27rc2; the checked compiler is Go1.27.1. Independently,
this protocol rejects trailing slashes and all path separators.

Requests include the host-injected 64hex `owner`; random 32hex transfer tokens
are scoped to that owner within this VM's worker. Methods:

- `file_upload_begin {path,size,sha256}` requires declared 0..8MiB size and SHA256.
- `file_upload_write {token,offset,data}` appends valid base64 at an exact sequential
  offset, 1..32768 decoded bytes per request, without exceeding the declared size.
- `file_upload_commit {token}` verifies length/hash, fsyncs the staged file, then
  publishes using Root.Link's atomic no-replace operation and fsyncs the directory.
  Existing destinations are never replaced. Ambiguous post-publication fsync or
  cleanup failures explicitly say the destination may already have been published.
- `file_download_begin {path}` returns token, size and complete SHA256;
  `file_download_read {token,offset}` returns <=32KiB base64 data and `eof`.
  The final read closes the transfer. Verify the assembled hash. Each read checks
  inode/size/mtime/ownership/link-count changes; concurrent edits are not snapshots.
- `file_cancel {token}` removes unfinished upload staging or closes a download.
- Internal `file_session_end {owner}` cleans that session and fences late begins.
  The MCP daemon supplies this on disconnect/DELETE/expiry; it is not a public tool.

There are at most16 active transfers,60seconds idle lifetime and5minutes absolute
lifetime. One-second pruning runs even without requests. Closed owners are retained
for5minutes, capped at256; exhaustion rejects admission rather than dropping the
revocation fence. One file RPC at a time has a5second worker deadline; concurrent
file RPCs return `busy`, and deadline failure terminates the worker and requires
service restart. The limit bounds worker work, not every end-to-end network wait.
Staging lives separately in private `.veynode-staging`; EOF/SIGTERM clean it.
After abrupt termination, the next exclusive worker removes only recognized
reserved partial files. It never removes published files. Cleanup failures are
errors, not successful cancellation. A per-root flock excludes another worker.

Fixture provisioning is opt-in and refuses existing destinations:

```sh
python3 scripts/provision-linux-qemu.py --name NEW-UNUSED-NAME --allow-files-fixture
python3 scripts/test-control-files.py --config .local/vms/NEW-UNUSED-NAME/config.json --expiry
```

The fixture also installs clearly named symlink/hardlink/FIFO/wrong-owner negative
samples in its newly created files root. It does not modify existing VM disks.
