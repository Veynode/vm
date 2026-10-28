# Private QEMU serial execution v2 (implementation contract)

Status: implemented with targeted real-Guest acceptance; still opt-in and not release acceptance.
Status: superseded in part — leases, takeover, exec scopes and the exec witness were removed; exec is a guest timeout plus process-group kill.
The measured character-device behavior in commit 952840d includes buffered bytes
after host close, loss of an observable close edge on rapid reconnect, and Guest
clocks that do not advance during a whole-VM pause. EOF and Guest timers alone
cannot establish command revocation.

## Wire

One connection per QEMU/service incarnation, no reconnect or resynchronization.
Framing is u32 big-endian JSON byte length, request <=65536 bytes, reply <=1048576
bytes. Exact outer keys: `version`, `type`, `guest_nonce`, optional `host_nonce`,
optional `sequence`, optional `request`, optional `response`. Unknown fields fail
closed. Version is 2. Nonces are 64 lowercase hexadecimal characters. Guest
`challenge` carries only version/type/guest_nonce; host `hello` adds host_nonce,
which equals the daemon-generated worker instance; Guest `ready` echoes both.
Only after startup cleanup of old cgroups may Guest send its challenge.

Every host request is type `request`, both nonces, strictly increasing sequence
(start 1), and `request`. Reply is type `response` with identical nonces/sequence
and `response:{id,result|error}` matching the nested request ID. Replies may be
out of order; requests may not. IDs are nonempty UTF-8 strings <=128 bytes.

Nested methods (exact method-specific fields):

- `{id,method:"info"}`: existing info shape; transport
  `virtio-serial-private-exec-v2`, transport_reconnect false, exec/async_exec/
  descendant_containment true, files/shutdown/desktop false, containment backend
  is cgroup-v2-clone3-kill.
- `{id,method:"heartbeat"}` -> `{alive:true}`.
- `{id,method:"exec_start",scope,argv,timeout_ms}` -> existing job_id/state shape.
- `{id,method:"exec_poll"|"exec_cancel",scope,job_id}` -> existing job result or
  cancel-request acknowledgement (not a termination proof).
- `{id,method:"scope_revoke",scope}` -> only successful proof is
  `{revoked:true,pending_starts:0,cgroup_empty:true}`. Error never means clean.

`scope` is exactly `{owner:64-lowercase-hex,epoch:integer 1..9007199254740991}`.
Both envelope nonces bind it to the original worker and Guest service. Job IDs
remain 32 lowercase hex. Existing argv/30-second execution/output limits remain.
Host admits at most one ordinary operation, one revoke, and one heartbeat. Guest
has matching bounded lanes and a single bounded writer (maximum 8 queued replies).
Heartbeat is every 500ms; the Guest independent watchdog fences after 2s without
a received heartbeat. Ordinary host RPC budget is 1s, revoke budget 5s; Guest
scope cleanup budget is 3s. No long-running synchronous exec travels on serial.

## Authority and cleanup

The daemon registers the original worker/owner/epoch cleanup obligation before
writing any start request, including one whose reply/job ID is subsequently lost.
The Guest registers an in-progress start before cmd.Start, checks that the scope
is not retired, and associates starting/running commands with the scope. Revoke
first retires the scope, cancels its jobs and waits for all in-progress starts,
process controls and cgroups to be empty. Failed containment is sticky. Retired
scope capacity is 1024; exhaustion refuses new scope rather than evicting fences.

Revocation becomes successful only on that exact scope proof or confirmation that
the original QEMU incarnation has ended. Receipt/queued ACK is not the
linearization point. Already authorized in-flight work may act before revocation
finishes; no rollback or instantaneous revocation claim is made. The host must
terminate the affected QEMU incarnation if bounded reconciliation fails and
retain an unconfirmed outcome until evidence is available.

A host guardian directly owns the QEMU child (PID remains owned until wait),
issues one fresh challenge at a time over the worker's private pipe, and
kills/reaps QEMU on EOF or an unanswered challenge (5 seconds after arming, plus
up to 500ms challenge cadence and scheduling). Startup allowance is 30 seconds. This is independent of Guest time and worker SIGSTOP. The worker
also ends on daemon EOF; daemon loss plus a stopped worker is bounded by the
independent guardian. The guardian is not protection against a trusted host user
stopping the guardian itself or the entire host: no local code can run then.
Recovery must not automatically issue new authority. Watchdog thresholds plus
bounded I/O/scheduling are engineering bounds, not instantaneous guarantees.

v1 `--virtio-serial` stays observer-only. Guest v2 requires new
`--virtio-serial-exec --allow-execute --exec-user=<fixed account>`, mandatory
cgroup-v2 clone3 containment, no legacy fallback, no files/shutdown. Mutable
config is explicit and must not be advertised or frozen as accepted until actual
Guest failure tests pass. Pure Unix/process fixtures do not replace that gate.

Host configuration opts in with `qemu_guest_exec: true` (QEMU only). On failure,
forced termination may interrupt Guest disk writes; it is not normal shutdown.
The initial host implementation retains failed scopes and refuses renewed
authority rather than claiming success from worker reap alone; there is no
automatic failed-scope recovery endpoint. This restriction survives worker
replacement within the daemon, but is not a durable daemon-crash ledger.


Normal ACPI shutdown has a separate, explicit quiescing state. The daemon must
first find no unresolved execution scope for this VM (otherwise the caller must
release/end its sessions and obtain cleanup). Under the same Manager lock it
fences existing leases and rejects new acquisitions/execution/input for that
worker lifetime. This does not create a new human hold on a future worker.
The trusted worker command carries `execution_scopes_empty: true`; the worker
also requires ready transport and no pending execution RPC. Before sending
QMP `system_powerdown`, it revokes its active input epoch and requires a real
QMP held-key/button release ACK. A failed release never sends the power button.

Only after this transition may serial service EOF be tolerated: Linux normally
stops the service before unmounting and powering off. Guardian challenge/response
continues independently during the 60-second host-time shutdown budget. Repeated
shutdown calls cannot extend that budget. Only normal owned QEMU/guardian exit
reports stopped; timeout, nonzero exit, or interrupted supervision remains
unconfirmed and uses existing failure cleanup. This is still ACPI normal-shutdown
support, not newly enabled Guest-service shutdown. Files remain unsupported on
this transport. Component regressions cannot substitute for installed Guest
shutdown validation; the first actual v2 runtime failed during this exact phase.


## Actual acceptance checkpoint (2026-10-09)

These are separate causal tests on the installed Alpine3.23.6 disposable clone,
QEMU10.0.12/HVF on M1 Pro/macOS26.4.1. Each first required15 real graphics pixel
checks and actual rootless cgroup-v2 integration; no graphics/performance claim
follows from the execution results.

| Test | Observed result | Limit |
|---|---|---|
| Normal execution (`vm-archive/docs-evidence/installed-qemu-exec-live/`) | 26 checks; escaped descendants cannot write after cancellation, timeout, completion, takeover or EOF; normal ACPI exit0 | Single-VM functional sample |
| Worker SIGSTOP (`vm-archive/docs-evidence/installed-qemu-exec-guardian-live/`) | QEMU reaped in5.195s while worker remained stopped | Independent guardian boundary; no Guest scope-empty ACK |
| Whole QEMU SIGSTOP (`vm-archive/docs-evidence/installed-qemu-pause-live/`) | Host serial failure retired original QEMU in3.644s without resume | No host sleep or stale queued dispatch proof |
| Daemon exit plus stopped worker (`vm-archive/docs-evidence/installed-qemu-daemon-exit-live/`) | Original three VM processes absent in0.240s; MCP cleanup unconfirmed | Not guardian-timeout attribution; OS signal cause was not traced; earlier failed oracle preserved |
| Lost start ACK (`vm-archive/docs-evidence/installed-qemu-lost-ack-live/`) | Original owner received no job ID; actual scope-empty ACK and stopped descendant writes; same VM later normally shut down | Worker-to-daemon ACK loss, not serial ACK loss |

A separate restart review found that in-memory scope obligations disappear with
the daemon and can cause a false old-session cleanup receipt after restart.
A durable pre-send journal and recovery-only admission fence are being validated;
the above frozen runtime evidence must not be read as accepting that new path.
Guest service SIGKILL, explicit durable reconciliation, host sleep/wake and
physical power-loss/storage behavior remain separate unfinished gates.

### Durable journal and explicit witness acceptance update (2026-10-09)

The preceding checkpoint's journal/reconciliation work was pending at that time.
Actual v5 evidence (`vm-archive/docs-evidence/installed-qemu-journal-live/`) now records
normal26-check cleanup with an empty journal and a21-check restart fence: the old
owner's EOF remains unconfirmed and cannot erase its persisted obligation.

Actual frozen v6 /5a0737a witness acceptance (`vm-archive/docs-evidence/installed-qemu-witness-live/`)
then passes36checks: active original-incarnation lock denies reconciliation;
after original process retirement and daemon restart, explicit reconciliation
uses the original file identity and exclusive lifetime witness. A new incarnation
executes normally. Retrying the old completed receipt twice leaves its new scope,
launch and active job untouched; unknown instances are refused. This is an
`original_incarnation_witness_exclusive` receipt, not a fabricated Guest cgroup
ACK. Both process chains are confirmed retired and the base hash unchanged.
The tested v6 still requires explicit reconciliation after normal clean shutdown;
later auto-reconciliation work has separate acceptance requirements. No overlapping
daemon generations, host sleep, same-incarnation resume or power-loss durability
was established. The Guest-service crash experiment remains a separate
failed framing acceptance (`vm-archive/docs-evidence/installed-qemu-guest-crash-failure/`),
not a pass inferred from these successful recovery tests.

### Guest service crash acceptance update (2026-10-09)

The previously recorded failed framing run remains failed.
A fresh actual run (`vm-archive/docs-evidence/installed-qemu-guest-crash-live/`), with
watcher framing fix `dcf4c7c` and timestamped pre-cleanup observations `64ba8ad`,
now passes9checks on unchanged frozen host bin-v5. Acknowledged rootless work and
an actively writing detached descendant precede pidfd SIGKILL of the exact Guest
service.26 host observations establish retirement of the original QEMU/guardian/
worker with two absence samples by1.677s from arm-request start; the final
assertion is1.679s. The original MCP returns cleanup failure and new execution
is refused. This establishes host incarnation fail-stop for this service-crash
sample, **not Guest cgroup-empty acknowledgement**, graceful shutdown or automatic
durable reconciliation. The imported base hash is unchanged; no power-loss,
host-sleep, universal latency or queued-byte replay guarantee is inferred.

### Same-daemon normal exit and restart acceptance (2026-10-09)

The earlier frozen-v6 requirement for explicit reconciliation after clean normal
exit is superseded for the tested **same live daemon, exact owned worker exit0**
path. Actual frozen v7r3 / dcefab5 evidence (`vm-archive/docs-evidence/installed-qemu-normal-restart-live/`)
passes20 checks: two installed boots, independent15-pixel/command/PNG checks,
old owner/job/epoch denial, two stopped/exit0 process retirements and automatic
exact completed receipts. The next boot requires no manual reconciliation.
Shutdown status, journal and process observations are preserved before assertions;
the previous v7 failure remains an independent failure, not retroactively passed.
This is still the original-incarnation lifetime-witness proof, not a synthetic
Guest scope-empty ACK. Recovered prior-daemon or nonzero/crashed incarnations are
not accepted by this normal-path result; crash reconciliation, persistence faults
and any later bounded-retention policy retain their separate evidence gates.

### Bounded v3 completion receipts and actual normal-path GC (2026-10-09)

Controller9a72e11 uses strict snapshot v3: pending launches and scopes each have
independent256-entry admission bounds, while completed receipts retain the latest
64 durable completions in global FIFO order. Pending obligations are never
retention-evicted. Only an exact already-completed witness basename may be safely
collected; a retained receipt remains idempotent after collection. Unknown or
expired receipts fail with cleanup unconfirmed and cannot clear a newer instance.
Retention is count-based, not a promised wall-clock TTL. Valid v2 migration uses
its stored serialized receipt order, not reconstructed historical time; v1 stays
conservatively fenced. See component semantics and fault tests (`vm-archive/docs-evidence/qemu-completion-retention/`).

Actual explicit-v3 normal acceptance (`vm-archive/docs-evidence/installed-qemu-retention-normal-live/`)
now passes22checks: two installed Guest boots, old-authority refusal, real commands
and captures, two normal exit0 automatic receipts, and each exact completed
witness file absent afterward. Both process chains are gone and the source base
hash is unchanged. This establishes the two-entry normal path and metadata GC;
it does not establish actual65th-entry eviction, migration, unresolved-peer
retention, crash auto-recovery, host sleep or physical power-loss behavior.

### Same-daemon peer isolation with a retained pending obligation (2026-10-09)

Actual frozen v8 peer acceptance (`vm-archive/docs-evidence/installed-qemu-peer-isolation-live/`)
passes12checks: two installed Linux VMs share one daemon; real Guest-service
SIGKILL retires B's original host incarnation, while A's existing rootless job
continues and fresh execution/capture still works on A's original instance.
A then cleans its scope and normally completes, with its completed witness
collected. B's exact scope, pending launch and witness remain unchanged, leaving
one scope/one pending launch/one completed receipt in v3. No reconciliation or
fabricated B cgroup-empty ACK occurs. This verifies a completed peer's GC does
not erase that pending obligation in this actual run, not arbitrary multi-VM
coverage, receipt65 eviction, three-OS concurrency or later B recovery.
